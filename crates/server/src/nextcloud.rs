//! Desktop sessions: the nextcloud-live add-in proves who the person is and that they may open
//! the file, with the app password it signed in with (Login Flow v2). craft-live asks Nextcloud
//! itself, once, and never stores the password (docs/design.md, "Desktop clients"):
//!
//! - `GET <base>/ocs/v2.php/cloud/user` → the user id and display name;
//! - `PROPFIND <base>/remote.php/dav/files/<user>/<path>` (depth 0) → `oc:fileid` must be the
//!   file id the client named, and `oc:permissions` says whether the person may write (`W`).

use std::time::Duration;

use quick_xml::events::Event;
use serde_json::Value as Json;

use crate::ids;

/// What Nextcloud said about the person and the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub user_id: String,
    pub display_name: String,
    pub file_id: String,
    pub name: String,
    pub can_write: bool,
    pub etag: String,
}

/// Why the check failed: `(HTTP status to answer, message)`.
pub type Refusal = (u16, String);

/// One WebDAV item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DavItem {
    pub href: String,
    pub file_id: String,
    pub permissions: String,
    pub name: String,
    pub etag: String,
    pub folder: bool,
}

/// Parses a multistatus answer (properties with a 2xx status only).
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavItem>, String> {
    let mut r = quick_xml::Reader::from_str(xml);
    let mut items = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut item = DavItem::default();
    let mut props = DavItem::default();
    let mut ok = true;
    loop {
        match r.read_event().map_err(|e| format!("bad XML: {e}"))? {
            Event::Start(e) => {
                if stack.len() > 64 {
                    return Err("XML nested too deep".into());
                }
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "response" => item = DavItem::default(),
                    "propstat" => {
                        props = DavItem::default();
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
                let name = g.decode().map_err(|e| e.to_string())?;
                text.push_str(match name.as_ref() {
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
                let value = text.trim().to_string();
                match (name.as_str(), parent) {
                    ("href", "response") => item.href = ids::clean(&value, 4096),
                    ("status", "propstat") => ok = value.split_whitespace().nth(1).is_some_and(|c| c.starts_with('2')),
                    ("fileid", "prop") => props.file_id = ids::clean(&value, 64),
                    ("permissions", "prop") => props.permissions = ids::clean(&value, 32),
                    ("displayname", "prop") => props.name = ids::clean(&value, 512),
                    ("getetag", "prop") => props.etag = ids::clean(&value, 256),
                    ("propstat", _) if ok => {
                        for (dst, src) in [
                            (&mut item.file_id, &props.file_id),
                            (&mut item.permissions, &props.permissions),
                            (&mut item.name, &props.name),
                            (&mut item.etag, &props.etag),
                        ] {
                            if !src.is_empty() {
                                *dst = src.clone();
                            }
                        }
                        item.folder |= props.folder;
                    }
                    ("response", _) => {
                        items.push(std::mem::take(&mut item));
                        if items.len() > 10_000 {
                            return Err("too many items".into());
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

/// Percent-encodes each segment of a remote path (`/Documents/My file.xlsx`).
pub fn encode_path(path: &str) -> String {
    path.split('/').map(ids::pct).collect::<Vec<_>>().join("/")
}

/// A remote path: starts with `/`, no `..`, no control characters, at most 4096 bytes.
pub fn valid_path(path: &str) -> bool {
    path.starts_with('/') && path.len() <= 4096 && !path.split('/').any(|s| s == ".." || s == ".") && !path.chars().any(char::is_control)
}

/// Asks Nextcloud who the credentials belong to and whether they reach the file.
pub async fn verify(base: &str, login: &str, password: &str, path: &str, file_id: &str, verify_tls: bool) -> Result<Identity, Refusal> {
    if !valid_path(path) || file_id.is_empty() || file_id.len() > 64 || !file_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err((400, "a file path and a file id are required".into()));
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(!verify_tls)
        .user_agent(concat!("craft-live/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| (500, e.to_string()))?;
    let net = |e: reqwest::Error| (502, format!("can't reach Nextcloud: {e}"));
    let r = http
        .get(format!("{base}/ocs/v2.php/cloud/user?format=json"))
        .basic_auth(login, Some(password))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(net)?;
    match r.status().as_u16() {
        200 => {}
        401 | 403 => return Err((401, "Nextcloud refused the app password".into())),
        s => return Err((502, format!("Nextcloud answered {s} for the user"))),
    }
    let body = r.bytes().await.map_err(net)?;
    if body.len() > 1 << 20 {
        return Err((502, "Nextcloud's answer is too large".into()));
    }
    let j: Json = serde_json::from_slice(&body).map_err(|_| (502, "Nextcloud's user answer isn't JSON".to_string()))?;
    let user_id = j.pointer("/ocs/data/id").and_then(Json::as_str).map(|s| ids::clean(s, 128)).unwrap_or_default();
    if user_id.is_empty() {
        return Err((502, "Nextcloud didn't say who the user is".into()));
    }
    let display_name = ["/ocs/data/displayname", "/ocs/data/display-name"]
        .iter()
        .find_map(|p| j.pointer(p).and_then(Json::as_str))
        .map(|s| ids::clean(s, 128))
        .unwrap_or_else(|| user_id.clone());
    let url = format!("{base}/remote.php/dav/files/{}{}", ids::pct(&user_id), encode_path(path));
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:displayname/><d:getetag/><d:resourcetype/><oc:fileid/><oc:permissions/></d:prop></d:propfind>"#;
    let method = reqwest::Method::from_bytes(b"PROPFIND").map_err(|e| (500, e.to_string()))?;
    let r = http
        .request(method, url)
        .basic_auth(login, Some(password))
        .header("Depth", "0")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(net)?;
    match r.status().as_u16() {
        207 => {}
        401 | 403 => return Err((403, "Nextcloud refused access to the file".into())),
        404 => return Err((404, "the file isn't in this person's Nextcloud".into())),
        s => return Err((502, format!("Nextcloud answered {s} for the file"))),
    }
    let body = r.bytes().await.map_err(net)?;
    if body.len() > 4 << 20 {
        return Err((502, "Nextcloud's answer is too large".into()));
    }
    let items = parse_multistatus(&String::from_utf8_lossy(&body)).map_err(|e| (502, e))?;
    let item = items.into_iter().next().ok_or((502, "Nextcloud's answer is empty".to_string()))?;
    if item.folder {
        return Err((400, "that's a folder".into()));
    }
    if item.file_id.trim_start_matches('0') != file_id.trim_start_matches('0') {
        return Err((403, "the path and the file id don't match".into()));
    }
    let name = if item.name.is_empty() { path.rsplit('/').next().unwrap_or("").to_string() } else { item.name };
    Ok(Identity { user_id, display_name, file_id: item.file_id, name, can_write: item.permissions.contains('W'), etag: item.etag })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_propfind() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
 <d:response><d:href>/remote.php/dav/files/ada/Budget%20Q1.xlsx</d:href>
  <d:propstat><d:prop><d:displayname>Budget Q1.xlsx</d:displayname><oc:fileid>288</oc:fileid><oc:permissions>RGDNVW</oc:permissions><d:getetag>"abc"</d:getetag><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><oc:missing/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].file_id, "288");
        assert!(items[0].permissions.contains('W'));
        assert_eq!(items[0].name, "Budget Q1.xlsx");
        assert!(!items[0].folder);
        assert!(parse_multistatus("<a><b></a>").is_err());
        assert_eq!(encode_path("/My Docs/a+b.xlsx"), "/My%20Docs/a%2Bb.xlsx");
        assert!(valid_path("/a/b.xlsx"));
        assert!(!valid_path("a"));
        assert!(!valid_path("/a/../b"));
    }
}
