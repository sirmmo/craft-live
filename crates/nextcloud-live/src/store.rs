//! The add-in's files, in its data folder (`CRAFT_NEXTCLOUD_LIVE_DATA`, else `nextcloud-live` in
//! the platform's data folder; shared by the three apps):
//!
//! - `accounts.json`: Nextcloud accounts and their app passwords (owner-only on Unix);
//! - `settings.json`: the craft-live address per Nextcloud, defaults;
//! - `files/<account>/<remote path>`: local copies of files opened live (the seed).
//!
//! Accounts the craft-nextcloud add-in already signed in (its `accounts.json`, same format) are
//! offered too, so nobody signs in twice.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

/// A Nextcloud account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    /// `https://cloud.example.com` (no trailing slash).
    pub server: String,
    pub login_name: String,
    /// The user id in WebDAV paths.
    pub user_id: String,
    pub app_password: String,
    /// `nextcloud-live` or `craft-nextcloud` (where it was found).
    pub source: String,
}

impl Account {
    pub fn label(&self) -> String {
        let host = self.server.split("://").nth(1).unwrap_or(&self.server);
        if self.login_name.contains('@') { format!("{} ({host})", self.login_name) } else { format!("{}@{host}", self.login_name) }
    }

    fn from_json(v: &Json, source: &str) -> Option<Account> {
        let s = |k: &str| v.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        let a = Account {
            id: s("id"),
            server: s("server"),
            login_name: s("loginName"),
            user_id: s("userId"),
            app_password: s("appPassword"),
            source: source.into(),
        };
        (!a.id.is_empty() && !a.server.is_empty() && !a.login_name.is_empty() && !a.app_password.is_empty()).then_some(a)
    }

    fn to_json(&self) -> Json {
        json!({"id": self.id, "server": self.server, "loginName": self.login_name, "userId": self.user_id, "appPassword": self.app_password})
    }

    /// The user id for WebDAV paths (the login name when the server didn't say).
    pub fn dav_user(&self) -> &str {
        if self.user_id.is_empty() { &self.login_name } else { &self.user_id }
    }
}

/// A platform data folder: `<env>` when set, else `<base>/<name>`.
pub fn data_dir(name: &str, env_var: &str) -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(p) = env(env_var) {
        return Some(p);
    }
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local").join("share")))
    };
    base.map(|b| b.join(name))
}

fn read_json(path: &Path) -> Json {
    let mut buf = Vec::new();
    match std::fs::File::open(path).and_then(|f| f.take(16 << 20).read_to_end(&mut buf)) {
        Ok(_) => serde_json::from_slice(&buf).ok().filter(Json::is_object).unwrap_or_else(|| json!({})),
        Err(_) => json!({}),
    }
}

/// Writes through a temporary file; owner-only on Unix.
pub fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("file")));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    f.write_all(data).and_then(|_| f.sync_all()).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// The data folder.
#[derive(Clone, Debug)]
pub struct Store {
    pub dir: PathBuf,
    /// craft-nextcloud's data folder, for its accounts.
    pub nextcloud_dir: Option<PathBuf>,
}

impl Store {
    pub fn open() -> Option<Store> {
        Some(Store { dir: data_dir("nextcloud-live", "CRAFT_NEXTCLOUD_LIVE_DATA")?, nextcloud_dir: data_dir("craft-nextcloud", "CRAFT_NEXTCLOUD_DATA") })
    }

    /// Own accounts first, then craft-nextcloud's that aren't the same.
    pub fn accounts(&self) -> Vec<Account> {
        let mut out: Vec<Account> = Vec::new();
        let mut add = |list: &Json, source: &str| {
            for a in list.get("accounts").and_then(Json::as_array).into_iter().flatten().filter_map(|v| Account::from_json(v, source)) {
                if !out.iter().any(|o| o.server == a.server && o.login_name == a.login_name) {
                    out.push(a);
                }
            }
        };
        add(&read_json(&self.dir.join("accounts.json")), "nextcloud-live");
        if let Some(d) = &self.nextcloud_dir {
            add(&read_json(&d.join("accounts.json")), "craft-nextcloud");
        }
        out
    }

    pub fn account(&self, id: Option<&str>) -> Option<Account> {
        let list = self.accounts();
        match id {
            Some(id) => list.iter().find(|a| a.id == id).cloned().or_else(|| list.first().cloned()),
            None => list.first().cloned(),
        }
    }

    /// Adds or replaces an own account.
    pub fn save_account(&self, acc: &Account) -> Result<(), String> {
        let mut file = read_json(&self.dir.join("accounts.json"));
        let mut list: Vec<Json> = file.get("accounts").and_then(Json::as_array).cloned().unwrap_or_default();
        list.retain(|v| v.get("id").and_then(Json::as_str) != Some(acc.id.as_str()));
        list.insert(0, acc.to_json());
        if let Some(o) = file.as_object_mut() {
            o.insert("accounts".into(), Json::Array(list));
        }
        write_private(&self.dir.join("accounts.json"), &serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?)
    }

    pub fn settings(&self) -> Json {
        read_json(&self.dir.join("settings.json"))
    }

    pub fn save_settings(&self, v: &Json) -> Result<(), String> {
        write_private(&self.dir.join("settings.json"), &serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?)
    }

    /// The craft-live address remembered for a Nextcloud.
    pub fn live_url(&self, server: &str) -> Option<String> {
        self.settings().get("liveUrls").and_then(|m| m.get(server)).and_then(Json::as_str).filter(|s| !s.is_empty()).map(str::to_string)
    }

    pub fn set_live_url(&self, server: &str, url: Option<&str>) -> Result<(), String> {
        let mut s = self.settings();
        if let Some(o) = s.as_object_mut() {
            let urls = o.entry("liveUrls").or_insert_with(|| json!({}));
            if let Some(u) = urls.as_object_mut() {
                match url {
                    Some(v) => u.insert(server.to_string(), json!(v)),
                    None => u.remove(server),
                };
            }
        }
        self.save_settings(&s)
    }

    /// Where a remote file's local copy goes: `files/<account>/<path>`, every segment checked.
    pub fn cache_path(&self, account: &str, remote: &str) -> Result<PathBuf, String> {
        let mut p = self.dir.join("files").join(safe_segment(account)?);
        for seg in remote.split('/').filter(|s| !s.is_empty()) {
            p.push(safe_segment(seg)?);
        }
        Ok(p)
    }
}

fn safe_segment(s: &str) -> Result<String, String> {
    if s == "."
        || s == ".."
        || s.is_empty()
        || s.len() > 255
        || s.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(format!("“{s}” can't be a local file name"));
    }
    Ok(s.to_string())
}

/// A short stable id of a text (FNV-1a, hex): account ids.
pub fn stable_id(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_and_paths() {
        let base = std::env::temp_dir().join(format!("nextcloud-live-store-{}", stable_id(&format!("{:?}", std::time::SystemTime::now()))));
        let s = Store { dir: base.join("live"), nextcloud_dir: Some(base.join("nc")) };
        assert!(s.accounts().is_empty());
        std::fs::create_dir_all(base.join("nc")).unwrap();
        std::fs::write(
            base.join("nc/accounts.json"),
            r#"{"accounts":[{"id":"n1","server":"https://c.example","loginName":"ada","userId":"ada","appPassword":"p"}]}"#,
        )
        .unwrap();
        assert_eq!(s.accounts()[0].source, "craft-nextcloud");
        let own = Account {
            id: "l1".into(),
            server: "https://c.example".into(),
            login_name: "ada".into(),
            user_id: "ada".into(),
            app_password: "q".into(),
            source: String::new(),
        };
        s.save_account(&own).unwrap();
        let list = s.accounts();
        assert_eq!(list.len(), 1, "the same account isn't listed twice");
        assert_eq!(list[0].app_password, "q");
        s.set_live_url("https://c.example", Some("https://live.example")).unwrap();
        assert_eq!(s.live_url("https://c.example").as_deref(), Some("https://live.example"));
        assert!(s.cache_path("l1", "/Docs/a.xlsx").unwrap().ends_with("files/l1/Docs/a.xlsx"));
        assert!(s.cache_path("l1", "/../x").is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
