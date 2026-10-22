#![recursion_limit = "256"]
use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::media::Media;
use grammers_client::message::Message as TgMessage;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_client::update::Update;
use grammers_client::{Client as TgClient, InvocationError, SenderPool, SignInError};
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use grammers_session::updates::UpdatesLike;
use mxbot_common::{
    config::{MatrixConfig, SecurityConfig},
    format::html_escape,
    matrix_sdk::Error as MatrixError,
    matrix_sdk::{
        attachment::{
            AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo,
            BaseVideoInfo,
        },
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::room::message::{OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            UInt,
        },
        Room, RoomState,
    },
    retry::Backoff,
    send::{classify_error, send_with_retry},
    Bot,
};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::{fs, time::sleep, time::Duration};
use tracing::{error, info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Config {
    telegram: TelegramConfig,
    matrix: MatrixConfig,
    #[serde(default)]
    security: SecurityConfig,
}

#[derive(Deserialize)]
struct TelegramConfig {
    api_id: i32,
    api_hash: String,
    phone: String,
    channel: String,
    /// How many recent messages to send on the very first run (0 = none).
    #[serde(default = "default_history_limit")]
    history_limit: usize,
}

fn default_history_limit() -> usize {
    50
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn prompt(msg: &str) -> String {
    print!("{msg}");
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).unwrap();
    line.trim().to_owned()
}

/// Append `text` HTML-escaped, newlines as `<br>`.
fn push_text(out: &mut String, text: &str) {
    out.push_str(&html_escape(text).replace('\n', "<br>"));
}

/// The href for a URL Telegram found in plain text: `example.com/x` (no scheme) would be a
/// relative link in a Matrix client, so it gets `https://`.
fn link_target(url: &str) -> String {
    if url.contains("://") || url.starts_with("mailto:") || url.starts_with("tg:") {
        url.to_owned()
    } else {
        format!("https://{url}")
    }
}

/// Convert Telegram message text + entities to Matrix-compatible HTML.
/// Telegram entity offsets are UTF-16 code unit positions; they come from the network and
/// are clamped to the text (and to whole characters), so a bad entity never panics.
fn entities_to_html(text: &str, entities: &[tl::enums::MessageEntity]) -> String {
    use std::cmp::Reverse;
    use tl::enums::MessageEntity as E;

    let utf16: Vec<u16> = text.encode_utf16().collect();
    let len = utf16.len();
    let is_low_surrogate = |pos: usize| pos < len && (0xDC00..=0xDFFF).contains(&utf16[pos]);
    // A position inside a surrogate pair moves before the pair (start) or after it (end).
    let clamp = |raw: i64, is_start: bool| -> usize {
        let pos = raw.clamp(0, len as i64) as usize;
        match (is_low_surrogate(pos), is_start) {
            (true, true) => pos - 1,
            (true, false) => pos + 1,
            (false, _) => pos,
        }
    };

    struct Span {
        start: usize,
        end: usize,
        open: String,
        close: &'static str,
    }
    let mut spans: Vec<Span> = Vec::new();
    for entity in entities {
        let (offset, length, open, close): (i32, i32, String, &'static str) = match entity {
            E::Bold(e) => (e.offset, e.length, "<strong>".into(), "</strong>"),
            E::Italic(e) => (e.offset, e.length, "<em>".into(), "</em>"),
            E::Code(e) => (e.offset, e.length, "<code>".into(), "</code>"),
            E::Pre(e) => (e.offset, e.length, "<pre><code>".into(), "</code></pre>"),
            E::Strike(e) => (e.offset, e.length, "<del>".into(), "</del>"),
            E::Underline(e) => (e.offset, e.length, "<u>".into(), "</u>"),
            E::Spoiler(e) => (
                e.offset,
                e.length,
                r#"<span data-mx-spoiler="">"#.into(),
                "</span>",
            ),
            E::Blockquote(e) => (e.offset, e.length, "<blockquote>".into(), "</blockquote>"),
            E::TextUrl(e) => (
                e.offset,
                e.length,
                format!("<a href=\"{}\">", html_escape(&e.url)),
                "</a>",
            ),
            E::Url(e) => {
                let start = clamp(i64::from(e.offset), true);
                let end = clamp(i64::from(e.offset) + i64::from(e.length), false);
                let url = link_target(&String::from_utf16_lossy(&utf16[start..end.max(start)]));
                (
                    e.offset,
                    e.length,
                    format!("<a href=\"{}\">", html_escape(&url)),
                    "</a>",
                )
            }
            _ => continue,
        };
        let start = clamp(i64::from(offset), true);
        let end = clamp(i64::from(offset) + i64::from(length), false);
        if end > start {
            spans.push(Span {
                start,
                end,
                open,
                close,
            });
        }
    }

    // Tag events by position. At one position closes come before opens; nested spans close
    // inner first (the one opened later) and open outer first (the longer one).
    let mut events: Vec<(usize, u8, Reverse<usize>, i64, &str)> = Vec::new();
    for (idx, span) in spans.iter().enumerate() {
        events.push((span.start, 1, Reverse(span.end), idx as i64, &span.open));
        events.push((span.end, 0, Reverse(span.start), -(idx as i64), span.close));
    }
    events.sort_by_key(|&(pos, kind, order, idx, _)| (pos, kind, order, idx));

    let mut result = String::new();
    let mut pos = 0;
    for (at, _, _, _, tag) in events {
        if at > pos {
            push_text(&mut result, &String::from_utf16_lossy(&utf16[pos..at]));
            pos = at;
        }
        result.push_str(tag);
    }
    push_text(&mut result, &String::from_utf16_lossy(&utf16[pos..]));
    result
}

/// Build (plain_text, html) from a Telegram message.
/// Returns None if the message has no text content.
fn format_message(
    text: &str,
    entities: Option<&Vec<tl::enums::MessageEntity>>,
) -> Option<(String, String)> {
    if text.is_empty() {
        return None;
    }
    let plain = text.to_owned();
    let html = match entities {
        Some(ents) if !ents.is_empty() => entities_to_html(text, ents),
        _ => {
            let mut html = String::new();
            push_text(&mut html, text);
            html
        }
    };
    Some((plain, html))
}

// ── Reliability helpers ──────────────────────────────────────────────────────

/// Classifies a Telegram [`InvocationError`] as permanent (retrying is
/// pointless, e.g. the account was logged out or banned) vs transient
/// (network hiccup, temporary server issue: safe to retry indefinitely).
fn is_terminal_telegram_error(err: &InvocationError) -> bool {
    match err {
        InvocationError::Rpc(rpc) => matches!(
            rpc.name.as_str(),
            "AUTH_KEY_UNREGISTERED"
                | "AUTH_KEY_INVALID"
                | "AUTH_KEY_PERM_EMPTY"
                | "SESSION_REVOKED"
                | "SESSION_EXPIRED"
                | "USER_DEACTIVATED"
                | "USER_DEACTIVATED_BAN"
                | "PHONE_NUMBER_BANNED"
        ),
        _ => false,
    }
}

/// (Re)builds a Telegram connection: a fresh [`SenderPool`], its client
/// handle, and the raw update receiver. The pool's driver task is spawned
/// and supervised here. If it ever exits (e.g. because of a panic deep in
/// the transport layer), that is logged instead of silently vanishing, and
/// the closed `updates` channel is what lets [`InvocationError::Dropped`]
/// surface to callers so they know to reconnect.
fn spawn_sender_pool(
    session: &Arc<SqliteSession>,
    api_id: i32,
) -> (TgClient, mpsc::UnboundedReceiver<UpdatesLike>) {
    let SenderPool {
        runner,
        updates,
        handle,
    } = SenderPool::new(Arc::clone(session), api_id);
    let tg = TgClient::new(handle);
    tokio::spawn(async move {
        runner.run().await;
        error!(
            "Telegram connection pool task exited unexpectedly: Telegram updates will reconnect"
        );
    });
    (tg, updates)
}

// ── Last-ID persistence ───────────────────────────────────────────────────────

/// The last mirrored message ID; 0 when nothing was mirrored yet (no file). A file that
/// cannot be read or parsed is reported, since 0 means "first run" (history_limit backfill).
async fn read_last_id(path: &std::path::Path) -> i32 {
    match fs::read_to_string(path).await {
        Ok(text) => parse_last_id(&text).unwrap_or_else(|| {
            warn!(?path, content = %text.trim(), "Unreadable last_message_id: treating as first run");
            0
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => {
            warn!(?path, "Failed to read last_message_id: {e}: treating as first run");
            0
        }
    }
}

/// Channel message IDs only grow: one at or below the last mirrored ID was sent already.
fn is_new_message(msg_id: i32, last_forwarded: i32) -> bool {
    msg_id > last_forwarded
}

fn parse_last_id(text: &str) -> Option<i32> {
    text.trim().parse().ok().filter(|id| *id >= 0)
}

/// Persist the last mirrored ID atomically (temporary file plus rename): a crash mid-write
/// must not leave an empty file, which would read as a first run and repost the history.
async fn write_last_id(path: &std::path::Path, id: i32) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = async {
        fs::write(&tmp, id.to_string()).await?;
        fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = result {
        warn!("Failed to persist last_message_id: {e}");
    }
}

// ── History backfill ──────────────────────────────────────────────────────────

/// Fetch and forward messages that arrived since `last_id`.
/// On first run (last_id == 0) caps at `history_limit` messages.
/// Messages are fetched newest-first and sent oldest-first.
async fn backfill(
    tg: &TgClient,
    channel_peer: &grammers_client::peer::Peer,
    matrix: &Bot,
    last_id: i32,
    history_limit: usize,
    last_id_path: &std::path::Path,
) -> Result<()> {
    let is_first_run = last_id == 0;

    if is_first_run && history_limit == 0 {
        return Ok(());
    }

    let peer_ref = channel_peer
        .to_ref()
        .await
        .map_err(|e| anyhow::anyhow!("Could not look up the channel peer: {e}"))?
        .context("Channel peer has no access hash: cannot fetch history")?;

    info!(
        "Fetching history (last_id={last_id}, history_limit={history_limit}, first_run={is_first_run})"
    );

    let mut iter = tg.iter_messages(peer_ref);
    let mut batch: Vec<TgMessage> = Vec::new();

    while let Some(msg) = iter.next().await? {
        if msg.id() <= last_id {
            break; // already sent everything newer than this
        }
        batch.push(msg);
        if is_first_run && batch.len() >= history_limit {
            break;
        }
    }

    if batch.is_empty() {
        info!("No new history to backfill");
        return Ok(());
    }

    info!("Backfilling {} messages", batch.len());

    // Reverse: send oldest first
    batch.reverse();
    for msg in &batch {
        forward_message(tg, matrix, msg).await;
        write_last_id(last_id_path, msg.id()).await;
    }

    info!("Backfill complete");
    Ok(())
}

// ── Matrix helpers ────────────────────────────────────────────────────────────

/// Cap on how much of a single Telegram media item we'll buffer in memory
/// before giving up on it. Prevents one oversized attachment (or a runaway
/// stream) from ballooning the process's memory usage; most homeservers
/// reject uploads well below this anyway.
const MAX_MEDIA_BYTES: usize = 100 * 1024 * 1024;

/// Waits between delivery rounds for the parts of a message that still failed transiently
/// (each text send has its own short retries first): about eight minutes of homeserver
/// trouble are bridged before a message is given up. Telegram updates queue meanwhile.
const DELIVERY_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

/// A Telegram media item, downloaded once for all rooms.
struct PreparedMedia {
    filename: String,
    mime: mime::Mime,
    bytes: Vec<u8>,
    /// Pixel size of an image; 0 = unknown.
    width: u32,
    height: u32,
}

impl PreparedMedia {
    fn new(filename: String, mime: mime::Mime, bytes: Vec<u8>) -> Self {
        let (width, height) = if mime.type_() == mime::IMAGE {
            media_dimensions(&bytes)
        } else {
            (0, 0)
        };
        Self {
            filename,
            mime,
            bytes,
            width,
            height,
        }
    }

    /// The attachment info (not `Clone`, so built per send).
    fn info(&self) -> AttachmentInfo {
        let size = UInt::new(self.bytes.len() as u64);
        let known = |v: u32| (v > 0).then(|| UInt::from(v));
        match self.mime.type_() {
            mime::IMAGE => AttachmentInfo::Image(BaseImageInfo {
                width: known(self.width),
                height: known(self.height),
                size,
                ..Default::default()
            }),
            mime::AUDIO => AttachmentInfo::Audio(BaseAudioInfo {
                size,
                ..Default::default()
            }),
            mime::VIDEO => AttachmentInfo::Video(BaseVideoInfo {
                size,
                ..Default::default()
            }),
            _ => AttachmentInfo::File(BaseFileInfo { size }),
        }
    }
}

/// Download a Telegram media item; `None` for media without a file (polls, contacts, geo,
/// ...) and when the download fails or is too large (logged).
async fn download_media(tg: &TgClient, media: &Media, tg_msg_id: i32) -> Option<PreparedMedia> {
    let (mime_str, filename) = match media {
        Media::Photo(_) => ("image/jpeg".to_owned(), "photo.jpg".to_owned()),
        Media::Document(doc) => {
            let m = doc
                .mime_type()
                .unwrap_or("application/octet-stream")
                .to_owned();
            let n = doc
                .name()
                .filter(|s| !s.is_empty())
                .unwrap_or("file")
                .to_owned();
            (m, n)
        }
        Media::Sticker(s) => {
            let m = s.document.mime_type().unwrap_or("image/webp").to_owned();
            (m, "sticker.webp".to_owned())
        }
        _ => return None,
    };

    let mime: mime::Mime = match mime_str.parse() {
        Ok(m) => m,
        Err(e) => {
            warn!(
                tg_msg_id,
                "Invalid MIME type '{mime_str}': {e}, sending as a file"
            );
            mime::APPLICATION_OCTET_STREAM
        }
    };

    // Download to memory, bailing out if the item is larger than we're willing to buffer.
    let mut iter = tg.iter_download(media);
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        match iter.next().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > MAX_MEDIA_BYTES {
                    warn!(
                        tg_msg_id,
                        limit_bytes = MAX_MEDIA_BYTES,
                        "Media exceeds size limit: skipping attachment"
                    );
                    return None;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                warn!(tg_msg_id, "Failed to download media: {e}");
                return None;
            }
        }
    }
    (!bytes.is_empty()).then(|| PreparedMedia::new(filename, mime, bytes))
}

/// One Telegram message, ready to send: media (above, as in Telegram) and text / caption.
struct Outgoing {
    media: Option<PreparedMedia>,
    text: Option<RoomMessageEventContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Media,
    Text,
}

impl Outgoing {
    fn parts(&self) -> Vec<Part> {
        let mut parts = Vec::new();
        if self.media.is_some() {
            parts.push(Part::Media);
        }
        if self.text.is_some() {
            parts.push(Part::Text);
        }
        parts
    }

    async fn send(&self, room: &Room, part: Part) -> Result<(), MatrixError> {
        match (part, &self.media, &self.text) {
            (Part::Media, Some(media), _) => room
                .send_attachment(
                    &media.filename,
                    &media.mime,
                    media.bytes.clone(),
                    AttachmentConfig::new().info(media.info()),
                )
                .await
                .map(|_| ()),
            (Part::Text, _, Some(text)) => send_with_retry(room, text.clone()).await.map(|_| ()),
            _ => Ok(()),
        }
    }
}

/// Extract pixel dimensions from image bytes without full decode.
/// Supports JPEG, PNG, and WebP (Telegram's three image formats).
/// Returns (0, 0) on failure. Callers treat 0 as "unknown".
fn media_dimensions(data: &[u8]) -> (u32, u32) {
    use std::io::Cursor;
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((0, 0))
}

/// Forward one Telegram message (text and/or media) to all broadcast rooms. Parts that
/// failed transiently are sent again in later rounds ([`DELIVERY_RETRY_DELAYS`]), so a
/// homeserver restart loses nothing; a part retried late may arrive below a newer one.
async fn forward_message(tg: &TgClient, matrix: &Bot, msg: &TgMessage) {
    let msg_id = msg.id();
    let media = match msg.media() {
        Some(media) => download_media(tg, &media, msg_id).await,
        None => None,
    };
    let text = format_message(msg.text(), msg.fmt_entities())
        .map(|(plain, html)| RoomMessageEventContent::text_html(plain, html));
    let outgoing = Outgoing { media, text };
    let parts = outgoing.parts();
    if parts.is_empty() {
        return;
    }
    let rooms = matrix.broadcast_rooms();
    if rooms.is_empty() {
        warn!(
            tg_msg_id = msg_id,
            "telegram->matrix: no joined Matrix rooms, message dropped"
        );
        return;
    }
    let mut pending: Vec<(Room, Part)> = rooms
        .iter()
        .flat_map(|room| parts.iter().map(move |part| (room.clone(), *part)))
        .collect();
    let mut delays = DELIVERY_RETRY_DELAYS.iter();
    loop {
        let mut failed = Vec::new();
        for (room, part) in pending {
            if let Err(e) = outgoing.send(&room, part).await {
                let (retryable, _) = classify_error(&e);
                error!(
                    room_id = %room.room_id(),
                    tg_msg_id = msg_id,
                    direction = "telegram->matrix",
                    ?part,
                    retryable,
                    "Failed to send: {e}"
                );
                if retryable {
                    failed.push((room, part));
                }
            }
        }
        if failed.is_empty() {
            return;
        }
        let Some(delay) = delays.next() else {
            error!(
                tg_msg_id = msg_id,
                failed = failed.len(),
                "telegram->matrix: giving up, message parts lost in some rooms"
            );
            return;
        };
        warn!(
            tg_msg_id = msg_id,
            failed = failed.len(),
            "telegram->matrix: sending the failed parts again in {delay:?}"
        );
        sleep(*delay).await;
        pending = failed;
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("telegram_mirror_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;

    let store_path = mxbot_common::config::store_path_from_env();
    fs::create_dir_all(&store_path).await?;

    // Liveness: mxbot-common's heartbeat file (MXBOT_HEARTBEAT_FILE), refreshed by every
    // successful Matrix sync (sync_forever_with below), so a dead sync or a wedged runtime
    // both make the health check fail.

    // ── Telegram client ───────────────────────────────────────────────────────

    let session_path = store_path.join("telegram.session");
    let session = Arc::new(
        SqliteSession::open(&session_path)
            .await
            .with_context(|| format!("Failed to open Telegram session at {session_path:?}"))?,
    );

    let (tg, updates) = spawn_sender_pool(&session, config.telegram.api_id);

    if !tg.is_authorized().await? {
        info!("Not signed in: starting interactive login");
        let token = tg
            .request_login_code(&config.telegram.phone, &config.telegram.api_hash)
            .await
            .context("request_login_code failed")?;

        let code = prompt("Enter the Telegram login code: ");
        match tg.sign_in(&token, &code).await {
            Ok(_) => info!("Signed in"),
            Err(SignInError::PasswordRequired(hint)) => {
                let hint_str = hint.hint().unwrap_or("(no hint)");
                let pw = prompt(&format!("2FA password [{hint_str}]: "));
                tg.check_password(hint, &pw)
                    .await
                    .context("check_password failed")?;
                info!("Signed in with 2FA");
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        info!("Already signed in");
    }

    let last_id_path = store_path.join("last_message_id");

    // ── Matrix client ─────────────────────────────────────────────────────────

    let matrix = Bot::builder("telegram-mirror-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .start(&config.matrix, &config.security)
        .await?;

    // Room messages: only the shared admin console applies here.
    matrix.client.add_event_handler({
        let admin = matrix.admin.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let admin = admin.clone();
            async move {
                if room.state() == RoomState::Joined {
                    admin.handle(&room, &ev, encryption.as_ref()).await;
                }
            }
        }
    });

    // Do an initial sync so the bot knows which rooms it has joined (also
    // joins invites received while offline), then spawn continuous sync.
    info!("Performing initial Matrix sync...");
    matrix.initial_sync().await;
    info!("Initial sync complete");

    // ── Run Telegram update loop and Matrix sync concurrently ─────────────────
    //
    // Both sides are supervised: a transient failure on either one is retried
    // with backoff in place, and neither side's failure takes the other down.
    // The whole process only exits on a genuinely unrecoverable error, or on
    // SIGTERM/SIGINT for a clean shutdown.

    let matrix_sync_handle = tokio::spawn({
        let matrix = matrix.clone();
        async move { mxbot_common::sync::sync_forever_with(&matrix.client, || {}).await }
    });

    tokio::select! {
        () = mxbot_common::sync::shutdown_signal() => Ok(()),
        res = run_telegram_bridge(
            session,
            config.telegram.api_id,
            config.telegram.channel.clone(),
            config.telegram.history_limit,
            tg,
            updates,
            matrix,
            last_id_path,
        ) => {
            res.context("Telegram bridge task ended")
        }
        res = matrix_sync_handle => {
            match res {
                Ok(()) => Err(anyhow::anyhow!("Matrix sync task exited unexpectedly")),
                Err(e) => Err(anyhow::anyhow!("Matrix sync task panicked: {e}")),
            }
        }
    }
}

/// Drives Telegram update ingestion forever: resolves the target channel,
/// catches up on anything missed since `last_id`, then forwards live
/// messages. Reconnects (rebuilding the whole [`SenderPool`]) whenever the
/// connection is irrecoverably lost (`InvocationError::Dropped`), honors
/// Telegram's `FLOOD_WAIT` retry_after, and backs off with jitter for other
/// transient errors. Only returns on a genuinely permanent failure (unknown
/// channel, revoked/banned session).
#[allow(clippy::too_many_arguments)]
async fn run_telegram_bridge(
    session: Arc<SqliteSession>,
    api_id: i32,
    channel: String,
    history_limit: usize,
    mut tg: TgClient,
    mut updates: mpsc::UnboundedReceiver<UpdatesLike>,
    matrix: Bot,
    last_id_path: PathBuf,
) -> Result<()> {
    let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(120)).with_jitter();

    'reconnect: loop {
        let channel_peer = loop {
            match tg.resolve_username(&channel).await {
                Ok(Some(peer)) => break peer,
                Ok(None) => {
                    error!(chat = %channel, "Configured Telegram channel not found: giving up");
                    return Err(anyhow::anyhow!("Telegram channel '{channel}' not found"));
                }
                Err(InvocationError::Dropped) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, ?delay, "Telegram connection pool is gone while resolving channel: reconnecting");
                    sleep(delay).await;
                    let (new_tg, new_updates) = spawn_sender_pool(&session, api_id);
                    tg = new_tg;
                    updates = new_updates;
                }
                Err(e) if is_terminal_telegram_error(&e) => {
                    error!(chat = %channel, "Unrecoverable Telegram error while resolving channel: {e}");
                    return Err(e.into());
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, "Failed to resolve Telegram channel: {e} (transient), retrying in {delay:?}");
                    sleep(delay).await;
                }
            }
        };
        let channel_peer_id: PeerId = match &channel_peer {
            Peer::Channel(ch) => ch.id(),
            Peer::Group(g) => g.id(),
            Peer::User(u) => u.id(),
        };
        info!(chat = %channel, ?channel_peer_id, "Mirroring Telegram channel");
        backoff.reset();

        // Catch up on anything sent while we were disconnected (also covers the very first run).
        // A failure here is not fatal: live updates still start.
        let last_id = read_last_id(&last_id_path).await;
        if let Err(e) = backfill(
            &tg,
            &channel_peer,
            &matrix,
            last_id,
            history_limit,
            &last_id_path,
        )
        .await
        {
            warn!(chat = %channel, "Backfill failed: {e}, continuing with live updates only");
        }

        let mut update_stream = match tg
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: false,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                // The receiver is spent, so start over with a fresh pool.
                let delay = backoff.next_delay();
                warn!(chat = %channel, "Could not start the Telegram update stream: {e}, retrying in {delay:?}");
                sleep(delay).await;
                let (new_tg, new_updates) = spawn_sender_pool(&session, api_id);
                tg = new_tg;
                updates = new_updates;
                continue 'reconnect;
            }
        };
        let mut last_forwarded = read_last_id(&last_id_path).await;
        info!(chat = %channel, "Listening for Telegram updates");
        backoff.reset();

        loop {
            match update_stream.next().await {
                Ok(Update::NewMessage(msg)) => {
                    if msg.outgoing() || msg.peer_id() != channel_peer_id {
                        continue;
                    }
                    let msg_id = msg.id();
                    // Updates queued while the backfill ran may repeat what it sent.
                    if !is_new_message(msg_id, last_forwarded) {
                        info!(chat = %channel, tg_msg_id = msg_id, "Already mirrored, skipped");
                        continue;
                    }
                    info!(
                        chat = %channel,
                        tg_msg_id = msg_id,
                        direction = "telegram->matrix",
                        "Forwarding message"
                    );
                    forward_message(&tg, &matrix, &msg).await;
                    write_last_id(&last_id_path, msg_id).await;
                    last_forwarded = msg_id;
                    backoff.reset();
                }
                Ok(_) => {}
                Err(InvocationError::Dropped) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, ?delay, "Telegram connection pool is gone: reconnecting");
                    sleep(delay).await;
                    let (new_tg, new_updates) = spawn_sender_pool(&session, api_id);
                    tg = new_tg;
                    updates = new_updates;
                    continue 'reconnect;
                }
                Err(InvocationError::Rpc(rpc)) if rpc.name == "FLOOD_WAIT" => {
                    let wait = rpc.value.unwrap_or(5).clamp(1, 3600);
                    warn!(
                        chat = %channel,
                        retry_after_s = wait,
                        "Telegram rate limit (FLOOD_WAIT): honoring retry_after"
                    );
                    sleep(Duration::from_secs(wait as u64)).await;
                }
                Err(e) if is_terminal_telegram_error(&e) => {
                    error!(chat = %channel, "Unrecoverable Telegram error, giving up: {e}");
                    return Err(e.into());
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, "Telegram update stream error (transient): {e}, retrying in {delay:?}");
                    sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc_error(name: &str) -> InvocationError {
        InvocationError::Rpc(grammers_client::sender::RpcError {
            code: 400,
            name: name.to_owned(),
            value: None,
            caused_by: None,
        })
    }

    #[test]
    fn terminal_errors_are_classified_as_permanent() {
        for name in [
            "AUTH_KEY_UNREGISTERED",
            "AUTH_KEY_INVALID",
            "SESSION_REVOKED",
            "USER_DEACTIVATED",
            "USER_DEACTIVATED_BAN",
            "PHONE_NUMBER_BANNED",
        ] {
            assert!(
                is_terminal_telegram_error(&rpc_error(name)),
                "{name} should be terminal"
            );
        }
    }

    #[test]
    fn transient_errors_are_not_classified_as_permanent() {
        for name in ["FLOOD_WAIT", "TIMEOUT", "INTERNAL", "CONNECTION_NOT_INITED"] {
            assert!(
                !is_terminal_telegram_error(&rpc_error(name)),
                "{name} should not be terminal"
            );
        }
        assert!(!is_terminal_telegram_error(&InvocationError::Dropped));
        assert!(!is_terminal_telegram_error(&InvocationError::InvalidDc));
    }

    #[test]
    fn entities_to_html_handles_unicode_and_overlapping_tags() {
        // "héllo 😀!" with Bold covering the whole ASCII-adjacent word and the emoji,
        // exercising both the surrogate-pair path and plain escaping.
        let text = "h\u{e9}llo \u{1F600}!";
        let entities = vec![tl::enums::MessageEntity::Bold(
            tl::types::MessageEntityBold {
                offset: 0,
                length: "hello ".encode_utf16().count() as i32
                    + "\u{1F600}".encode_utf16().count() as i32,
            },
        )];
        let html = entities_to_html(text, &entities);
        assert!(html.starts_with("<strong>"));
        assert!(html.contains('\u{1F600}'));
        assert!(html.ends_with("</strong>!"));
    }

    #[test]
    fn entities_to_html_escapes_plain_text() {
        let html = entities_to_html("<script>&\"\n", &[]);
        assert_eq!(html, "&lt;script&gt;&amp;&quot;<br>");
    }

    fn bold(offset: i32, length: i32) -> tl::enums::MessageEntity {
        tl::enums::MessageEntity::Bold(tl::types::MessageEntityBold { offset, length })
    }

    fn italic(offset: i32, length: i32) -> tl::enums::MessageEntity {
        tl::enums::MessageEntity::Italic(tl::types::MessageEntityItalic { offset, length })
    }

    fn url(offset: i32, length: i32) -> tl::enums::MessageEntity {
        tl::enums::MessageEntity::Url(tl::types::MessageEntityUrl { offset, length })
    }

    #[test]
    fn entities_outside_the_text_do_not_panic_and_tags_stay_closed() {
        // Offsets from the network: beyond the end, negative, overflowing.
        let html = entities_to_html(
            "hi",
            &[
                url(5, 3),
                bold(-3, 4),
                italic(1, i32::MAX),
                url(i32::MAX, i32::MAX),
            ],
        );
        assert_eq!(html, "<strong>h</strong><em>i</em>");
        assert_eq!(html.matches("<a ").count(), 0, "empty links are dropped");
    }

    #[test]
    fn entity_boundaries_inside_a_surrogate_pair_keep_the_character_whole() {
        // "a😀b": the emoji is UTF-16 units 1..3; an entity cut at unit 2 widens to it.
        let html = entities_to_html("a\u{1F600}b", &[bold(2, 1), italic(0, 2)]);
        assert_eq!(html, "<em>a<strong>\u{1F600}</strong></em>b");
        assert!(!html.contains('\u{FFFD}'));
    }

    #[test]
    fn nested_entities_close_inner_first() {
        // Bold over all, italic over the end: both end at the same position.
        let html = entities_to_html("abcd", &[bold(0, 4), italic(2, 2)]);
        assert_eq!(html, "<strong>ab<em>cd</em></strong>");
        // Same start, the longer one opens first.
        let html = entities_to_html("abcd", &[italic(0, 2), bold(0, 4)]);
        assert_eq!(html, "<strong><em>ab</em>cd</strong>");
    }

    #[test]
    fn urls_and_text_are_escaped_for_attributes() {
        let text = "x\"><script>";
        let html = entities_to_html(text, &[url(0, text.encode_utf16().count() as i32)]);
        assert_eq!(
            html,
            "<a href=\"https://x&quot;&gt;&lt;script&gt;\">x&quot;&gt;&lt;script&gt;</a>"
        );
        let (_, html) = format_message("it's <b>\nnext", None).unwrap();
        assert_eq!(html, "it&#39;s &lt;b&gt;<br>next");
    }

    #[test]
    fn bare_urls_link_absolutely() {
        let html = entities_to_html("see example.com/a", &[url(4, 13)]);
        assert_eq!(
            html,
            "see <a href=\"https://example.com/a\">example.com/a</a>"
        );
        assert_eq!(link_target("http://x.y"), "http://x.y");
    }

    #[test]
    fn last_id_parsing_rejects_garbage() {
        assert_eq!(parse_last_id("42\n"), Some(42));
        assert_eq!(parse_last_id(""), None);
        assert_eq!(parse_last_id("-1"), None);
        assert_eq!(parse_last_id("4x"), None);
    }

    #[tokio::test]
    async fn last_id_is_written_atomically_and_read_back() {
        let dir = std::env::temp_dir().join(format!("tg-mirror-last-id-{}", std::process::id()));
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("last_message_id");
        assert_eq!(read_last_id(&path).await, 0);
        write_last_id(&path, 7).await;
        write_last_id(&path, 12).await;
        assert_eq!(read_last_id(&path).await, 12);
        assert!(!dir.join("last_message_id.tmp").exists());
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn already_mirrored_messages_are_not_new() {
        assert!(is_new_message(11, 10));
        assert!(!is_new_message(10, 10));
        assert!(!is_new_message(3, 10));
        assert!(is_new_message(1, 0));
    }

    #[test]
    fn media_info_matches_the_mime_type() {
        let png = PreparedMedia::new("a.png".into(), mime::IMAGE_PNG, vec![1, 2, 3]);
        match png.info() {
            AttachmentInfo::Image(info) => {
                assert_eq!(info.size, UInt::new(3));
                assert_eq!(info.width, None, "undecodable image: size unknown");
            }
            _ => panic!("expected image info"),
        }
        let pdf = PreparedMedia::new("a.pdf".into(), mime::APPLICATION_PDF, vec![0; 10]);
        assert!(matches!(pdf.info(), AttachmentInfo::File(_)));
    }

    #[test]
    fn rendered_house_infra_config_parses() {
        // A copy of house-infra's rendered config (fake secrets), as deployed.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/house-infra-config.toml");
        let config: Config = mxbot_common::config::load_toml(std::path::Path::new(path))
            .expect("the config house-infra renders");
        config.security.validate().unwrap();
        assert_eq!(config.telegram.history_limit, 50);
    }

    #[test]
    fn format_message_returns_none_for_empty_text() {
        assert!(format_message("", None).is_none());
    }
}
