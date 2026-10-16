//! The WOPI client: craft-live's calls to the WOPI host (Nextcloud's richdocuments endpoint).
//!
//! Every request carries the access token twice (the `access_token` query parameter and
//! `Authorization: Bearer`), the proof headers when proof keys are on, and no redirect is
//! followed. Answers are read up to a limit. Lock ids are ASCII, at most 1024 characters.
//! Operations, headers and status codes follow Microsoft's WOPI REST documentation
//! (docs/design.md lists the pages).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use reqwest::{Method, StatusCode};
use serde_json::Value as Json;

use crate::ids;
use crate::proof::ProofKeys;

/// Largest CheckFileInfo or PutRelativeFile answer read.
const MAX_JSON: usize = 1 << 20;

/// Why a WOPI call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WopiError {
    /// 401: the token is invalid or expired.
    Unauthorized,
    /// 404: no such file, or the user may not see it.
    NotFound,
    /// 409: the lock didn't match; the host's current lock (empty: unlocked) and its reason.
    Conflict {
        lock: String,
        reason: String,
    },
    /// 412 (GetFile: larger than allowed) or 413 (PutFile: too large for the host).
    TooLarge,
    /// 501: the host doesn't do this.
    NotImplemented,
    Status(u16, String),
    /// The host couldn't be reached.
    Net(String),
    /// The answer made no sense.
    Bad(String),
}

impl std::fmt::Display for WopiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WopiError::Unauthorized => write!(f, "the WOPI host refused the access token (401)"),
            WopiError::NotFound => write!(f, "the WOPI host doesn't know the file (404)"),
            WopiError::Conflict { lock, reason } if lock.is_empty() => write!(f, "lock mismatch (409, the file isn't locked) {reason}"),
            WopiError::Conflict { reason, .. } => write!(f, "the file is locked by someone else (409) {reason}"),
            WopiError::TooLarge => write!(f, "the file is too large"),
            WopiError::NotImplemented => write!(f, "the WOPI host doesn't support this (501)"),
            WopiError::Status(c, m) => write!(f, "the WOPI host answered {c} {m}"),
            WopiError::Net(m) => write!(f, "can't reach the WOPI host: {m}"),
            WopiError::Bad(m) => write!(f, "unexpected answer from the WOPI host: {m}"),
        }
    }
}

/// What CheckFileInfo says (the properties craft-live uses; omitted booleans are false and
/// omitted strings empty, as the WOPI documentation says).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileInfo {
    pub base_file_name: String,
    pub owner_id: String,
    pub size: u64,
    pub user_id: String,
    pub version: String,
    pub user_friendly_name: String,
    pub user_can_write: bool,
    pub read_only: bool,
    pub user_can_not_write_relative: bool,
    pub supports_locks: bool,
    pub supports_get_lock: bool,
    pub supports_update: bool,
    pub supports_extended_lock_length: bool,
    pub is_anonymous_user: bool,
    pub post_message_origin: String,
    pub close_url: String,
    pub host_edit_url: String,
    pub last_modified_time: String,
}

impl FileInfo {
    pub fn from_json(j: &Json) -> Result<FileInfo, WopiError> {
        if !j.is_object() {
            return Err(WopiError::Bad("CheckFileInfo isn't a JSON object".into()));
        }
        let s = |k: &str, n: usize| match j.get(k) {
            Some(Json::String(v)) => ids::clean(v, n),
            Some(Json::Number(v)) => v.to_string(),
            _ => String::new(),
        };
        let b = |k: &str| j.get(k).and_then(Json::as_bool).unwrap_or(false);
        let info = FileInfo {
            base_file_name: s("BaseFileName", 512),
            owner_id: s("OwnerId", 256),
            size: j.get("Size").and_then(Json::as_u64).unwrap_or(0),
            user_id: s("UserId", 256),
            version: s("Version", 256),
            user_friendly_name: s("UserFriendlyName", 256),
            user_can_write: b("UserCanWrite"),
            read_only: b("ReadOnly"),
            user_can_not_write_relative: b("UserCanNotWriteRelative"),
            supports_locks: b("SupportsLocks"),
            supports_get_lock: b("SupportsGetLock"),
            supports_update: b("SupportsUpdate"),
            supports_extended_lock_length: b("SupportsExtendedLockLength"),
            is_anonymous_user: b("IsAnonymousUser"),
            post_message_origin: s("PostMessageOrigin", 512),
            close_url: s("CloseUrl", 2048),
            host_edit_url: s("HostEditUrl", 2048),
            last_modified_time: s("LastModifiedTime", 64),
        };
        if info.base_file_name.is_empty() {
            return Err(WopiError::Bad("CheckFileInfo has no BaseFileName".into()));
        }
        Ok(info)
    }

    /// Whether this user may change the file.
    pub fn can_edit(&self) -> bool {
        self.user_can_write && !self.read_only
    }
}

/// What PutRelativeFile created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelativeFile {
    pub name: String,
    pub url: String,
}

/// A WOPI client.
#[derive(Clone)]
pub struct Wopi {
    http: reqwest::Client,
    proof: Option<Arc<ProofKeys>>,
}

/// A lock id: ASCII without control characters, 1–1024 characters.
pub fn valid_lock(lock: &str) -> bool {
    (1..=1024).contains(&lock.len()) && lock.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// UTF-7 (RFC 2152), as `X-WOPI-SuggestedTarget` and `X-WOPI-RelativeTarget` want: characters
/// outside the direct set become `+<modified base64 of UTF-16BE>-`.
pub fn utf7(s: &str) -> String {
    let direct = |c: char| c.is_ascii_alphanumeric() || " '(),-./:?!\"#$%&*;<=>@[]^_`{|}".contains(c);
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |out: &mut String, pending: &mut Vec<u16>| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
        out.push('+');
        out.push_str(&base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes));
        out.push('-');
        pending.clear();
    };
    for c in s.chars() {
        if c == '+' {
            flush(&mut out, &mut pending);
            out.push_str("+-");
        } else if direct(c) {
            flush(&mut out, &mut pending);
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut out, &mut pending);
    out
}

fn header(r: &reqwest::Response, name: &str) -> String {
    r.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| ids::clean(s, 1024)).unwrap_or_default()
}

/// Reads a body up to `limit` bytes.
async fn read_body(mut r: reqwest::Response, limit: u64) -> Result<Vec<u8>, WopiError> {
    if r.content_length().is_some_and(|n| n > limit) {
        return Err(WopiError::TooLarge);
    }
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| WopiError::Net(e.to_string()))? {
        if (out.len() + chunk.len()) as u64 > limit {
            return Err(WopiError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The usual errors from a status.
async fn status_error(r: reqwest::Response) -> WopiError {
    let status = r.status().as_u16();
    match status {
        401 => WopiError::Unauthorized,
        404 => WopiError::NotFound,
        409 => WopiError::Conflict { lock: header(&r, "x-wopi-lock"), reason: header(&r, "x-wopi-lockfailurereason") },
        412 | 413 => WopiError::TooLarge,
        501 => WopiError::NotImplemented,
        _ => {
            let reason = header(&r, "x-wopi-servererror");
            WopiError::Status(status, reason)
        }
    }
}

impl Wopi {
    /// `verify_tls: false` accepts any certificate (test setups only).
    pub fn new(proof: Option<Arc<ProofKeys>>, verify_tls: bool, timeout: Duration) -> Result<Wopi, String> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .danger_accept_invalid_certs(!verify_tls)
            .user_agent(concat!("craft-live/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Wopi { http, proof })
    }

    /// `<src><suffix>?access_token=…`.
    fn url(src: &str, suffix: &str, token: &str) -> String {
        let sep = if src.contains('?') { '&' } else { '?' };
        format!("{src}{suffix}{sep}access_token={}", ids::pct(token))
    }

    fn request(&self, method: Method, url: &str, token: &str) -> reqwest::RequestBuilder {
        let mut b = self.http.request(method, url).header("Authorization", format!("Bearer {token}")).header("X-Request-ID", ids::token(12));
        if let Some(p) = &self.proof {
            let h = p.headers(token, url);
            b = b.header("X-WOPI-Proof", h.proof).header("X-WOPI-ProofOld", h.proof_old).header("X-WOPI-TimeStamp", h.timestamp);
        }
        b
    }

    async fn send(&self, b: reqwest::RequestBuilder) -> Result<reqwest::Response, WopiError> {
        b.send().await.map_err(|e| WopiError::Net(e.to_string()))
    }

    /// CheckFileInfo.
    pub async fn check_file_info(&self, src: &str, token: &str) -> Result<FileInfo, WopiError> {
        let url = Wopi::url(src, "", token);
        let r = self.send(self.request(Method::GET, &url, token)).await?;
        if r.status() != StatusCode::OK {
            return Err(status_error(r).await);
        }
        let body = read_body(r, MAX_JSON as u64).await?;
        let j: Json = serde_json::from_slice(&body).map_err(|_| WopiError::Bad("CheckFileInfo isn't JSON".into()))?;
        FileInfo::from_json(&j)
    }

    /// GetFile: the bytes and `X-WOPI-ItemVersion`.
    pub async fn get_file(&self, src: &str, token: &str, max: u64) -> Result<(Vec<u8>, String), WopiError> {
        let url = Wopi::url(src, "/contents", token);
        let r = self.send(self.request(Method::GET, &url, token).header("X-WOPI-MaxExpectedSize", max.min(i32::MAX as u64).to_string())).await?;
        if r.status() != StatusCode::OK {
            return Err(status_error(r).await);
        }
        let version = header(&r, "x-wopi-itemversion");
        Ok((read_body(r, max).await?, version))
    }

    /// PutFile: the new `X-WOPI-ItemVersion` (empty when the host didn't say). `autosave` goes
    /// in the Collabora-style `X-COOL-WOPI-IsAutosave` header (hosts may keep explicit saves as
    /// versions); `X-COOL-WOPI-IsModifiedByUser` is always true (craft-live only saves edits).
    pub async fn put_file(&self, src: &str, token: &str, lock: Option<&str>, editors: &str, autosave: bool, body: Vec<u8>) -> Result<String, WopiError> {
        let url = Wopi::url(src, "/contents", token);
        let mut b = self
            .request(Method::POST, &url, token)
            .header("X-WOPI-Override", "PUT")
            .header("Content-Type", "application/octet-stream")
            .header("X-COOL-WOPI-IsAutosave", if autosave { "true" } else { "false" })
            .header("X-COOL-WOPI-IsModifiedByUser", "true");
        if let Some(l) = lock.filter(|l| valid_lock(l)) {
            b = b.header("X-WOPI-Lock", l);
        }
        let editors = ids::clean(editors, 1024);
        if !editors.is_empty() && editors.is_ascii() {
            b = b.header("X-WOPI-Editors", editors);
        }
        let r = self.send(b.body(body)).await?;
        if r.status() != StatusCode::OK {
            return Err(status_error(r).await);
        }
        Ok(header(&r, "x-wopi-itemversion"))
    }

    async fn lock_op(&self, src: &str, token: &str, op: &str, lock: Option<&str>, old: Option<&str>) -> Result<String, WopiError> {
        let url = Wopi::url(src, "", token);
        let mut b = self.request(Method::POST, &url, token).header("X-WOPI-Override", op).header("Content-Length", "0");
        if let Some(l) = lock {
            if !valid_lock(l) {
                return Err(WopiError::Bad("bad lock id".into()));
            }
            b = b.header("X-WOPI-Lock", l);
        }
        if let Some(o) = old {
            b = b.header("X-WOPI-OldLock", o);
        }
        let r = self.send(b).await?;
        if r.status() != StatusCode::OK {
            return Err(status_error(r).await);
        }
        Ok(header(&r, "x-wopi-lock"))
    }

    /// Lock (also refreshes a lock it already holds with the same id).
    pub async fn lock(&self, src: &str, token: &str, lock: &str) -> Result<(), WopiError> {
        self.lock_op(src, token, "LOCK", Some(lock), None).await.map(|_| ())
    }

    pub async fn refresh_lock(&self, src: &str, token: &str, lock: &str) -> Result<(), WopiError> {
        self.lock_op(src, token, "REFRESH_LOCK", Some(lock), None).await.map(|_| ())
    }

    pub async fn unlock(&self, src: &str, token: &str, lock: &str) -> Result<(), WopiError> {
        self.lock_op(src, token, "UNLOCK", Some(lock), None).await.map(|_| ())
    }

    /// The current lock (empty: unlocked).
    pub async fn get_lock(&self, src: &str, token: &str) -> Result<String, WopiError> {
        self.lock_op(src, token, "GET_LOCK", None, None).await
    }

    pub async fn unlock_and_relock(&self, src: &str, token: &str, old: &str, new: &str) -> Result<(), WopiError> {
        if !valid_lock(old) {
            return Err(WopiError::Bad("bad lock id".into()));
        }
        self.lock_op(src, token, "LOCK", Some(new), Some(old)).await.map(|_| ())
    }

    /// PutRelativeFile in suggested mode (the host picks a free name near `suggested`).
    pub async fn put_relative_file(&self, src: &str, token: &str, suggested: &str, body: Vec<u8>) -> Result<RelativeFile, WopiError> {
        let url = Wopi::url(src, "", token);
        let b = self
            .request(Method::POST, &url, token)
            .header("X-WOPI-Override", "PUT_RELATIVE")
            .header("X-WOPI-SuggestedTarget", utf7(suggested))
            .header("X-WOPI-Size", body.len().to_string())
            .header("Content-Type", "application/octet-stream");
        let r = self.send(b.body(body)).await?;
        if r.status() != StatusCode::OK {
            return Err(status_error(r).await);
        }
        let j: Json = serde_json::from_slice(&read_body(r, MAX_JSON as u64).await?).map_err(|_| WopiError::Bad("PutRelativeFile answer isn't JSON".into()))?;
        let name = j.get("Name").and_then(Json::as_str).map(|s| ids::clean(s, 512)).unwrap_or_default();
        let url = j.get("Url").and_then(Json::as_str).map(|s| ids::clean(s, 4096)).unwrap_or_default();
        Ok(RelativeFile { name, url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_info_defaults_and_refusals() {
        let i = FileInfo::from_json(&json!({"BaseFileName": "Budget.xlsx", "Size": 12, "UserCanWrite": true, "Version": 3, "UserId": "ada"})).unwrap();
        assert_eq!(i.version, "3");
        assert!(i.can_edit());
        assert!(!i.supports_locks);
        assert!(FileInfo::from_json(&json!([1])).is_err());
        assert!(FileInfo::from_json(&json!({"Size": 1})).is_err());
        let ro = FileInfo::from_json(&json!({"BaseFileName": "a.docx", "UserCanWrite": true, "ReadOnly": true})).unwrap();
        assert!(!ro.can_edit());
    }

    #[test]
    fn utf7_and_locks() {
        assert_eq!(utf7("Budget.xlsx"), "Budget.xlsx");
        assert_eq!(utf7("Hi Mom -☺-!"), "Hi Mom -+Jjo--!");
        assert_eq!(utf7("a+b"), "a+-b");
        assert_eq!(utf7("città"), "citt+AOA-");
        assert!(valid_lock("abc"));
        assert!(!valid_lock(""));
        assert!(!valid_lock("é"));
        assert!(!valid_lock(&"a".repeat(1025)));
        assert_eq!(Wopi::url("https://h/wopi/files/1", "/contents", "a b"), "https://h/wopi/files/1/contents?access_token=a%20b");
    }
}
