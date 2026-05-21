use std::collections::HashSet;
use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::{Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::media::Media;
use grammers_client::message::Message as TgMessage;
use grammers_client::tl;
use grammers_client::update::Update;
use grammers_client::{Client as TgClient, SenderPool, SignInError};
use matrix_sdk::attachment::AttachmentConfig;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use matrix_sdk::{
    Client as MatrixClient, Room, RoomState, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::verification::{SasState, Verification, VerificationRequest, VerificationRequestState},
    ruma::{
        OwnedDeviceId, OwnedServerName, OwnedUserId, RoomOrAliasId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use matrix_sdk_base::crypto::CollectStrategy;
use serde::Deserialize;
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

#[derive(Deserialize)]
struct MatrixConfig {
    homeserver: String,
    user_id: String,
    access_token: String,
    device_id: String,
    recovery_key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum EncryptionStrategy {
    AllDevices,
    #[default]
    IdentityBased,
    OnlyTrusted,
}

impl From<EncryptionStrategy> for CollectStrategy {
    fn from(s: EncryptionStrategy) -> Self {
        match s {
            EncryptionStrategy::AllDevices => CollectStrategy::AllDevices,
            EncryptionStrategy::IdentityBased => CollectStrategy::IdentityBasedStrategy,
            EncryptionStrategy::OnlyTrusted => CollectStrategy::OnlyTrustedDevices,
        }
    }
}

// Serde helper: deserializes either the string "all" or a list of strings.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum RawAllowList {
    Wildcard(String),
    List(Vec<String>),
}
impl Default for RawAllowList {
    fn default() -> Self {
        RawAllowList::Wildcard("all".to_owned())
    }
}

#[derive(Debug, Clone)]
enum UserAllowList {
    All,
    Deny,
    Explicit(HashSet<OwnedUserId>),
}
impl UserAllowList {
    fn allows(&self, user: &OwnedUserId) -> bool {
        match self {
            Self::All => true,
            Self::Deny => false,
            Self::Explicit(set) => set.contains(user),
        }
    }
    fn is_allow_all(&self) -> bool { matches!(self, Self::All) }
    fn is_deny_all(&self) -> bool { matches!(self, Self::Deny) }
    fn explicit_count(&self) -> Option<usize> {
        if let Self::Explicit(set) = self { Some(set.len()) } else { None }
    }
}

#[derive(Debug, Clone)]
enum RoomAllowList {
    All,
    Deny,
    Explicit(HashSet<matrix_sdk::ruma::OwnedRoomId>),
}
impl RoomAllowList {
    fn allows(&self, room_id: &matrix_sdk::ruma::RoomId) -> bool {
        match self {
            Self::All => true,
            Self::Deny => false,
            Self::Explicit(set) => set.contains(room_id),
        }
    }
    fn is_allow_all(&self) -> bool { matches!(self, Self::All) }
    fn is_deny_all(&self) -> bool { matches!(self, Self::Deny) }
    fn explicit_count(&self) -> Option<usize> {
        if let Self::Explicit(set) = self { Some(set.len()) } else { None }
    }
}

#[derive(Deserialize, Default)]
struct SecurityConfig {
    /// "all" = accept invites from any user; [] = reject all invites; explicit list = allowlist.
    #[serde(default)]
    allowed_inviters: RawAllowList,
    /// "all" = operate in any room; [] = operate in no room; explicit list = allowlist.
    #[serde(default)]
    allowed_rooms: RawAllowList,
    #[serde(default)]
    admin_users: Vec<String>,
    #[serde(default)]
    encryption_strategy: EncryptionStrategy,
}

// ── Bot state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BotState {
    bot_user_id: OwnedUserId,
    allowed_inviters: UserAllowList,
    allowed_rooms: RoomAllowList,
    admin_users: HashSet<OwnedUserId>,
    reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>>,
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
        _ => return "",
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
                events.push(TagEvent { pos: o, is_open: true, tag: "<strong>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</strong>".into() });
            }
            tl::enums::MessageEntity::Italic(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: "<em>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</em>".into() });
            }
            tl::enums::MessageEntity::Code(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: "<code>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</code>".into() });
            }
            tl::enums::MessageEntity::Pre(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: "<pre><code>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</code></pre>".into() });
            }
            tl::enums::MessageEntity::Strike(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: "<del>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</del>".into() });
            }
            tl::enums::MessageEntity::Underline(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: "<u>".into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</u>".into() });
            }
            tl::enums::MessageEntity::Spoiler(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent { pos: o, is_open: true, tag: r#"<span data-mx-spoiler="">"#.into() });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</span>".into() });
            }
            tl::enums::MessageEntity::TextUrl(e) => {
                let (o, l) = (e.offset as usize, e.length as usize);
                events.push(TagEvent {
                    pos: o,
                    is_open: true,
                    tag: format!("<a href=\"{}\">", html_escape(&e.url)),
                });
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</a>".into() });
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
                events.push(TagEvent { pos: o + l, is_open: false, tag: "</a>".into() });
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
                if esc.is_empty() { result.push(c); } else { result.push_str(esc); }
            }
            i += 2;
        } else {
            if let Some(c) = char::from_u32(unit as u32) {
                if c == '\n' {
                    result.push_str("<br>");
                } else {
                    let esc = html_escape_char(c);
                    if esc.is_empty() { result.push(c); } else { result.push_str(esc); }
                }
            }
            i += 1;
        }
    }

    result
}

/// Build (plain_text, html) from a Telegram message.
/// Returns None if the message has no text content.
fn format_message(text: &str, entities: Option<&Vec<tl::enums::MessageEntity>>) -> Option<(String, String)> {
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

// ── Last-ID persistence ───────────────────────────────────────────────────────

async fn read_last_id(path: &std::path::Path) -> i32 {
    fs::read_to_string(path).await
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
    matrix: &MatrixClient,
    last_id: i32,
    history_limit: usize,
    last_id_path: &std::path::Path,
) -> Result<()> {
    let is_first_run = last_id == 0;

    if is_first_run && history_limit == 0 {
        return Ok(());
    }

    let peer_ref = channel_peer.to_ref().await
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

async fn send_text_to_rooms(matrix: &MatrixClient, plain: &str, html: &str) {
    let rooms = matrix.joined_rooms();
    if rooms.is_empty() {
        warn!("No joined Matrix rooms — message dropped");
        return;
    }
    for room in rooms {
        let content = RoomMessageEventContent::text_html(plain, html);
        if let Err(e) = room.send(content).await {
            error!("Failed to send to {}: {e}", room.room_id());
        }
    }
}

/// Download a Telegram media item and send it to all joined Matrix rooms.
async fn send_media_to_rooms(tg: &TgClient, matrix: &MatrixClient, media: &Media) {
    let (mime_str, filename) = match media {
        Media::Photo(_) => ("image/jpeg".to_owned(), "photo.jpg".to_owned()),
        Media::Document(doc) => {
            let m = doc.mime_type().unwrap_or("application/octet-stream").to_owned();
            let n = doc.name().unwrap_or("file").to_owned();
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
            warn!("Invalid MIME type '{mime_str}': {e}");
            return;
        }
    };

    // Download to memory
    let mut iter = tg.iter_download(media);
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        match iter.next().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(e) => {
                warn!("Failed to download media: {e}");
                return;
            }
        }
    }
    if bytes.is_empty() {
        return;
    }

    let rooms = matrix.joined_rooms();
    if rooms.is_empty() {
        warn!("No joined Matrix rooms — media dropped");
        return;
    }
    for room in rooms {
        if let Err(e) = room
            .send_attachment(&filename, &mime, bytes.clone(), AttachmentConfig::new())
            .await
        {
            error!("Failed to send media to {}: {e}", room.room_id());
        }
    }
}

/// Forward one Telegram message (text and/or media) to all Matrix rooms.
async fn forward_message(tg: &TgClient, matrix: &MatrixClient, msg: &TgMessage) {
    // Send media first (mirrors Telegram's layout: image above caption)
    if let Some(media) = msg.media() {
        send_media_to_rooms(tg, matrix, &media).await;
    }
    // Send text / caption
    let text = msg.text();
    if !text.is_empty() {
        if let Some((plain, html)) = format_message(text, msg.fmt_entities()) {
            send_text_to_rooms(matrix, &plain, &html).await;
        }
    }
}

// ── Verification (identical pattern to other bots) ────────────────────────────

async fn handle_verification_request(
    client: MatrixClient,
    state: BotState,
    request: VerificationRequest,
) {
    let user_id = request.other_user_id();

    let already_verified = client
        .encryption()
        .get_user_devices(user_id)
        .await
        .map(|devices| devices.devices().any(|d| d.is_verified()))
        .unwrap_or(false);

    if already_verified {
        let allowed = state.reset_allowed.lock().await.remove(user_id);
        if !allowed {
            warn!("Rejecting verification from {} — already has a verified device", user_id);
            request.cancel().await.ok();
            return;
        }
        info!("Allowing re-verification for {} (trust was reset by admin)", user_id);
    }

    info!("Accepting verification from {user_id}");
    if let Err(e) = request.accept().await {
        error!("Failed to accept verification: {e}");
        return;
    }

    let mut stream = request.changes();
    while let Some(state) = futures_util::StreamExt::next(&mut stream).await {
        match state {
            VerificationRequestState::Transitioned { verification } => {
                if let Verification::SasV1(sas) = verification {
                    tokio::spawn(handle_sas(sas));
                    break;
                }
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => break,
            _ => {}
        }
    }
}

async fn handle_sas(sas: matrix_sdk::encryption::verification::SasVerification) {
    if let Err(e) = sas.accept().await {
        error!("SAS accept failed: {e}");
        return;
    }
    let mut stream = sas.changes();
    while let Some(state) = futures_util::StreamExt::next(&mut stream).await {
        match state {
            SasState::KeysExchanged { emojis, .. } => {
                if let Some(e) = emojis {
                    info!("SAS emojis: {:?}", e.emojis.iter().map(|em| em.description).collect::<Vec<_>>());
                }
                if let Err(e) = sas.confirm().await {
                    error!("SAS confirm failed: {e}");
                }
            }
            SasState::Done { .. } => {
                info!("SAS verification complete");
                break;
            }
            SasState::Cancelled(info) => {
                warn!("SAS cancelled: {:?}", info.cancel_code());
                break;
            }
            _ => {}
        }
    }
}

fn is_join_terminal(e: &matrix_sdk::Error) -> bool {
    let s = e.to_string();
    s.contains("No known servers")
        || s.contains("M_FORBIDDEN")
        || s.contains("M_UNKNOWN_TOKEN")
        || s.contains("M_GUEST_ACCESS_FORBIDDEN")
}

async fn bootstrap_cross_signing(client: &MatrixClient, user_id: &OwnedUserId) {
    if let Some(status) = client.encryption().cross_signing_status().await {
        if status.has_master && status.has_self_signing && status.has_user_signing {
            info!("Cross-signing already complete (keys present) — skipping bootstrap");
            return;
        }
    }
    match client.encryption().bootstrap_cross_signing(None).await {
        Ok(()) => info!("Cross-signing bootstrapped for {user_id}"),
        Err(e) => warn!("Cross-signing bootstrap failed: {e}"),
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "telegram_mirror_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".to_owned());
    let config_str = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {config_path}"))?;
    let config: Config = toml::from_str(&config_str)?;

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
    fs::create_dir_all(&store_path).await?;

    // ── Telegram client ───────────────────────────────────────────────────────

    let session_path = store_path.join("telegram.session");
    let session = Arc::new(
        SqliteSession::open(&session_path)
            .await
            .with_context(|| format!("Failed to open Telegram session at {session_path:?}"))?,
    );

    let SenderPool { runner, updates, handle } =
        SenderPool::new(Arc::clone(&session), config.telegram.api_id);
    let tg = TgClient::new(handle.clone());
    tokio::spawn(runner.run());

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
                tg.check_password(hint, &pw).await.context("check_password failed")?;
                info!("Signed in with 2FA");
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        info!("Already signed in");
    }

    // Resolve the configured channel to a PeerId for filtering updates
    let channel_peer = tg
        .resolve_username(&config.telegram.channel)
        .await
        .context("resolve_username failed")?
        .with_context(|| format!("Channel '{}' not found", config.telegram.channel))?;

    let channel_peer_id: PeerId = match &channel_peer {
        grammers_client::peer::Peer::Channel(ch) => ch.id(),
        grammers_client::peer::Peer::Group(g) => g.id(),
        grammers_client::peer::Peer::User(u) => u.id(),
    };
    info!("Mirroring channel with peer id: {channel_peer_id:?}");

    let last_id_path = store_path.join("last_message_id");

    // ── Matrix client ─────────────────────────────────────────────────────────

    let strategy: CollectStrategy = config.security.encryption_strategy.into();
    let matrix = MatrixClient::builder()
        .homeserver_url(&config.matrix.homeserver)
        .sqlite_store(&store_path, None)
        .with_room_key_recipient_strategy(strategy)
        .build()
        .await?;

    let user_id: OwnedUserId = config.matrix.user_id.parse()?;
    let device_id: OwnedDeviceId = OwnedDeviceId::from(config.matrix.device_id);

    matrix
        .restore_session(MatrixSession {
            meta: SessionMeta { user_id: user_id.clone(), device_id },
            tokens: SessionTokens { access_token: config.matrix.access_token, refresh_token: None },
        })
        .await?;
    info!("Matrix session restored as {user_id}");

    if let Some(ref key) = config.matrix.recovery_key {
        match matrix.encryption().recovery().recover(key).await {
            Ok(()) => info!("Cross-signing keys recovered"),
            Err(e) => warn!("Recovery failed: {e}"),
        }
    }
    bootstrap_cross_signing(&matrix, &user_id).await;

    let allowed_inviters: UserAllowList = match &config.security.allowed_inviters {
        RawAllowList::Wildcard(s) if s == "all" => UserAllowList::All,
        RawAllowList::Wildcard(s) => anyhow::bail!("Invalid allowed_inviters value: {:?} (expected \"all\" or a list)", s),
        RawAllowList::List(list) if list.is_empty() => UserAllowList::Deny,
        RawAllowList::List(list) => {
            let mut set = HashSet::new();
            for s in list {
                let uid = s.parse::<OwnedUserId>()
                    .with_context(|| format!("Invalid Matrix user ID in allowed_inviters: {:?}", s))?;
                set.insert(uid);
            }
            UserAllowList::Explicit(set)
        }
    };

    let allowed_rooms: RoomAllowList = match &config.security.allowed_rooms {
        RawAllowList::Wildcard(s) if s == "all" => RoomAllowList::All,
        RawAllowList::Wildcard(s) => anyhow::bail!("Invalid allowed_rooms value: {:?} (expected \"all\" or a list)", s),
        RawAllowList::List(list) if list.is_empty() => RoomAllowList::Deny,
        RawAllowList::List(list) => {
            let mut set = HashSet::new();
            for s in list {
                let rid = s.parse::<matrix_sdk::ruma::OwnedRoomId>()
                    .with_context(|| format!("Invalid Matrix room ID in allowed_rooms: {:?}", s))?;
                set.insert(rid);
            }
            RoomAllowList::Explicit(set)
        }
    };

    if allowed_inviters.is_deny_all() {
        warn!("allowed_inviters = [] — bot will reject all invites");
    } else if allowed_inviters.is_allow_all() {
        warn!("allowed_inviters = \"all\" — bot will accept invites from any Matrix user");
    } else {
        info!("Allowed inviters configured (explicit list, {} user(s))", allowed_inviters.explicit_count().unwrap_or(0));
    }
    if allowed_rooms.is_deny_all() {
        warn!("allowed_rooms = [] — bot will not operate in any room");
    } else if allowed_rooms.is_allow_all() {
        info!("allowed_rooms = \"all\" — bot will operate in any joined room");
    } else {
        info!("Allowed rooms configured (explicit list, {} room(s))", allowed_rooms.explicit_count().unwrap_or(0));
    }

    let admin_users: HashSet<OwnedUserId> = config
        .security
        .admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    if admin_users.is_empty() {
        warn!("No admin_users configured — !reset-trust command is disabled");
    } else {
        info!("Admin users: {admin_users:?}");
    }

    let bot_state = BotState {
        bot_user_id: user_id,
        allowed_inviters: allowed_inviters.clone(),
        allowed_rooms: allowed_rooms.clone(),
        admin_users,
        reset_allowed: Arc::new(Mutex::new(HashSet::new())),
    };

    // Invite handler
    matrix.add_event_handler({
        let state = bot_state.clone();
        move |ev: StrippedRoomMemberEvent, room: Room, client: MatrixClient| {
            let state = state.clone();
            async move {
                if ev.state_key != state.bot_user_id {
                    return;
                }
                if !state.allowed_inviters.allows(&ev.sender) {
                    warn!("Rejecting invite from {}: inviter not in allowed_inviters", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                if !state.allowed_rooms.allows(room.room_id()) {
                    warn!("Rejecting invite to {}: room not in allowed_rooms", room.room_id());
                    room.leave().await.ok();
                    return;
                }
                info!("Accepted invite from {} to {}", ev.sender, room.room_id());
                let room_id = room.room_id().to_owned();
                let mut via: Vec<OwnedServerName> = vec![ev.sender.server_name().to_owned()];
                if let Some(s) = room_id.server_name() {
                    let s = s.to_owned();
                    if !via.contains(&s) {
                        via.push(s);
                    }
                }
                let room_or_alias = match RoomOrAliasId::parse(room_id.as_str()) {
                    Ok(id) => id,
                    Err(e) => {
                        error!("Invalid room ID {room_id}: {e}");
                        return;
                    }
                };
                tokio::spawn(async move {
                    let mut delay = 2u64;
                    const MAX_ATTEMPTS: u32 = 8;
                    for attempt in 1..=MAX_ATTEMPTS {
                        match client.join_room_by_id_or_alias(&room_or_alias, &via).await {
                            Ok(_) => {
                                info!("Joined {room_id}");
                                return;
                            }
                            Err(ref e) if is_join_terminal(e) => {
                                warn!("Join failed (terminal) for {room_id}: {e}");
                                return;
                            }
                            Err(e) if attempt == MAX_ATTEMPTS => {
                                warn!("Join failed after {MAX_ATTEMPTS} attempts for {room_id}: {e}");
                            }
                            Err(e) => {
                                warn!("Join attempt {attempt}/{MAX_ATTEMPTS} failed for {room_id}: {e}; retry in {delay}s");
                                sleep(Duration::from_secs(delay)).await;
                                delay = (delay * 2).min(300);
                            }
                        }
                    }
                });
            }
        }
    });

    // To-device verification requests
    matrix.add_event_handler({
        let state = bot_state.clone();
        move |ev: ToDeviceKeyVerificationRequestEvent, client: MatrixClient| {
            let state = state.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                else {
                    warn!("Verification request object not found");
                    return;
                };
                tokio::spawn(handle_verification_request(client, state, request));
            }
        }
    });

    // In-room verification requests + !reset-trust command
    matrix.add_event_handler({
        let state = bot_state.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: MatrixClient| {
            let state = state.clone();
            async move {
                if ev.sender == state.bot_user_id || room.state() != RoomState::Joined {
                    return;
                }
                match &ev.content.msgtype {
                    MessageType::VerificationRequest(_) => {
                        let Some(request) = client
                            .encryption()
                            .get_verification_request(&ev.sender, &ev.event_id)
                            .await
                        else {
                            return;
                        };
                        tokio::spawn(handle_verification_request(client, state, request));
                    }
                    MessageType::Text(text) => {
                        let body = text.body.trim();
                        if let Some(rest) = body.strip_prefix("!reset-trust") {
                            let target = rest.trim();
                            if state.admin_users.contains(&ev.sender) {
                                if let Ok(target_user) = target.parse::<OwnedUserId>() {
                                    state.reset_allowed.lock().await.insert(target_user.clone());
                                    info!("Trust reset for {target_user} (by {})", ev.sender);
                                    let msg = RoomMessageEventContent::text_plain(
                                        format!("Trust reset for {target_user}. They may re-verify."),
                                    );
                                    room.send(msg).await.ok();
                                }
                            } else {
                                warn!("!reset-trust from non-admin {}", ev.sender);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Do an initial sync so the bot knows which rooms it has joined, then spawn continuous sync.
    info!("Performing initial Matrix sync...");
    {
        let filter = FilterDefinition::with_lazy_loading();
        matrix.sync_once(SyncSettings::default().filter(filter.into())).await?;
    }
    info!("Initial sync complete");

    // Drain pending invites from prior sessions.
    let invited = matrix.invited_rooms();
    if !invited.is_empty() {
        info!("Pending invite(s) found after initial sync — processing {} room(s)", invited.len());
        for room in invited {
            let room_id = room.room_id().to_owned();
            // Inviter info is unavailable when replaying from the store.
            if allowed_inviters.is_deny_all() {
                warn!("Pending invite to {room_id} declined: allowed_inviters = []");
                room.leave().await.ok();
                continue;
            }
            if !allowed_rooms.allows(&room_id) {
                warn!("Pending invite to {room_id} declined: room not in allowed_rooms");
                room.leave().await.ok();
                continue;
            }
            let via: Vec<OwnedServerName> = room_id
                .server_name()
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default();
            match RoomOrAliasId::parse(room_id.as_str()) {
                Ok(room_or_alias) => {
                    match matrix.join_room_by_id_or_alias(&room_or_alias, &via).await {
                        Ok(_) => info!("Joined pending invite room {room_id}"),
                        Err(e) => warn!("Failed to join pending invite room {room_id}: {e}"),
                    }
                }
                Err(e) => warn!("Invalid room ID in pending invite {room_id}: {e}"),
            }
        }
    }

    // ── Backfill history ──────────────────────────────────────────────────────

    let last_id = read_last_id(&last_id_path).await;
    backfill(
        &tg,
        &channel_peer,
        &matrix,
        last_id,
        config.telegram.history_limit,
        &last_id_path,
    )
    .await?;

    // ── Run Telegram update loop and Matrix sync concurrently ─────────────────

    let mut update_stream = tg
        .stream_updates(updates, UpdatesConfiguration { catch_up: false, ..Default::default() })
        .await;

    info!("Listening for Telegram updates from channel {:?}", config.telegram.channel);

    let matrix_for_sync = matrix.clone();
    let sync_fut = async move {
        let filter = FilterDefinition::with_lazy_loading();
        matrix_for_sync.sync(SyncSettings::default().filter(filter.into())).await
    };
    tokio::pin!(sync_fut);

    loop {
        tokio::select! {
            sync_result = &mut sync_fut => {
                if let Err(e) = sync_result {
                    error!("Matrix sync ended with error: {e}");
                }
                break;
            }
            update_result = update_stream.next() => {
                let update = update_result?;
                if let Update::NewMessage(msg) = update {
                    if msg.outgoing() {
                        continue;
                    }
                    if msg.peer_id() != channel_peer_id {
                        continue;
                    }
                    let msg_id = msg.id();
                    info!("Forwarding live message {msg_id}");
                    forward_message(&tg, &matrix, &msg).await;
                    write_last_id(&last_id_path, msg_id).await;
                }
            }
        }
    }

    Ok(())
}
