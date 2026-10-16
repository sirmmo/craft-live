//! Sessions: one person in one room, behind a craft-live session token.
//!
//! A **web** session starts when Nextcloud Office opens the editor page with a WOPI access token
//! (CheckFileInfo says who the person is and whether they may write). A **desktop** session
//! starts when the nextcloud-live add-in proves the person's Nextcloud identity and their access
//! to the file (docs/design.md, "Desktop clients"). Either way the clients only ever hold the
//! craft-live token; the WOPI token stays on the server.
//!
//! Sessions are kept in `sessions.json` (owner-only), so a restart doesn't throw everyone out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ids;

/// Sessions held at most (the oldest go first).
const MAX_SESSIONS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Web,
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Edit,
    View,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Edit => "edit",
            Role::View => "view",
        }
    }
}

/// A person as the room shows them (protocol 2's `user`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub color: String,
}

impl User {
    /// From a WOPI or Nextcloud user id and display name: 1–128 characters each.
    pub fn new(id: &str, name: &str) -> User {
        let mut id = ids::clean(id, 128);
        if id.is_empty() {
            id = format!("guest-{}", ids::token(6));
        }
        let mut name = ids::clean(name.trim(), 128);
        if name.is_empty() {
            name = id.clone();
        }
        User { color: ids::color_for(&id).to_string(), id, name }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"id": self.id, "name": self.name, "color": self.color})
    }
}

/// One person in one room.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The craft-live session token (secret).
    pub token: String,
    pub room: String,
    pub kind: Kind,
    pub user: User,
    pub role: Role,
    /// `gridcraft`, `wordcraft` or `deckcraft`.
    pub app: String,
    /// The file's name.
    pub file_name: String,
    /// Web sessions: the WOPISrc and the WOPI access token with its expiry (ms since 1970; 0 =
    /// unknown).
    #[serde(default)]
    pub wopi_src: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub access_token_ttl: i64,
    /// Desktop sessions: the Nextcloud base URL and the file id.
    #[serde(default)]
    pub nextcloud: String,
    #[serde(default)]
    pub file_id: String,
    pub created_ms: i64,
    /// When the session ends (ms since 1970).
    pub expires_ms: i64,
    /// The page that embeds the editor (CheckFileInfo's PostMessageOrigin), for the shell.
    #[serde(default)]
    pub post_message_origin: String,
    #[serde(default)]
    pub close_url: String,
}

impl Session {
    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_ms
    }

    /// The WOPI token can still be used.
    pub fn wopi_usable(&self, now_ms: i64) -> bool {
        !self.access_token.is_empty() && !self.wopi_src.is_empty() && (self.access_token_ttl == 0 || now_ms < self.access_token_ttl)
    }
}

/// The session table.
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    file: Option<PathBuf>,
}

impl Sessions {
    /// Reads `sessions.json` from `dir` (when given), dropping the expired ones.
    pub fn open(dir: Option<PathBuf>) -> Sessions {
        let file = dir.map(|d| d.join("sessions.json"));
        let mut map = HashMap::new();
        if let Some(f) = &file
            && let Ok(Some(bytes)) = crate::store::read_limited(f, 256 << 20)
            && let Ok(list) = serde_json::from_slice::<Vec<Session>>(&bytes)
        {
            let now = ids::now_ms();
            for s in list.into_iter().filter(|s| !s.expired(now)) {
                map.insert(s.token.clone(), Arc::new(s));
            }
        }
        Sessions { inner: Arc::new(Mutex::new(map)), file }
    }

    fn save(&self, map: &HashMap<String, Arc<Session>>) {
        let Some(f) = &self.file else { return };
        let list: Vec<&Session> = map.values().map(|s| s.as_ref()).collect();
        match serde_json::to_vec(&list) {
            Ok(bytes) => {
                if let Err(e) = crate::store::write_private(f, &bytes) {
                    tracing::warn!("can't store the sessions: {e}");
                }
            }
            Err(e) => tracing::warn!("can't store the sessions: {e}"),
        }
    }

    pub fn insert(&self, s: Session) -> Arc<Session> {
        let s = Arc::new(s);
        let Ok(mut map) = self.inner.lock() else { return s };
        let now = ids::now_ms();
        map.retain(|_, v| !v.expired(now));
        while map.len() >= MAX_SESSIONS {
            let oldest = map.values().min_by_key(|v| v.created_ms).map(|v| v.token.clone());
            match oldest {
                Some(k) => map.remove(&k),
                None => break,
            };
        }
        map.insert(s.token.clone(), s.clone());
        self.save(&map);
        s
    }

    /// A live session by its token.
    pub fn get(&self, token: &str) -> Option<Arc<Session>> {
        if token.is_empty() || token.len() > 200 {
            return None;
        }
        let map = self.inner.lock().ok()?;
        let s = map.get(token)?;
        (ids::same_secret(&s.token, token) && !s.expired(ids::now_ms())).then(|| s.clone())
    }

    pub fn remove(&self, token: &str) {
        if let Ok(mut map) = self.inner.lock()
            && map.remove(token).is_some()
        {
            self.save(&map);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(token: &str, expires: i64) -> Session {
        Session {
            token: token.into(),
            room: "nc:x:1".into(),
            kind: Kind::Web,
            user: User::new("ada", "Ada"),
            role: Role::Edit,
            app: "gridcraft".into(),
            file_name: "a.xlsx".into(),
            wopi_src: String::new(),
            access_token: String::new(),
            access_token_ttl: 0,
            nextcloud: String::new(),
            file_id: String::new(),
            created_ms: 0,
            expires_ms: expires,
            post_message_origin: String::new(),
            close_url: String::new(),
        }
    }

    #[test]
    fn persists_and_expires() {
        let dir = std::env::temp_dir().join(format!("craft-live-sessions-{}", ids::token(8)));
        let s = Sessions::open(Some(dir.clone()));
        s.insert(session("aaaa", ids::now_ms() + 60_000));
        s.insert(session("bbbb", 1));
        assert!(s.get("aaaa").is_some());
        assert!(s.get("bbbb").is_none());
        assert!(s.get("").is_none());
        let again = Sessions::open(Some(dir.clone()));
        assert!(again.get("aaaa").is_some());
        assert_eq!(again.len(), 1);
        assert!(User::new("", "").name.starts_with("guest-"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
