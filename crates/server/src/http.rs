//! The HTTP side: discovery, capabilities, the editor page, the apps' web builds, the room
//! WebSockets and the small API the clients use (docs/design.md, "Endpoints").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json as JsonBody, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

use crate::config::{self, APPS};
use crate::discovery;
use crate::ids;
use crate::nextcloud;
use crate::proof::ProofKeys;
use crate::room::{Msg, OUT_QUEUE, Out, Registry};
use crate::session::{Kind, Role, Session, User};
use crate::shell;
use crate::wopi::WopiError;
use crate::ywire::Frame;

/// Everything the handlers share.
pub struct App {
    pub rooms: Registry,
    pub proof: Option<Arc<ProofKeys>>,
    next_conn: AtomicU64,
}

type St = State<Arc<App>>;

impl App {
    pub fn new(rooms: Registry, proof: Option<Arc<ProofKeys>>) -> App {
        App { rooms, proof, next_conn: AtomicU64::new(1) }
    }

    fn cfg(&self) -> &crate::config::Config {
        &self.rooms.ctx().cfg
    }
}

/// The router.
pub fn router(app: Arc<App>) -> Router {
    let upload_limit = usize::try_from(app.cfg().max_file()).unwrap_or(usize::MAX).saturating_add(1 << 20);
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/hosting/discovery", get(discovery_xml))
        .route("/hosting/capabilities", get(capabilities))
        .route("/hosting/wopiAccessCheck", post(wopi_access_check))
        .route("/edit/:app", post(editor).get(editor_get))
        .route("/apps/:app/*path", get(app_file))
        .route("/static/:file", get(static_file))
        .route("/ws/:room", get(ws))
        .route("/api/v1/sessions", post(desktop_session))
        .route("/api/v1/session", get(session_info))
        .route("/api/v1/prepare", post(prepare_again))
        .route("/api/v1/save", post(save_now))
        .route("/api/v1/rooms/:room/file", get(download).put(upload).layer(DefaultBodyLimit::max(upload_limit)))
        .route("/api/v1/rooms/:room/status", get(room_status))
        .layer(DefaultBodyLimit::max(4 << 20))
        .with_state(app)
}

// ----- Small helpers -----

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body.into()).into_response()
}

fn err_json(status: u16, message: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, JsonBody(json!({"error": message}))).into_response()
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The session token: `Authorization: Bearer …`, else `?token=`.
fn bearer(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    let h = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|t| t.trim().to_string());
    h.or_else(|| query.get("token").cloned()).filter(|t| !t.is_empty())
}

#[allow(clippy::result_large_err)]
fn session_from(app: &App, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<Arc<Session>, Response> {
    let token = bearer(headers, query).ok_or_else(|| err_json(401, "no session token"))?;
    app.rooms.ctx().sessions.get(&token).ok_or_else(|| err_json(401, "the session expired or doesn't exist"))
}

/// The origin (`scheme://host[:port]`) of a URL.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

/// `https://` → `wss://`, `http://` → `ws://`.
fn ws_base(public: &str) -> String {
    if let Some(r) = public.strip_prefix("https://") { format!("wss://{r}") } else { format!("ws://{}", public.strip_prefix("http://").unwrap_or(public)) }
}

// ----- Discovery -----

async fn index(State(app): St) -> Response {
    let apps: Vec<&str> = APPS.iter().copied().filter(|a| app.cfg().apps.contains_key(*a)).collect();
    text(
        StatusCode::OK,
        format!(
            "craft-live {}: a WOPI editor for Nextcloud Office with the craft apps ({}).\nDiscovery: {}/hosting/discovery\n",
            env!("CARGO_PKG_VERSION"),
            if apps.is_empty() { "none configured".to_string() } else { apps.join(", ") },
            app.cfg().public_url
        ),
    )
}

async fn health(State(app): St) -> Response {
    JsonBody(json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "rooms": app.rooms.open_rooms(), "sessions": app.rooms.ctx().sessions.len()}))
        .into_response()
}

async fn discovery_xml(State(app): St) -> Response {
    let xml = discovery::discovery(app.cfg(), app.proof.as_deref());
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/xml; charset=utf-8"), (header::CACHE_CONTROL, "max-age=3600")], xml).into_response()
}

async fn capabilities(State(app): St) -> Response {
    JsonBody(discovery::capabilities(app.cfg(), &app.rooms.ctx().server_id)).into_response()
}

/// Nextcloud's connectivity check: can craft-live reach the Nextcloud at `callbackUrl`? Only an
/// allowed host is probed (anything else would let anyone make craft-live fetch any address).
async fn wopi_access_check(State(app): St, body: Bytes) -> Response {
    let url = serde_json::from_slice::<Json>(&body).ok().and_then(|j| j.get("callbackUrl").and_then(Json::as_str).map(str::to_string)).unwrap_or_default();
    let allowed = config::normalize_base(&url).ok().and_then(|b| app.cfg().hosts().into_iter().find(|h| b.starts_with(&h.base)).map(|h| h.base));
    let Some(base) = allowed else {
        return JsonBody(json!({"status": "NotHttpSuccess", "details": "this Nextcloud isn't in craft-live's wopi_hosts"})).into_response();
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(!app.cfg().verify_tls)
        .redirect(reqwest::redirect::Policy::none())
        .build();
    let Ok(client) = client else { return JsonBody(json!({"status": "NotHttpSuccess", "details": "no HTTP client"})).into_response() };
    let answer = match client.get(format!("{base}/status.php")).send().await {
        Ok(r) if r.status().is_success() => json!({"status": "Ok", "details": ""}),
        Ok(r) => json!({"status": "NotHttpSuccess", "details": format!("{base}/status.php answered {}", r.status().as_u16())}),
        Err(e) if e.is_connect() && format!("{e:?}").to_ascii_lowercase().contains("certificate") => {
            json!({"status": "SslHandshake", "details": e.to_string()})
        }
        Err(e) => json!({"status": "NotHttpSuccess", "details": e.to_string()}),
    };
    JsonBody(answer).into_response()
}

// ----- The editor page -----

/// The fields Nextcloud posts: `access_token`, `access_token_ttl` (also `access_token_TTL`),
/// and others the page ignores (`ui_defaults`, `css_variables`…).
async fn form_fields(req: Request) -> Result<HashMap<String, String>, String> {
    let ctype = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mut out = HashMap::new();
    if ctype.starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, &()).await.map_err(|e| e.to_string())?;
        let mut n = 0;
        while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
            n += 1;
            if n > 64 {
                break;
            }
            let name = field.name().unwrap_or("").to_string();
            let value = field.text().await.map_err(|e| e.to_string())?;
            if value.len() <= 64 << 10 {
                out.insert(name, value);
            }
        }
    } else {
        let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.map_err(|e| e.to_string())?;
        for pair in String::from_utf8_lossy(&body).split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if let (Some(k), Some(v)) = (ids::unpct(k, true), ids::unpct(v, true)) {
                out.insert(k, v);
            }
        }
    }
    Ok(out)
}

fn page(status: StatusCode, html: String, frame_ancestors: &str) -> Response {
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&format!("frame-ancestors 'self' {frame_ancestors}")) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

/// The allowed Nextcloud origins, for `frame-ancestors`.
fn ancestors(app: &App) -> String {
    let mut o: Vec<String> = app.cfg().hosts().iter().filter_map(|h| origin_of(&h.base)).collect();
    o.dedup();
    o.join(" ")
}

async fn editor_get(State(app): St) -> Response {
    page(
        StatusCode::METHOD_NOT_ALLOWED,
        shell::problem_page("Open documents from Nextcloud Office: this page needs the access token Nextcloud sends.", ""),
        &ancestors(&app),
    )
}

async fn editor(State(app): St, UrlPath(app_id): UrlPath<String>, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    let fa = ancestors(&app);
    let problem = |status: StatusCode, msg: &str| page(status, shell::problem_page(msg, ""), &fa);
    if !APPS.contains(&app_id.as_str()) {
        return problem(StatusCode::NOT_FOUND, "There's no such editor.");
    }
    let fields = match form_fields(req).await {
        Ok(f) => f,
        Err(e) => return problem(StatusCode::BAD_REQUEST, &format!("The request couldn't be read: {e}")),
    };
    let src = q.get("WOPISrc").or_else(|| q.get("wopisrc")).cloned().unwrap_or_default();
    let cfg = app.cfg();
    let Some((host, file_id)) = cfg.check_wopi_src(&src) else {
        tracing::warn!("refused a WOPISrc outside wopi_hosts: {}", ids::clean(&src, 200));
        return problem(StatusCode::FORBIDDEN, "This Nextcloud isn't allowed to use this craft-live server. Its administrator adds it to wopi_hosts.");
    };
    let token = fields.get("access_token").cloned().unwrap_or_default();
    if token.is_empty() || token.len() > 4096 {
        return problem(StatusCode::BAD_REQUEST, "Nextcloud didn't send an access token.");
    }
    let ttl: i64 = fields.get("access_token_ttl").or_else(|| fields.get("access_token_TTL")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let ctx = app.rooms.ctx();
    let info = match ctx.wopi.check_file_info(&src, &token).await {
        Ok(i) => i,
        Err(WopiError::Unauthorized | WopiError::NotFound) => {
            return problem(StatusCode::FORBIDDEN, "Nextcloud didn't accept this editing session. Open the file again.");
        }
        Err(e) => {
            tracing::warn!("CheckFileInfo failed: {e}");
            return problem(StatusCode::BAD_GATEWAY, &format!("craft-live couldn't reach Nextcloud ({e})."));
        }
    };
    let origin = if info.post_message_origin.is_empty() { origin_of(&host.base).unwrap_or_default() } else { info.post_message_origin.clone() };
    let fa_with = if fa.contains(&origin) || origin.is_empty() { fa.clone() } else { format!("{fa} {origin}") };
    if config::app_for_name(&info.base_file_name).is_some_and(|a| a != app_id) {
        return page(StatusCode::BAD_REQUEST, shell::problem_page(&format!("{} isn't a file this editor opens.", info.base_file_name), &origin), &fa_with);
    }
    let view = q.get("mode").is_some_and(|m| m == "view") || q.get("permission").is_some_and(|p| p == "readonly");
    let role = if info.can_edit() && !view { Role::Edit } else { Role::View };
    let user_id = if info.user_id.is_empty() || info.is_anonymous_user { String::new() } else { info.user_id.clone() };
    let user = User::new(&user_id, &info.user_friendly_name);
    let room = config::room_id(&host, &file_id);
    let now = ids::now_ms();
    let max_ttl = now.saturating_add(i64::try_from(cfg.session_ttl().as_millis()).unwrap_or(i64::MAX));
    let expires = if ttl > now { ttl } else { max_ttl };
    let session = ctx.sessions.insert(Session {
        token: ids::token(32),
        room: room.clone(),
        kind: Kind::Web,
        user: user.clone(),
        role,
        app: app_id.clone(),
        file_name: info.base_file_name.clone(),
        wopi_src: src.clone(),
        access_token: token,
        access_token_ttl: if ttl > 0 { ttl } else { 0 },
        nextcloud: host.base.clone(),
        file_id: file_id.clone(),
        created_ms: now,
        expires_ms: expires,
        post_message_origin: origin.clone(),
        close_url: info.close_url.clone(),
    });
    let s2 = session.clone();
    let version = info.version.clone();
    let locks = info.supports_locks;
    let prepared = match app
        .rooms
        .call(&room, move |reply| Msg::Prepare { session: s2.clone(), version: Some(version.clone()), etag: None, supports_locks: locks, reply })
        .await
    {
        Ok(p) => p,
        Err(e) => return page(StatusCode::SERVICE_UNAVAILABLE, shell::problem_page(&e, &origin), &fa_with),
    };
    tracing::info!(room = %room, user = %user.id, role = role.as_str(), seed = prepared.seed, "opened the editor");
    let embed = embed_config(cfg, &session, prepared.seed, q.get("lang").map(String::as_str).unwrap_or(""));
    let app_url = cfg.apps.get(&app_id).map(|_| format!("/apps/{app_id}/index.html#{}", embed_fragment(&embed)));
    let label = match app_id.as_str() {
        "gridcraft" => "GridCraft",
        "wordcraft" => "WordCraft",
        _ => "DeckCraft",
    };
    let shell_cfg = json!({
        "origin": origin,
        "closeUrl": info.close_url,
        "token": session.token,
        "api": format!("{}/api/v1", cfg.public_url),
        "fileName": info.base_file_name,
        "embed": embed,
        "helloTimeoutMs": 20000,
        "notLive": format!("This {label} build can't edit documents live yet: it needs craft-live support (the embed contract in craft-live's docs/design.md). Nothing was changed."),
        "problem": format!("{label} isn't installed on this craft-live server."),
    });
    page(StatusCode::OK, shell::editor_page(&format!("{} – {label}", info.base_file_name), &shell_cfg, app_url.as_deref()), &fa_with)
}

/// What the app needs (the "craft-live embed" contract, docs/design.md).
fn embed_config(cfg: &crate::config::Config, s: &Session, seed: bool, lang: &str) -> Json {
    let room_enc = ids::pct(&s.room);
    json!({
        "v": 1,
        "ws": format!("{}/ws/{room_enc}", ws_base(&cfg.public_url)),
        "room": s.room,
        "token": s.token,
        "user": s.user.json(),
        "role": s.role.as_str(),
        "seed": seed,
        "file": format!("{}/api/v1/rooms/{room_enc}/file", cfg.public_url),
        "name": s.file_name,
        "format": config::extension(&s.file_name),
        "api": format!("{}/api/v1", cfg.public_url),
        "lang": ids::clean(lang, 16),
    })
}

/// The embed configuration as a URL fragment: `craft-live=1&ws=…&room=…` (strings as they are,
/// `user` as JSON, booleans as 1/0).
pub fn embed_fragment(embed: &Json) -> String {
    let mut parts = vec!["craft-live=1".to_string()];
    if let Some(o) = embed.as_object() {
        for (k, v) in o {
            if k == "v" {
                continue;
            }
            let value = match v {
                Json::String(s) => s.clone(),
                Json::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
                other => other.to_string(),
            };
            parts.push(format!("{k}={}", ids::pct(&value)));
        }
    }
    parts.join("&")
}

// ----- Static files -----

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A file inside `root` (no `..`, no hidden files, no symlinks out of it).
async fn serve_from(root: &Path, rel: &str) -> Response {
    if rel.is_empty() || rel.len() > 1024 || rel.split('/').any(|s| s.is_empty() || s.starts_with('.') || s.contains('\\')) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let (Ok(root), Ok(path)) = (tokio::fs::canonicalize(root).await, tokio::fs::canonicalize(root.join(rel)).await) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    if !path.starts_with(&root) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let Ok(meta) = tokio::fs::metadata(&path).await else { return text(StatusCode::NOT_FOUND, "not found") };
    if !meta.is_file() || meta.len() > 512 << 20 {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let Ok(file) = tokio::fs::File::open(&path).await else { return text(StatusCode::NOT_FOUND, "not found") };
    let stream = tokio_util_stream(file);
    let cache = if rel.ends_with(".html") { "no-cache" } else { "public, max-age=3600" };
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(&path)));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&meta.len().to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    r
}

/// A file as a stream of chunks.
fn tokio_util_stream(file: tokio::fs::File) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 << 10];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}

async fn app_file(State(app): St, UrlPath((app_id, path)): UrlPath<(String, String)>) -> Response {
    let Some(root) = app.cfg().apps.get(&app_id).cloned() else { return text(StatusCode::NOT_FOUND, "not found") };
    let mut r = serve_from(&root, &path).await;
    // The apps' builds are only for the editor page (same origin).
    if let Ok(v) = HeaderValue::from_str("frame-ancestors 'self'") {
        r.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

/// The apps' icons (original, drawn here: a coloured tile with the app's initial).
async fn static_file(UrlPath(file): UrlPath<String>) -> Response {
    let (letter, color) = match file.as_str() {
        "gridcraft.svg" => ("G", "#2E9E5B"),
        "wordcraft.svg" => ("W", "#3B7DD8"),
        "deckcraft.svg" => ("D", "#D8513B"),
        _ => return text(StatusCode::NOT_FOUND, "not found"),
    };
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\"><rect width=\"32\" height=\"32\" rx=\"7\" fill=\"{color}\"/><text x=\"16\" y=\"22.5\" font-family=\"sans-serif\" font-size=\"18\" font-weight=\"700\" text-anchor=\"middle\" fill=\"#fff\">{letter}</text></svg>"
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")], svg).into_response()
}

// ----- The room WebSocket -----

async fn ws(State(app): St, UrlPath(room): UrlPath<String>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, up: WebSocketUpgrade) -> Response {
    let session = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if session.room != room {
        return err_json(403, "this session is for another document");
    }
    // A browser page from another site can't use someone's token here.
    if let Some(o) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let ours = origin_of(&app.cfg().public_url).unwrap_or_default();
        if !o.eq_ignore_ascii_case(&ours) && o != "null" {
            return err_json(403, "WebSocket from another origin");
        }
    }
    let max = crate::room::MAX_UPDATE + 1024;
    up.max_message_size(max).max_frame_size(max).on_upgrade(move |socket| connection(app, session, socket))
}

async fn connection(app: Arc<App>, session: Arc<Session>, socket: WebSocket) {
    let conn = app.next_conn.fetch_add(1, Ordering::Relaxed);
    let (out_tx, mut out_rx) = mpsc::channel::<Out>(OUT_QUEUE);
    let (mut sink, mut stream) = socket.split();
    let s2 = session.clone();
    let tx2 = out_tx.clone();
    let joined = app.rooms.call(&session.room, move |reply| Msg::Join { conn, session: s2.clone(), out: tx2.clone(), reply }).await;
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => {
            let _ = sink.send(Message::Binary(Frame::Denied(e.clone()).encode())).await;
            let _ = sink.send(Message::Close(Some(CloseFrame { code: 4403, reason: e.into() }))).await;
            return;
        }
    }
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.tick().await;
        loop {
            tokio::select! {
                m = out_rx.recv() => match m {
                    Some(Out::Frame(b)) => {
                        if sink.send(Message::Binary(b)).await.is_err() { break; }
                    }
                    Some(Out::Close(code, reason)) => {
                        let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                        break;
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new())).await.is_err() { break; }
                }
            }
        }
    });
    let room = session.room.clone();
    let idle = Duration::from_secs(120);
    loop {
        let next = tokio::time::timeout(idle, stream.next()).await;
        let msg = match next {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        match msg {
            Message::Binary(b) => match Frame::decode(&b) {
                Some(frame) => app.rooms.send(&room, Msg::Frame { conn, frame }).await,
                None => tracing::debug!(room = %room, conn, "dropped a frame that isn't y-websocket"),
            },
            Message::Close(_) => break,
            _ => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    app.rooms.send(&room, Msg::Leave { conn }).await;
    writer.abort();
}

// ----- The API -----

/// `POST /api/v1/sessions` (desktop): `{nextcloud, path, fileId}` with `Authorization: Basic`
/// (Nextcloud login name and app password). The password is used for two requests to that
/// Nextcloud and forgotten.
async fn desktop_session(State(app): St, headers: HeaderMap, body: Bytes) -> Response {
    let cfg = app.cfg();
    if !cfg.desktop_sessions {
        return err_json(403, "desktop sessions are turned off on this server");
    }
    let Ok(req) = serde_json::from_slice::<Json>(&body) else { return err_json(400, "the body isn't JSON") };
    let field = |k: &str| req.get(k).and_then(Json::as_str).unwrap_or("").trim().to_string();
    let Some(host) = cfg.host_for_base(&field("nextcloud")) else { return err_json(403, "this Nextcloud isn't allowed on this craft-live server") };
    let path = field("path");
    let file_id = match req.get("fileId") {
        Some(Json::Number(n)) => n.to_string(),
        _ => field("fileId"),
    };
    let basic = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ")).map(str::trim).unwrap_or("");
    let decoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(basic).ok().and_then(|b| String::from_utf8(b).ok())
    };
    let Some((login, password)) = decoded.as_deref().and_then(|d| d.split_once(':')) else {
        return err_json(401, "Basic credentials (login and app password) are required");
    };
    let Some(app_id) = config::app_for_name(&path) else { return err_json(400, "craft-live doesn't open this kind of file") };
    let ident = match nextcloud::verify(&host.base, login, password, &path, &file_id, cfg.verify_tls).await {
        Ok(i) => i,
        Err((status, msg)) => return err_json(status, &msg),
    };
    let room = config::room_id(&host, &ident.file_id);
    let now = ids::now_ms();
    let user = User::new(&ident.user_id, &ident.display_name);
    let role = if ident.can_write { Role::Edit } else { Role::View };
    let session = app.rooms.ctx().sessions.insert(Session {
        token: ids::token(32),
        room: room.clone(),
        kind: Kind::Desktop,
        user,
        role,
        app: app_id.to_string(),
        file_name: ident.name.clone(),
        wopi_src: String::new(),
        access_token: String::new(),
        access_token_ttl: 0,
        nextcloud: host.base.clone(),
        file_id: ident.file_id.clone(),
        created_ms: now,
        expires_ms: now.saturating_add(i64::try_from(cfg.session_ttl().as_millis()).unwrap_or(i64::MAX)),
        post_message_origin: String::new(),
        close_url: String::new(),
    });
    let s2 = session.clone();
    let etag = ident.etag.clone();
    let prepared = match app
        .rooms
        .call(&room, move |reply| Msg::Prepare { session: s2.clone(), version: None, etag: Some(etag.clone()), supports_locks: false, reply })
        .await
    {
        Ok(p) => p,
        Err(e) => return err_json(503, &e),
    };
    tracing::info!(room = %room, user = %session.user.id, role = role.as_str(), seed = prepared.seed, "desktop session");
    let mut out = embed_config(cfg, &session, prepared.seed, "");
    if let Some(o) = out.as_object_mut() {
        o.insert("seeded".into(), json!(prepared.seeded));
        o.insert("expires".into(), json!(session.expires_ms));
        o.insert("app".into(), json!(app_id));
        o.insert("etag".into(), json!(ident.etag));
    }
    no_store(JsonBody(out).into_response())
}

/// `GET /api/v1/session`: the embed configuration again (an app that lost its fragment).
async fn session_info(State(app): St, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut v = embed_config(app.cfg(), &s, false, "");
    if let Some(o) = v.as_object_mut() {
        o.remove("seed");
        o.remove("token");
    }
    no_store(JsonBody(v).into_response())
}

/// `POST /api/v1/prepare`: asks again for the seed lease (after a `reseed`).
async fn prepare_again(State(app): St, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let s2 = s.clone();
    match app.rooms.call(&s.room, move |reply| Msg::Prepare { session: s2.clone(), version: None, etag: None, supports_locks: false, reply }).await {
        Ok(p) => no_store(JsonBody(json!({"seed": p.seed, "seeded": p.seeded})).into_response()),
        Err(e) => err_json(503, &e),
    }
}

/// `POST /api/v1/save`: save now (Nextcloud's `Action_Save`, a Save button).
async fn save_now(State(app): St, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if s.role != Role::Edit {
        return err_json(403, "viewers can't save");
    }
    let force = q.get("force").is_some_and(|v| v == "1" || v == "true");
    app.rooms.send(&s.room, Msg::SaveNow { force }).await;
    JsonBody(json!({"ok": true})).into_response()
}

/// `GET /api/v1/rooms/<room>/file`: the file, for the client that seeds the room (web sessions;
/// a desktop client downloads it through its own WebDAV access).
async fn download(State(app): St, UrlPath(room): UrlPath<String>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if s.room != room {
        return err_json(403, "this session is for another document");
    }
    if !s.wopi_usable(ids::now_ms()) {
        return err_json(409, "this session has no WOPI access: download the file from Nextcloud");
    }
    let ctx = app.rooms.ctx();
    match ctx.wopi.get_file(&s.wopi_src, &s.access_token, ctx.cfg.max_file()).await {
        Ok((bytes, version)) => {
            let mut r = Response::new(Body::from(bytes));
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(v) = HeaderValue::from_str(&version) {
                h.insert("X-Craft-Version", v);
            }
            if let Ok(v) = HeaderValue::from_str(&ids::pct(&s.file_name)) {
                h.insert("X-Craft-File-Name", v);
            }
            r
        }
        Err(WopiError::TooLarge) => err_json(413, "the file is larger than this server allows"),
        Err(WopiError::Unauthorized) => err_json(401, "Nextcloud refused the session"),
        Err(e) => err_json(502, &e.to_string()),
    }
}

/// `PUT /api/v1/rooms/<room>/file?save=<id>`: the file bytes the room asked for, with
/// `X-Craft-File-Name` (the name the app gave, percent-encoded). Answers once the WOPI host
/// has them.
async fn upload(State(app): St, UrlPath(room): UrlPath<String>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if s.room != room {
        return err_json(403, "this session is for another document");
    }
    if s.role != Role::Edit {
        return err_json(403, "viewers can't save");
    }
    let Some(save) = q.get("save").and_then(|v| v.parse::<u64>().ok()) else { return err_json(400, "save=<id> is required") };
    let name =
        headers.get("X-Craft-File-Name").and_then(|v| v.to_str().ok()).and_then(|v| ids::unpct(v, false)).map(|v| ids::clean(&v, 512)).unwrap_or_default();
    let body = body.to_vec();
    let s2 = s.clone();
    let result = app.rooms.call(&room, move |reply| Msg::Upload { save, session: s2.clone(), name: name.clone(), body: body.clone(), reply }).await;
    match result {
        Ok(Ok(v)) => JsonBody(v).into_response(),
        Ok(Err((status, msg))) => err_json(status, &msg),
        Err(e) => err_json(503, &e),
    }
}

/// `GET /api/v1/rooms/<room>/status`: the room's state, for people in it.
async fn room_status(State(app): St, UrlPath(room): UrlPath<String>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = match session_from(&app, &headers, &q) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if s.room != room {
        return err_json(403, "this session is for another document");
    }
    match app.rooms.call(&room, |reply| Msg::Status { reply }).await {
        Ok(v) => JsonBody(v).into_response(),
        Err(e) => err_json(503, &e),
    }
}

/// The folder an app's build would be served from (for `craft-live check`).
pub fn app_dir(cfg: &crate::config::Config, app: &str) -> Option<PathBuf> {
    cfg.apps.get(app).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_and_origins() {
        let f = embed_fragment(&json!({"v": 1, "ws": "wss://l/ws/nc%3Ax", "seed": true, "user": {"id": "a"}}));
        assert_eq!(f, "craft-live=1&ws=wss%3A%2F%2Fl%2Fws%2Fnc%253Ax&seed=1&user=%7B%22id%22%3A%22a%22%7D");
        assert_eq!(origin_of("https://Cloud.example:8443/index.php").as_deref(), Some("https://cloud.example:8443"));
        assert_eq!(ws_base("https://live.example"), "wss://live.example");
        assert_eq!(ws_base("http://127.0.0.1:1"), "ws://127.0.0.1:1");
    }
}
