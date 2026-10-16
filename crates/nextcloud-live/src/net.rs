//! HTTP: Nextcloud (Login Flow v2, OCS, WebDAV) and craft-live's API. Every request has a
//! connect and a total timeout, answers are read up to a limit, and redirects aren't followed.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use quick_xml::events::Event;
use serde_json::{Value as Json, json};
use ureq::Agent;
use ureq::http::{self, Method, Response};

use crate::store::Account;

/// Largest file downloaded or uploaded.
pub const MAX_FILE: u64 = 1 << 30;
const MAX_TEXT: u64 = 32 << 20;

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    Net(String),
    Status(u16, String),
    Bad(String),
}

impl Fail {
    pub fn text(&self) -> String {
        match self {
            Fail::Net(m) => format!("can't reach the server ({m})"),
            Fail::Status(401, m) => format!("the server refused the sign-in (401){}", if m.is_empty() { String::new() } else { format!(": {m}") }),
            Fail::Status(c, m) if m.is_empty() => format!("the server answered {c}"),
            Fail::Status(c, m) => format!("the server answered {c}: {m}"),
            Fail::Bad(m) => format!("unexpected answer ({m})"),
        }
    }
}

fn agent(total: Duration) -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(total))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .allow_non_standard_methods(true)
        .user_agent(concat!("nextcloud-live/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

pub fn b64(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Percent-encodes a path segment.
pub fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn encode_path(p: &str) -> String {
    p.split('/').map(pct).collect::<Vec<_>>().join("/")
}

fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header(r: &Response<ureq::Body>, name: &str) -> String {
    r.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string()
}

fn body(r: &mut Response<ureq::Body>, limit: u64) -> Result<Vec<u8>, Fail> {
    r.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| Fail::Net(e.to_string()))
}

fn reason(r: &mut Response<ureq::Body>) -> String {
    let b = body(r, 64 << 10).unwrap_or_default();
    if let Ok(j) = serde_json::from_slice::<Json>(&b) {
        if let Some(m) = j.get("error").and_then(Json::as_str) {
            return m.chars().take(300).collect();
        }
        if let Some(m) = j.pointer("/ocs/meta/message").and_then(Json::as_str) {
            return m.chars().take(300).collect();
        }
    }
    String::new()
}

fn run<B: ureq::AsSendBody>(a: &Agent, req: Result<http::Request<B>, http::Error>) -> Result<Response<ureq::Body>, Fail> {
    a.run(req.map_err(|e| Fail::Bad(e.to_string()))?).map_err(|e| Fail::Net(e.to_string()))
}

/// `cloud.example.com/` → `https://cloud.example.com`.
pub fn normalize_server(input: &str) -> Result<String, String> {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return Err("type the server's address".into());
    }
    if s.len() > 2048 || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("that address isn't valid".into());
    }
    if !s.contains("://") {
        s = format!("https://{s}");
    }
    let lower = s.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err("the address must start with https://".into());
    }
    if let Some(q) = s.find(['?', '#']) {
        s.truncate(q);
    }
    for tail in ["/", "/index.php", "/login", "/apps/files", "/"] {
        while s.to_ascii_lowercase().ends_with(tail) {
            s.truncate(s.len() - tail.len());
        }
    }
    let host = s.split("://").nth(1).unwrap_or("");
    if host.is_empty() || host.starts_with('/') || host.contains('@') {
        return Err("the address has no server name".into());
    }
    Ok(s)
}

/// A Nextcloud file link: `https://cloud.example/f/123` or `…/index.php/f/123` →
/// `(server, file id)`.
pub fn parse_file_link(link: &str) -> Option<(String, String)> {
    let l = link.trim();
    let (before, id) = l.rsplit_once("/f/")?;
    let id: String = id.chars().take_while(char::is_ascii_digit).collect();
    if id.is_empty() {
        return None;
    }
    let server = normalize_server(before.trim_end_matches("/index.php")).ok()?;
    Some((server, id))
}

/// One WebDAV item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// The path in the user's files (`/Documents/Budget.xlsx`).
    pub path: String,
    pub name: String,
    pub file_id: String,
    pub permissions: String,
    pub etag: String,
    pub size: Option<u64>,
    pub folder: bool,
}

impl Item {
    pub fn writable(&self) -> bool {
        self.permissions.is_empty() || self.permissions.contains('W')
    }
}

/// Parses a multistatus answer; paths are made relative to `/remote.php/dav/files/<user>`.
pub fn parse_multistatus(xml: &str, user: &str) -> Result<Vec<Item>, String> {
    let mut r = quick_xml::Reader::from_str(xml);
    let mut items = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut item = Item::default();
    let mut href = String::new();
    let mut props = Item::default();
    let mut ok = true;
    let prefix = format!("/remote.php/dav/files/{user}");
    loop {
        match r.read_event().map_err(|e| format!("bad XML: {e}"))? {
            Event::Start(e) => {
                if stack.len() > 64 {
                    return Err("XML nested too deep".into());
                }
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "response" => {
                        item = Item::default();
                        href.clear();
                    }
                    "propstat" => {
                        props = Item::default();
                        ok = true;
                    }
                    "collection" => props.folder = true,
                    _ => {}
                }
                stack.push(name);
                text.clear();
            }
            Event::Empty(e) if e.local_name().as_ref() == b"collection" => props.folder = true,
            Event::Text(t) => text.push_str(&t.decode().map_err(|e| e.to_string())?),
            Event::GeneralRef(g) => {
                let n = g.decode().map_err(|e| e.to_string())?;
                text.push_str(match n.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                });
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                let parent = stack.last().map(String::as_str).unwrap_or("");
                let v = text.trim().to_string();
                match (name.as_str(), parent) {
                    ("href", "response") => href = v,
                    ("status", "propstat") => ok = v.split_whitespace().nth(1).is_some_and(|c| c.starts_with('2')),
                    ("displayname", "prop") => props.name = v.chars().take(512).collect(),
                    ("fileid", "prop") => props.file_id = v.chars().take(64).collect(),
                    ("permissions", "prop") => props.permissions = v.chars().take(32).collect(),
                    ("getetag", "prop") => props.etag = v.chars().take(256).collect(),
                    ("getcontentlength", "prop") => props.size = v.parse().ok(),
                    ("propstat", _) if ok => {
                        if !props.name.is_empty() {
                            item.name = std::mem::take(&mut props.name);
                        }
                        if !props.file_id.is_empty() {
                            item.file_id = std::mem::take(&mut props.file_id);
                        }
                        if !props.permissions.is_empty() {
                            item.permissions = std::mem::take(&mut props.permissions);
                        }
                        if !props.etag.is_empty() {
                            item.etag = std::mem::take(&mut props.etag);
                        }
                        item.size = item.size.or(props.size);
                        item.folder |= props.folder;
                    }
                    ("response", _) => {
                        let h = unpct(&href);
                        let path = h.strip_prefix(&prefix).unwrap_or(&h).trim_end_matches('/').to_string();
                        item.path = if path.is_empty() { "/".into() } else { path };
                        if item.name.is_empty() {
                            item.name = item.path.rsplit('/').next().unwrap_or("").to_string();
                        }
                        items.push(std::mem::take(&mut item));
                        if items.len() > 10_000 {
                            break;
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(items)
}

/// A client for one account.
pub struct Nc {
    agent: Agent,
    server: String,
    user: String,
    auth: String,
}

impl Nc {
    pub fn new(acc: &Account, total: Duration) -> Nc {
        Nc {
            agent: agent(total),
            server: acc.server.trim_end_matches('/').to_string(),
            user: acc.dav_user().to_string(),
            auth: format!("Basic {}", b64(format!("{}:{}", acc.login_name, acc.app_password).as_bytes())),
        }
    }

    fn dav_url(&self, remote: &str) -> String {
        format!("{}/remote.php/dav/files/{}{}", self.server, pct(&self.user), encode_path(remote))
    }

    fn req(&self, method: &str, url: &str) -> Result<http::request::Builder, Fail> {
        let m = Method::from_bytes(method.as_bytes()).map_err(|_| Fail::Bad("method".into()))?;
        Ok(http::Request::builder().method(m).uri(url).header("Authorization", &self.auth))
    }

    /// `PROPFIND` depth 0 or 1.
    pub fn propfind(&self, remote: &str, depth: u8) -> Result<Vec<Item>, Fail> {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:displayname/><d:getcontentlength/><d:getetag/><d:resourcetype/><oc:fileid/><oc:permissions/></d:prop></d:propfind>"#;
        let mut url = self.dav_url(remote);
        if depth > 0 && !url.ends_with('/') {
            url.push('/');
        }
        let req =
            self.req("PROPFIND", &url)?.header("Depth", if depth == 0 { "0" } else { "1" }).header("Content-Type", "application/xml; charset=utf-8").body(xml);
        let mut r = run(&self.agent, req)?;
        let status = r.status().as_u16();
        if status != 207 {
            return Err(Fail::Status(status, reason(&mut r)));
        }
        let b = body(&mut r, MAX_TEXT)?;
        parse_multistatus(&String::from_utf8_lossy(&b), &self.user).map_err(Fail::Bad)
    }

    pub fn stat(&self, remote: &str) -> Result<Item, Fail> {
        self.propfind(remote, 0)?.into_iter().next().ok_or_else(|| Fail::Bad("empty listing".into()))
    }

    /// The file with this id (WebDAV `SEARCH`).
    pub fn find_by_id(&self, file_id: &str) -> Result<Item, Fail> {
        if file_id.is_empty() || !file_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Fail::Bad("bad file id".into()));
        }
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:basicsearch>
<d:select><d:prop><d:displayname/><d:getetag/><d:resourcetype/><oc:fileid/><oc:permissions/></d:prop></d:select>
<d:from><d:scope><d:href>/files/{}</d:href><d:depth>infinity</d:depth></d:scope></d:from>
<d:where><d:eq><d:prop><oc:fileid/></d:prop><d:literal>{file_id}</d:literal></d:eq></d:where>
<d:orderby/></d:basicsearch></d:searchrequest>"#,
            pct(&self.user)
        );
        let req = self.req("SEARCH", &format!("{}/remote.php/dav/", self.server))?.header("Content-Type", "text/xml; charset=utf-8").body(xml);
        let mut r = run(&self.agent, req)?;
        let status = r.status().as_u16();
        if status != 207 {
            return Err(Fail::Status(status, reason(&mut r)));
        }
        let b = body(&mut r, MAX_TEXT)?;
        let items = parse_multistatus(&String::from_utf8_lossy(&b), &self.user).map_err(Fail::Bad)?;
        items.into_iter().find(|i| i.file_id.trim_start_matches('0') == file_id.trim_start_matches('0')).ok_or_else(|| Fail::Status(404, "no such file".into()))
    }

    /// Downloads a file to `dest`; its etag.
    pub fn download(&self, remote: &str, dest: &Path) -> Result<String, Fail> {
        let req = self.req("GET", &self.dav_url(remote))?.body(());
        let mut r = run(&self.agent, req)?;
        let status = r.status().as_u16();
        if status != 200 {
            return Err(Fail::Status(status, reason(&mut r)));
        }
        let etag = {
            let oc = header(&r, "oc-etag");
            if oc.is_empty() { header(&r, "etag") } else { oc }
        };
        let bytes = body(&mut r, MAX_FILE)?;
        let dir = dest.parent().ok_or_else(|| Fail::Bad("no folder".into()))?;
        std::fs::create_dir_all(dir).map_err(|e| Fail::Bad(format!("can't create {}: {e}", dir.display())))?;
        crate::store::write_private(dest, &bytes).map_err(Fail::Bad)?;
        Ok(etag)
    }

    /// Uploads; `if_match`: only over this version (412 otherwise); `create`: only if absent.
    /// The new etag.
    pub fn upload(&self, remote: &str, data: Vec<u8>, if_match: Option<&str>, create: bool) -> Result<String, Fail> {
        let mut b = self.req("PUT", &self.dav_url(remote))?.header("Content-Type", "application/octet-stream");
        if let Some(e) = if_match.filter(|e| !e.is_empty()) {
            let e = e.trim().trim_start_matches("W/");
            b = b.header("If-Match", if e.starts_with('"') { e.to_string() } else { format!("\"{e}\"") });
        }
        if create {
            b = b.header("If-None-Match", "*");
        }
        let mut r = run(&self.agent, b.body(data))?;
        let status = r.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Fail::Status(status, reason(&mut r)));
        }
        let oc = header(&r, "oc-etag");
        Ok(if oc.is_empty() { header(&r, "etag") } else { oc })
    }

    fn ocs(&self, path: &str) -> Result<Json, Fail> {
        let url = format!("{}/ocs/v2.php/{path}?format=json", self.server);
        let req = self.req("GET", &url)?.header("OCS-APIRequest", "true").header("Accept", "application/json").body(());
        let mut r = run(&self.agent, req)?;
        let status = r.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Fail::Status(status, reason(&mut r)));
        }
        serde_json::from_slice(&body(&mut r, MAX_TEXT)?).map_err(|_| Fail::Bad("not JSON".into()))
    }

    pub fn user_id(&self) -> Result<String, Fail> {
        let j = self.ocs("cloud/user")?;
        j.pointer("/ocs/data/id")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty() && s.len() < 256)
            .map(str::to_string)
            .ok_or_else(|| Fail::Bad("no user id".into()))
    }

    /// The Office server Nextcloud uses (`richdocuments` capabilities), when it says.
    pub fn office_server(&self) -> Option<String> {
        let j = self.ocs("cloud/capabilities").ok()?;
        let rd = j.pointer("/ocs/data/capabilities/richdocuments")?;
        ["/config/wopi_url", "/config/public_wopi_url", "/wopi_url"]
            .iter()
            .find_map(|p| rd.pointer(p).and_then(Json::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

/// Login Flow v2, step 1: `(login page, poll token, poll endpoint)`.
pub fn login_start(server: &str) -> Result<(String, String, String), Fail> {
    let a = agent(Duration::from_secs(30));
    let req = http::Request::builder().method(Method::POST).uri(format!("{server}/index.php/login/v2")).body(());
    let mut r = run(&a, req)?;
    let status = r.status().as_u16();
    if status != 200 {
        return Err(Fail::Status(status, reason(&mut r)));
    }
    let j: Json = serde_json::from_slice(&body(&mut r, 1 << 20)?).map_err(|_| Fail::Bad("not a Nextcloud server".into()))?;
    let get = |p: &str| j.pointer(p).and_then(Json::as_str).filter(|s| !s.is_empty() && s.len() < 4096).map(str::to_string);
    let login = get("/login").ok_or_else(|| Fail::Bad("not a Nextcloud server".into()))?;
    let token = get("/poll/token").ok_or_else(|| Fail::Bad("no poll token".into()))?;
    let endpoint = get("/poll/endpoint").ok_or_else(|| Fail::Bad("no poll endpoint".into()))?;
    for u in [&login, &endpoint] {
        let l = u.to_ascii_lowercase();
        if !(l.starts_with("https://") || l.starts_with("http://")) {
            return Err(Fail::Bad("bad address in the answer".into()));
        }
    }
    Ok((login, token, endpoint))
}

/// Login Flow v2, step 2: `Some((server, login name, app password))` once signed in.
pub fn login_poll(endpoint: &str, token: &str) -> Result<Option<(String, String, String)>, Fail> {
    let a = agent(Duration::from_secs(30));
    let req = http::Request::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("token={}", pct(token)));
    let mut r = run(&a, req)?;
    let status = r.status().as_u16();
    if status == 404 {
        return Ok(None);
    }
    if status != 200 {
        return Err(Fail::Status(status, reason(&mut r)));
    }
    let j: Json = serde_json::from_slice(&body(&mut r, 1 << 20)?).map_err(|_| Fail::Bad("not JSON".into()))?;
    let get = |k: &str| j.get(k).and_then(Json::as_str).filter(|s| !s.is_empty() && s.len() < 4096).map(str::to_string);
    match (get("server"), get("loginName"), get("appPassword")) {
        (Some(s), Some(l), Some(p)) => Ok(Some((s, l, p))),
        _ => Err(Fail::Bad("incomplete sign-in answer".into())),
    }
}

/// craft-live's capabilities: its API address, when the server is craft-live.
pub fn live_api(base: &str) -> Option<String> {
    let a = agent(Duration::from_secs(15));
    let req = http::Request::builder().method(Method::GET).uri(format!("{}/hosting/capabilities", base.trim_end_matches('/'))).body(());
    let mut r = run(&a, req).ok()?;
    if r.status().as_u16() != 200 {
        return None;
    }
    let j: Json = serde_json::from_slice(&body(&mut r, 1 << 20).ok()?).ok()?;
    j.pointer("/craftLive/api").and_then(Json::as_str).map(str::to_string)
}

/// `POST <api>/sessions`: a desktop session for a file.
pub fn create_session(api: &str, acc: &Account, path: &str, file_id: &str) -> Result<Json, Fail> {
    let a = agent(Duration::from_secs(60));
    let auth = format!("Basic {}", b64(format!("{}:{}", acc.login_name, acc.app_password).as_bytes()));
    let req = http::Request::builder()
        .method(Method::POST)
        .uri(format!("{api}/sessions"))
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(json!({"nextcloud": acc.server, "path": path, "fileId": file_id}).to_string());
    let mut r = run(&a, req)?;
    let status = r.status().as_u16();
    if status != 200 {
        return Err(Fail::Status(status, reason(&mut r)));
    }
    serde_json::from_slice(&body(&mut r, 1 << 20)?).map_err(|_| Fail::Bad("not JSON".into()))
}

/// `PUT <url>`: the file bytes for a save craft-live asked for.
pub fn upload_save(url: &str, token: &str, name: &str, data: Vec<u8>) -> Result<Json, Fail> {
    let a = agent(Duration::from_secs(300));
    let req = http::Request::builder()
        .method(Method::PUT)
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Craft-File-Name", pct(name))
        .header("Content-Type", "application/octet-stream")
        .body(data);
    let mut r = run(&a, req)?;
    let status = r.status().as_u16();
    if status != 200 {
        return Err(Fail::Status(status, reason(&mut r)));
    }
    serde_json::from_slice(&body(&mut r, 1 << 20)?).map_err(|_| Fail::Bad("not JSON".into()))
}

/// Reads at most `limit` bytes of a local file.
pub fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(limit + 1).read_to_end(&mut out)).map_err(|e| format!("{}: {e}", path.display()))?;
    if out.len() as u64 > limit {
        return Err(format!("{} is too large", path.display()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_servers() {
        assert_eq!(parse_file_link("https://cloud.example/f/288"), Some(("https://cloud.example".into(), "288".into())));
        assert_eq!(parse_file_link("https://cloud.example/nc/index.php/f/12?x"), Some(("https://cloud.example/nc".into(), "12".into())));
        assert_eq!(parse_file_link("https://cloud.example/x"), None);
        assert_eq!(normalize_server("cloud.example/index.php/").unwrap(), "https://cloud.example");
        assert!(normalize_server("ftp://x").is_err());
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:response><d:href>/remote.php/dav/files/ada/My%20Docs/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><oc:fileid>5</oc:fileid></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/remote.php/dav/files/ada/My%20Docs/a.xlsx</d:href><d:propstat><d:prop><oc:fileid>6</oc:fileid><oc:permissions>RW</oc:permissions><d:getetag>"e"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let items = parse_multistatus(xml, "ada").unwrap();
        assert_eq!(items[0].path, "/My Docs");
        assert!(items[0].folder);
        assert_eq!(items[1].path, "/My Docs/a.xlsx");
        assert_eq!(items[1].name, "a.xlsx");
        assert!(items[1].writable());
    }
}
