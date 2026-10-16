//! A fake Nextcloud for the tests: the richdocuments WOPI endpoint (CheckFileInfo, GetFile,
//! PutFile, locks, PutRelativeFile, with proof-key checking), OCS (`cloud/user`,
//! `cloud/capabilities`), WebDAV (PROPFIND, GET, PUT, SEARCH by file id), Login Flow v2 and
//! `status.php`. It behaves as the WOPI documentation says a host does; what real Nextcloud does
//! differently is listed in docs/design.md.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey};
use serde_json::json;
use sha2::Sha256;

/// A file on the fake server.
#[derive(Clone, Debug)]
pub struct File {
    pub id: u64,
    /// `/Budget.xlsx` in the owner's files.
    pub path: String,
    pub owner: String,
    pub bytes: Vec<u8>,
    pub version: u64,
    pub lock: Option<String>,
}

impl File {
    pub fn name(&self) -> String {
        self.path.rsplit('/').next().unwrap_or("").to_string()
    }
}

/// A WOPI access token.
#[derive(Clone, Debug)]
pub struct Token {
    pub user: String,
    pub name: String,
    pub file: u64,
    pub write: bool,
    pub expired: bool,
}

/// What the fake holds; tests read and change it.
#[derive(Debug, Default)]
pub struct FakeState {
    pub files: BTreeMap<u64, File>,
    pub tokens: HashMap<String, Token>,
    /// `login` → `(app password, user id, display name)`.
    pub accounts: HashMap<String, (String, String, String)>,
    pub supports_locks: bool,
    /// Proof key (modulus, exponent) to check `X-WOPI-Proof` against; requests without a proof
    /// are refused when it's set.
    pub proof_key: Option<RsaPublicKey>,
    pub proof_failures: u32,
    pub proofs_ok: u32,
    /// Every WOPI and WebDAV call, in order (`"PutFile 1 lock=… autosave=true"`).
    pub log: Vec<String>,
    pub base: String,
    /// `cloud/capabilities` → `richdocuments.config.wopi_url`.
    pub wopi_url: String,
    pub post_message_origin: String,
    pub next_id: u64,
}

impl FakeState {
    pub fn file_by_path(&self, owner: &str, path: &str) -> Option<&File> {
        self.files.values().find(|f| f.owner == owner && f.path == path)
    }
}

/// A running fake Nextcloud.
#[derive(Clone)]
pub struct FakeNextcloud {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<FakeState>>,
}

type St = State<Arc<Mutex<FakeState>>>;

fn lock(st: &Arc<Mutex<FakeState>>) -> MutexGuard<'_, FakeState> {
    match st.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl FakeNextcloud {
    pub async fn start() -> FakeNextcloud {
        let state = Arc::new(Mutex::new(FakeState { supports_locks: true, next_id: 100, ..FakeState::default() }));
        let app = Router::new()
            .route("/status.php", get(|| async { Json(json!({"installed": true, "productname": "Fake Nextcloud"})) }))
            .route("/index.php/apps/richdocuments/wopi/files/:id", get(check_file_info).post(file_op))
            .route("/index.php/apps/richdocuments/wopi/files/:id/contents", get(get_file).post(put_file))
            .route("/ocs/v2.php/cloud/user", get(ocs_user))
            .route("/ocs/v2.php/cloud/capabilities", get(ocs_capabilities))
            .route("/index.php/login/v2", post(login_start))
            .route("/login/v2/poll", post(login_poll))
            .route("/remote.php/dav/files/:user/*path", any(dav))
            .route("/remote.php/dav/files/:user", any(dav_root))
            .route("/remote.php/dav/", any(dav_search))
            .route("/remote.php/dav", any(dav_search))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        lock(&state).base = format!("http://{addr}");
        lock(&state).post_message_origin = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeNextcloud { addr, state }
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn st(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.state)
    }

    /// Adds a file and returns its id.
    pub fn add_file(&self, owner: &str, path: &str, bytes: &[u8]) -> u64 {
        let mut s = self.st();
        s.next_id += 1;
        let id = s.next_id;
        s.files.insert(id, File { id, path: path.into(), owner: owner.into(), bytes: bytes.to_vec(), version: 1, lock: None });
        id
    }

    /// A WOPI token for a user and a file.
    pub fn token(&self, user: &str, name: &str, file: u64, write: bool) -> String {
        let t = format!("tok-{user}-{file}-{}", self.st().tokens.len());
        self.st().tokens.insert(t.clone(), Token { user: user.into(), name: name.into(), file, write, expired: false });
        t
    }

    /// The WOPISrc of a file (`<id>_<instance>` as Nextcloud makes them).
    pub fn wopi_src(&self, file: u64) -> String {
        format!("{}/index.php/apps/richdocuments/wopi/files/{file}_ocfake123", self.base())
    }

    pub fn add_account(&self, login: &str, password: &str, user: &str, display: &str) {
        self.st().accounts.insert(login.into(), (password.into(), user.into(), display.into()));
    }

    pub fn file(&self, id: u64) -> File {
        self.st().files.get(&id).cloned().expect("file")
    }

    pub fn log(&self) -> Vec<String> {
        self.st().log.clone()
    }

    /// Changes a file as if someone uploaded a new version in Nextcloud.
    pub fn change_outside(&self, id: u64, bytes: &[u8]) {
        let mut s = self.st();
        if let Some(f) = s.files.get_mut(&id) {
            f.bytes = bytes.to_vec();
            f.version += 1;
        }
    }

    /// Uses the proof key published in a discovery XML.
    pub fn trust_discovery(&self, xml: &str) {
        let attr = |name: &str| -> Option<String> {
            let tag = xml.split("<proof-key").nth(1)?;
            let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
            let rest = tag.get(start..)?;
            Some(rest.get(..rest.find('"')?)?.to_string())
        };
        let (Some(m), Some(e)) = (attr("modulus"), attr("exponent")) else { return };
        let n = BigUint::from_bytes_be(&STANDARD.decode(m).unwrap_or_default());
        let e = BigUint::from_bytes_be(&STANDARD.decode(e).unwrap_or_default());
        self.st().proof_key = RsaPublicKey::new(n, e).ok();
    }
}

// ----- WOPI -----

fn file_id(id: &str) -> u64 {
    id.split('_').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Checks the token (query and header must agree) and the proof; the token's record.
fn auth(s: &mut FakeState, id: u64, q: &HashMap<String, String>, h: &HeaderMap, method: &Method, path: &str) -> Result<Token, Response> {
    let token = q.get("access_token").cloned().unwrap_or_default();
    let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if bearer != token {
        return Err((StatusCode::UNAUTHORIZED, "token mismatch").into_response());
    }
    if let Some(key) = s.proof_key.clone() {
        let url = format!("{}{path}?access_token={}", s.base, pct(&token));
        let ok = check_proof(&key, &token, &url, h);
        if ok {
            s.proofs_ok += 1;
        } else {
            s.proof_failures += 1;
            s.log.push(format!("ProofFailed {method} {path}"));
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "Invalid WOPI proof").into_response());
        }
    }
    let t = s.tokens.get(&token).cloned().ok_or_else(|| (StatusCode::UNAUTHORIZED, "unknown token").into_response())?;
    if t.expired {
        return Err((StatusCode::UNAUTHORIZED, "expired").into_response());
    }
    if t.file != id || !s.files.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, "no such file").into_response());
    }
    Ok(t)
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Microsoft's proof check, written independently of the server's.
fn check_proof(key: &RsaPublicKey, token: &str, url: &str, h: &HeaderMap) -> bool {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Ok(ticks) = get("X-WOPI-TimeStamp").parse::<u64>() else { return false };
    let mut data = Vec::new();
    data.extend_from_slice(&(token.len() as u32).to_be_bytes());
    data.extend_from_slice(token.as_bytes());
    let up = url.to_uppercase();
    data.extend_from_slice(&(up.len() as u32).to_be_bytes());
    data.extend_from_slice(up.as_bytes());
    data.extend_from_slice(&8u32.to_be_bytes());
    data.extend_from_slice(&ticks.to_be_bytes());
    let verify = |sig: &str| {
        STANDARD
            .decode(sig)
            .ok()
            .and_then(|b| Signature::try_from(b.as_slice()).ok())
            .is_some_and(|s| VerifyingKey::<Sha256>::new(key.clone()).verify(&data, &s).is_ok())
    };
    verify(&get("X-WOPI-Proof")) || verify(&get("X-WOPI-ProofOld"))
}

async fn check_file_info(State(st): St, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>, h: HeaderMap) -> Response {
    let fid = file_id(&id);
    let mut s = lock(&st);
    let t = match auth(&mut s, fid, &q, &h, &Method::GET, &format!("/index.php/apps/richdocuments/wopi/files/{id}")) {
        Ok(t) => t,
        Err(r) => return r,
    };
    s.log.push(format!("CheckFileInfo {fid} {}", t.user));
    let origin = s.post_message_origin.clone();
    let locks = s.supports_locks;
    let Some(f) = s.files.get(&fid) else { return StatusCode::NOT_FOUND.into_response() };
    Json(json!({
        "BaseFileName": f.name(), "OwnerId": f.owner, "Size": f.bytes.len(), "UserId": t.user, "UserFriendlyName": t.name,
        "Version": f.version.to_string(), "UserCanWrite": t.write, "SupportsLocks": locks, "SupportsGetLock": locks, "SupportsUpdate": true,
        "PostMessageOrigin": origin, "UserCanNotWriteRelative": false,
    }))
    .into_response()
}

async fn get_file(State(st): St, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>, h: HeaderMap) -> Response {
    let fid = file_id(&id);
    let mut s = lock(&st);
    if let Err(r) = auth(&mut s, fid, &q, &h, &Method::GET, &format!("/index.php/apps/richdocuments/wopi/files/{id}/contents")) {
        return r;
    }
    s.log.push(format!("GetFile {fid}"));
    let Some(f) = s.files.get(&fid) else { return StatusCode::NOT_FOUND.into_response() };
    let max: usize = h.get("X-WOPI-MaxExpectedSize").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    if f.bytes.len() > max {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    (StatusCode::OK, [("X-WOPI-ItemVersion", f.version.to_string())], f.bytes.clone()).into_response()
}

fn conflict(current: &Option<String>) -> Response {
    (StatusCode::CONFLICT, [("X-WOPI-Lock", current.clone().unwrap_or_default()), ("X-WOPI-LockFailureReason", "mismatch".to_string())]).into_response()
}

async fn put_file(State(st): St, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>, h: HeaderMap, body: Bytes) -> Response {
    let fid = file_id(&id);
    let mut s = lock(&st);
    let t = match auth(&mut s, fid, &q, &h, &Method::POST, &format!("/index.php/apps/richdocuments/wopi/files/{id}/contents")) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if get("X-WOPI-Override") != "PUT" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !t.write {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let lock_hdr = get("X-WOPI-Lock");
    s.log.push(format!("PutFile {fid} lock={} autosave={} editors={}", !lock_hdr.is_empty(), get("X-COOL-WOPI-IsAutosave"), get("X-WOPI-Editors")));
    let locks = s.supports_locks;
    let Some(f) = s.files.get_mut(&fid) else { return StatusCode::NOT_FOUND.into_response() };
    if locks {
        match &f.lock {
            Some(l) if *l != lock_hdr => return conflict(&f.lock),
            None if !f.bytes.is_empty() && !lock_hdr.is_empty() => return conflict(&None),
            _ => {}
        }
    }
    f.bytes = body.to_vec();
    f.version += 1;
    (StatusCode::OK, [("X-WOPI-ItemVersion", f.version.to_string())]).into_response()
}

async fn file_op(State(st): St, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>, h: HeaderMap, body: Bytes) -> Response {
    let fid = file_id(&id);
    let mut s = lock(&st);
    let t = match auth(&mut s, fid, &q, &h, &Method::POST, &format!("/index.php/apps/richdocuments/wopi/files/{id}")) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let op = get("X-WOPI-Override");
    let l = get("X-WOPI-Lock");
    s.log.push(format!("{op} {fid}"));
    let locks = s.supports_locks;
    if op == "PUT_RELATIVE" {
        let suggested = get("X-WOPI-SuggestedTarget");
        let owner = s.files.get(&fid).map(|f| f.owner.clone()).unwrap_or_default();
        let dir = s.files.get(&fid).map(|f| f.path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()).unwrap_or_default();
        s.next_id += 1;
        let nid = s.next_id;
        let name = suggested.clone();
        s.files.insert(nid, File { id: nid, path: format!("{dir}/{name}"), owner, bytes: body.to_vec(), version: 1, lock: None });
        let token = format!("tok-rel-{nid}");
        s.tokens.insert(token.clone(), Token { file: nid, ..t });
        s.log.push(format!("Created {nid} {name}"));
        let base = s.base.clone();
        return Json(json!({"Name": name, "Url": format!("{base}/index.php/apps/richdocuments/wopi/files/{nid}_ocfake123?access_token={token}")}))
            .into_response();
    }
    if !locks {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }
    let Some(f) = s.files.get_mut(&fid) else { return StatusCode::NOT_FOUND.into_response() };
    match op.as_str() {
        "LOCK" => {
            let old = get("X-WOPI-OldLock");
            if !old.is_empty() {
                if f.lock.as_deref() != Some(old.as_str()) {
                    return conflict(&f.lock);
                }
                f.lock = Some(l);
                return StatusCode::OK.into_response();
            }
            if l.is_empty() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            match &f.lock {
                None => f.lock = Some(l),
                Some(cur) if *cur == l => {}
                Some(_) => return conflict(&f.lock),
            }
            StatusCode::OK.into_response()
        }
        "REFRESH_LOCK" | "UNLOCK" => {
            if f.lock.as_deref() != Some(l.as_str()) || l.is_empty() {
                return conflict(&f.lock);
            }
            if op == "UNLOCK" {
                f.lock = None;
            }
            StatusCode::OK.into_response()
        }
        "GET_LOCK" => (StatusCode::OK, [("X-WOPI-Lock", f.lock.clone().unwrap_or_default())]).into_response(),
        _ => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

// ----- OCS and login -----

/// The account behind `Authorization: Basic`.
fn basic(s: &FakeState, h: &HeaderMap) -> Option<(String, String)> {
    let b = h.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Basic ")?;
    let d = String::from_utf8(STANDARD.decode(b.trim()).ok()?).ok()?;
    let (login, pass) = d.split_once(':')?;
    let (p, user, name) = s.accounts.get(login)?;
    (p == pass).then(|| (user.clone(), name.clone()))
}

async fn ocs_user(State(st): St, h: HeaderMap) -> Response {
    let s = lock(&st);
    match basic(&s, &h) {
        Some((user, name)) => Json(json!({"ocs": {"meta": {"status": "ok"}, "data": {"id": user, "displayname": name}}})).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn ocs_capabilities(State(st): St) -> Response {
    let s = lock(&st);
    Json(json!({"ocs": {"data": {"capabilities": {"richdocuments": {"config": {"wopi_url": s.wopi_url}}}}}})).into_response()
}

async fn login_start(State(st): St) -> Response {
    let base = lock(&st).base.clone();
    Json(json!({"poll": {"token": "polltoken", "endpoint": format!("{base}/login/v2/poll")}, "login": format!("{base}/login/v2/flow/abc")})).into_response()
}

/// Approves at once with the first account.
async fn login_poll(State(st): St) -> Response {
    let s = lock(&st);
    let base = s.base.clone();
    match s.accounts.iter().next() {
        Some((login, (pass, _, _))) => Json(json!({"server": base, "loginName": login, "appPassword": pass})).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ----- WebDAV -----

fn prop_response(user: &str, path: &str, f: Option<&File>) -> String {
    let href = format!("/remote.php/dav/files/{user}{}", path.split('/').map(pct).collect::<Vec<_>>().join("/"));
    match f {
        Some(f) => format!(
            "<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:displayname>{}</d:displayname><d:getcontentlength>{}</d:getcontentlength><d:getlastmodified>Fri, 16 Oct 2026 10:00:00 GMT</d:getlastmodified><d:getetag>\"e{}-{}\"</d:getetag><d:resourcetype/><oc:fileid>{}</oc:fileid><oc:permissions>RGDNVW</oc:permissions></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
            f.name(),
            f.bytes.len(),
            f.id,
            f.version,
            f.id
        ),
        None => format!(
            "<d:response><d:href>{href}/</d:href><d:propstat><d:prop><d:displayname>{}</d:displayname><d:resourcetype><d:collection/></d:resourcetype><oc:fileid>1</oc:fileid><oc:permissions>RGDNVCK</oc:permissions></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
            path.rsplit('/').next().unwrap_or("")
        ),
    }
}

fn multistatus(inner: &str) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\">{inner}</d:multistatus>"),
    )
        .into_response()
}

async fn dav_root(state: St, Path(user): Path<String>, req: Request) -> Response {
    dav_inner(state, user, String::new(), req).await
}

async fn dav(state: St, Path((user, path)): Path<(String, String)>, req: Request) -> Response {
    dav_inner(state, user, path, req).await
}

async fn dav_inner(State(st): St, user: String, path: String, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), 64 << 20).await.unwrap_or_default();
    let mut s = lock(&st);
    let Some((uid, _)) = basic(&s, &headers) else { return StatusCode::UNAUTHORIZED.into_response() };
    if uid != user {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = format!("/{}", path.trim_end_matches('/'));
    let path = if path == "/" { "/".to_string() } else { path };
    s.log.push(format!("DAV {method} {path}"));
    match method.as_str() {
        "PROPFIND" => {
            let depth = headers.get("Depth").and_then(|v| v.to_str().ok()).unwrap_or("1").to_string();
            if let Some(f) = s.file_by_path(&uid, &path) {
                return multistatus(&prop_response(&uid, &path, Some(f)));
            }
            let prefix = if path == "/" { "/".to_string() } else { format!("{path}/") };
            let children: Vec<&File> = s.files.values().filter(|f| f.owner == uid && f.path.starts_with(&prefix)).collect();
            if children.is_empty() && path != "/" {
                return StatusCode::NOT_FOUND.into_response();
            }
            let mut inner = prop_response(&uid, if path == "/" { "" } else { &path }, None);
            if depth != "0" {
                for f in children.iter().filter(|f| !f.path[prefix.len()..].contains('/')) {
                    inner.push_str(&prop_response(&uid, &f.path, Some(f)));
                }
            }
            multistatus(&inner)
        }
        "GET" => match s.file_by_path(&uid, &path) {
            Some(f) => (StatusCode::OK, [("ETag", format!("\"e{}-{}\"", f.id, f.version)), ("OC-FileId", f.id.to_string())], f.bytes.clone()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        "PUT" => {
            let if_match = headers.get("If-Match").and_then(|v| v.to_str().ok()).map(|v| v.trim_matches('"').to_string());
            let existing = s.files.values_mut().find(|f| f.owner == uid && f.path == path);
            match existing {
                Some(f) => {
                    let etag = format!("e{}-{}", f.id, f.version);
                    if if_match.is_some_and(|m| m != etag) {
                        return StatusCode::PRECONDITION_FAILED.into_response();
                    }
                    if f.lock.is_some() {
                        return StatusCode::LOCKED.into_response();
                    }
                    f.bytes = body.to_vec();
                    f.version += 1;
                    (StatusCode::NO_CONTENT, [("OC-ETag", format!("\"e{}-{}\"", f.id, f.version)), ("OC-FileId", f.id.to_string())]).into_response()
                }
                None => {
                    s.next_id += 1;
                    let id = s.next_id;
                    s.files.insert(id, File { id, path: path.clone(), owner: uid.clone(), bytes: body.to_vec(), version: 1, lock: None });
                    (StatusCode::CREATED, [("OC-ETag", format!("\"e{id}-1\"")), ("OC-FileId", id.to_string())]).into_response()
                }
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// `SEARCH /remote.php/dav/` for `oc:fileid` = n (Nextcloud's WebDAV search).
async fn dav_search(State(st): St, req: Request) -> Response {
    let headers = req.headers().clone();
    let method = req.method().clone();
    let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
    let s = lock(&st);
    let Some((uid, _)) = basic(&s, &headers) else { return StatusCode::UNAUTHORIZED.into_response() };
    if method.as_str() != "SEARCH" {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let text = String::from_utf8_lossy(&body);
    let id: Option<u64> = text.split("<d:literal>").nth(1).and_then(|r| r.split('<').next()).and_then(|v| v.trim().parse().ok());
    let inner = match id.and_then(|i| s.files.get(&i)).filter(|f| f.owner == uid) {
        Some(f) => prop_response(&uid, &f.path, Some(f)),
        None => String::new(),
    };
    multistatus(&inner)
}
