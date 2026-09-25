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

fn html_escape_char(c: char) -> &'static str {
    match c {
        '<' => "&lt;",
        '>' => "&gt;",
        '&' => "&amp;",
        '"' => "&quot;",
        _ => "",
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let esc = html_escape_char(c);
        if esc.is_empty() {
            out.push(c);
        } else {
            out.push_str(esc);
        }
    }
    out
}

/// Convert Telegram message text + entities to Matrix-compatible HTML.
/// Telegram entity offsets are UTF-16 code unit positions.
fn entities_to_html(text: &str, entities: &[tl::enums::MessageEntity]) -> String {
    let utf16: Vec<u16> = text.encode_utf16().collect();

    // Collect open/close tag events keyed by UTF-16 position.
    // At the same position: closes (is_open=false) sort before opens (is_open=true).
    struct TagEvent {
        pos: usize,
        is_open: bool,
        tag: String,
    }
    let mut events: Vec<TagEvent> = Vec::new();

    for entity in entities {
        match entity {
            tl::enums::MessageEntity::Bold(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<strong>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</strong>".into(),
                });
            }
            tl::enums::MessageEntity::Italic(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<em>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</em>".into(),
                });
            }
            tl::enums::MessageEntity::Code(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<code>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</code>".into(),
                });
            }
            tl::enums::MessageEntity::Pre(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<pre><code>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</code></pre>".into(),
                });
            }
            tl::enums::MessageEntity::Strike(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<del>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</del>".into(),
                });
            }
            tl::enums::MessageEntity::Underline(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: "<u>".into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</u>".into(),
                });
            }
            tl::enums::MessageEntity::Spoiler(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: r#"<span data-mx-spoiler="">"#.into(),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</span>".into(),
                });
            }
            tl::enums::MessageEntity::TextUrl(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: format!("<a href=\"{}\">", html_escape(&e.url)),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</a>".into(),
                });
            }
            tl::enums::MessageEntity::Url(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                let end = (o + l).min(utf16.len());
                let url = String::from_utf16_lossy(&utf16[o..end]).to_string();
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: format!("<a href=\"{}\">", html_escape(&url)),
                });
                events.push(TagEvent {
                    pos: o + l,
                    is_open: false,
                    tag: "</a>".into(),
                });
            }
            _ => {}
        }
    }

    // Closes (false=0) sort before opens (true=1) at the same position.
    events.sort_by_key(|e| (e.pos, e.is_open as usize));

    let mut result = String::new();
    let mut ev_idx = 0;
    let mut i = 0usize;

    while i <= utf16.len() {
        while ev_idx < events.len() && events[ev_idx].pos == i {
            result.push_str(&events[ev_idx].tag);
            ev_idx += 1;
        }
        if i >= utf16.len() {
            break;
        }
        let unit = utf16[i];
        if (0xD800..=0xDBFF).contains(&unit) && i + 1 < utf16.len() {
            // Surrogate pair
            let low = utf16[i + 1];
            let cp = 0x10000u32 + ((unit as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
            if let Some(c) = char::from_u32(cp) {
                let esc = html_escape_char(c);
                if esc.is_empty() {
                    result.push(c);
                } else {
                    result.push_str(esc);
                }
            }
            i += 2;
        } else {
            if let Some(c) = char::from_u32(unit as u32) {
                if c == '\n' {
                    result.push_str("<br>");
                } else {
                    let esc = html_escape_char(c);
                    if esc.is_empty() {
                        result.push(c);
                    } else {
                        result.push_str(esc);
                    }
                }
            }
            i += 1;
        }
    }

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
        _ => html_escape(text).replace('\n', "<br>"),
    };
    Some((plain, html))
}

// ── Reliability helpers ──────────────────────────────────────────────────────

/// Classifies a Telegram [`InvocationError`] as permanent (retrying is
/// pointless, e.g. the account was logged out or banned) vs transient
/// (network hiccup, temporary server issue — safe to retry indefinitely).
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

/// How often the heartbeat file's timestamp is refreshed. Must stay well
/// under the `HEALTHCHECK` staleness threshold in the Dockerfile.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Periodically touches a file so an external health check can tell "the
/// process is alive but the runtime is wedged" apart from "still running
/// normally". Errors are logged once but never fatal — a broken heartbeat
/// file should not take the bot down.
async fn heartbeat_loop(path: PathBuf) {
    loop {
        if let Err(e) = fs::write(&path, unix_now_secs().to_string()).await {
            warn!("Failed to write heartbeat file {path:?}: {e}");
        }
        sleep(HEARTBEAT_INTERVAL).await;
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (Re)builds a Telegram connection: a fresh [`SenderPool`], its client
/// handle, and the raw update receiver. The pool's driver task is spawned
/// and supervised here — if it ever exits (e.g. because of a panic deep in
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
            "Telegram connection pool task exited unexpectedly — Telegram updates will reconnect"
        );
    });
    (tg, updates)
}

// ── Last-ID persistence ───────────────────────────────────────────────────────

async fn read_last_id(path: &std::path::Path) -> i32 {
    fs::read_to_string(path)
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

async fn write_last_id(path: &std::path::Path, id: i32) {
    if let Err(e) = fs::write(path, id.to_string()).await {
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
        .context("Channel peer has no access hash — cannot fetch history")?;

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

    // Ensure we save the newest ID even if all messages were media-only
    if let Some(newest) = batch.last() {
        write_last_id(last_id_path, newest.id()).await;
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

async fn send_text_to_rooms(matrix: &Bot, plain: &str, html: &str, tg_msg_id: i32) {
    let rooms = matrix.broadcast_rooms();
    if rooms.is_empty() {
        warn!(
            tg_msg_id,
            "telegram->matrix: no joined Matrix rooms — message dropped"
        );
        return;
    }
    for room in rooms {
        let content = RoomMessageEventContent::text_html(plain, html);
        if let Err(e) = room.send(content).await {
            error!(
                room_id = %room.room_id(),
                tg_msg_id,
                direction = "telegram->matrix",
                op = "send_text",
                "Failed to send message: {e}"
            );
        }
    }
}

/// Download a Telegram media item and send it to all joined Matrix rooms.
async fn send_media_to_rooms(tg: &TgClient, matrix: &Bot, media: &Media, tg_msg_id: i32) {
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
        _ => return, // polls, contacts, geo, etc.
    };

    let mime: mime::Mime = match mime_str.parse() {
        Ok(m) => m,
        Err(e) => {
            warn!(tg_msg_id, "Invalid MIME type '{mime_str}': {e}");
            return;
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
                        "Media exceeds size limit — skipping attachment"
                    );
                    return;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                warn!(tg_msg_id, "Failed to download media: {e}");
                return;
            }
        }
    }
    if bytes.is_empty() {
        return;
    }

    // Extract metadata once before the per-room loop.
    let size = bytes.len();
    let is_image = mime.type_() == mime::IMAGE;
    let is_audio = mime.type_() == mime::AUDIO;
    let is_video = mime.type_() == mime::VIDEO;
    let (img_w, img_h) = if is_image {
        media_dimensions(&bytes)
    } else {
        (0, 0)
    };

    let rooms = matrix.broadcast_rooms();
    if rooms.is_empty() {
        warn!("No joined Matrix rooms — media dropped");
        return;
    }
    for room in rooms {
        // Rebuild AttachmentInfo per iteration (not Clone) from the primitives above.
        let uint_size = UInt::new(size as u64);
        let info = if is_image {
            AttachmentInfo::Image(BaseImageInfo {
                width: if img_w > 0 {
                    UInt::new(img_w as u64)
                } else {
                    None
                },
                height: if img_h > 0 {
                    UInt::new(img_h as u64)
                } else {
                    None
                },
                size: uint_size,
                ..Default::default()
            })
        } else if is_audio {
            AttachmentInfo::Audio(BaseAudioInfo {
                size: uint_size,
                ..Default::default()
            })
        } else if is_video {
            AttachmentInfo::Video(BaseVideoInfo {
                size: uint_size,
                ..Default::default()
            })
        } else {
            AttachmentInfo::File(BaseFileInfo { size: uint_size })
        };
        if let Err(e) = room
            .send_attachment(
                &filename,
                &mime,
                bytes.clone(),
                AttachmentConfig::new().info(info),
            )
            .await
        {
            error!(
                room_id = %room.room_id(),
                tg_msg_id,
                direction = "telegram->matrix",
                op = "send_media",
                "Failed to send media: {e}"
            );
        }
    }
}

/// Extract pixel dimensions from image bytes without full decode.
/// Supports JPEG, PNG, and WebP (Telegram's three image formats).
/// Returns (0, 0) on failure — callers treat 0 as "unknown".
fn media_dimensions(data: &[u8]) -> (u32, u32) {
    use std::io::Cursor;
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((0, 0))
}

/// Forward one Telegram message (text and/or media) to all Matrix rooms.
async fn forward_message(tg: &TgClient, matrix: &Bot, msg: &TgMessage) {
    let msg_id = msg.id();
    // Send media first (mirrors Telegram's layout: image above caption)
    if let Some(media) = msg.media() {
        send_media_to_rooms(tg, matrix, &media, msg_id).await;
    }
    // Send text / caption
    let text = msg.text();
    if !text.is_empty() {
        if let Some((plain, html)) = format_message(text, msg.fmt_entities()) {
            send_text_to_rooms(matrix, &plain, &html, msg_id).await;
        }
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

    // A liveness heartbeat for `docker healthcheck`: touched on a fixed timer by a task
    // independent of Matrix/Telegram traffic, so it keeps ticking through quiet periods but
    // stalls if the async runtime itself wedges (e.g. a deadlock) — the one failure mode that
    // "the process is still running" can't detect on its own.
    tokio::spawn(heartbeat_loop(store_path.join(".heartbeat")));

    // ── Telegram client ───────────────────────────────────────────────────────

    let session_path = store_path.join("telegram.session");
    let session = Arc::new(
        SqliteSession::open(&session_path)
            .await
            .with_context(|| format!("Failed to open Telegram session at {session_path:?}"))?,
    );

    let (tg, updates) = spawn_sender_pool(&session, config.telegram.api_id);

    if !tg.is_authorized().await? {
        info!("Not signed in — starting interactive login");
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
        async move { matrix.sync_forever().await }
    });

    tokio::select! {
        _ = shutdown_signal() => {
            info!("Received shutdown signal — exiting");
            Ok(())
        }
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

/// Waits for SIGTERM (Docker's normal stop signal) or SIGINT (Ctrl-C).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!("Failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
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
                    error!(chat = %channel, "Configured Telegram channel not found — giving up");
                    return Err(anyhow::anyhow!("Telegram channel '{channel}' not found"));
                }
                Err(InvocationError::Dropped) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, ?delay, "Telegram connection pool is gone while resolving channel — reconnecting");
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
                    warn!(chat = %channel, "Failed to resolve Telegram channel: {e} (transient) — retrying in {delay:?}");
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
        // A failure here is not fatal — live updates still start.
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
            warn!(chat = %channel, "Backfill failed: {e} — continuing with live updates only");
        }

        let mut update_stream = tg
            .stream_updates(
                updates,
                UpdatesConfiguration {
                    catch_up: false,
                    ..Default::default()
                },
            )
            .await;
        info!(chat = %channel, "Listening for Telegram updates");
        backoff.reset();

        loop {
            match update_stream.next().await {
                Ok(Update::NewMessage(msg)) => {
                    if msg.outgoing() || msg.peer_id() != channel_peer_id {
                        continue;
                    }
                    let msg_id = msg.id();
                    info!(
                        chat = %channel,
                        tg_msg_id = msg_id,
                        direction = "telegram->matrix",
                        "Forwarding message"
                    );
                    forward_message(&tg, &matrix, &msg).await;
                    write_last_id(&last_id_path, msg_id).await;
                    backoff.reset();
                }
                Ok(_) => {}
                Err(InvocationError::Dropped) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, ?delay, "Telegram connection pool is gone — reconnecting");
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
                        "Telegram rate limit (FLOOD_WAIT) — honoring retry_after"
                    );
                    sleep(Duration::from_secs(wait as u64)).await;
                }
                Err(e) if is_terminal_telegram_error(&e) => {
                    error!(chat = %channel, "Unrecoverable Telegram error, giving up: {e}");
                    return Err(e.into());
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    warn!(chat = %channel, "Telegram update stream error (transient): {e} — retrying in {delay:?}");
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
        let html = entities_to_html("<script>&\"", &[]);
        assert_eq!(html, "&lt;script&gt;&amp;&quot;");
    }

    #[test]
    fn format_message_returns_none_for_empty_text() {
        assert!(format_message("", None).is_none());
    }
}
