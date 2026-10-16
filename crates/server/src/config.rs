//! Configuration: a TOML file (`--config PATH` or `CRAFT_LIVE_CONFIG`), then environment
//! variables, which win. `docs/deploy.md` documents every key; `craft-live.example.toml` shows
//! them all.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// The three apps and what they open.
pub const APPS: [&str; 3] = ["gridcraft", "wordcraft", "deckcraft"];

/// One format a craft app opens: extension, MIME type, app.
pub struct Format {
    pub ext: &'static str,
    pub mime: &'static str,
    pub app: &'static str,
}

/// Every format in the discovery.
pub const FORMATS: [Format; 10] = [
    Format { ext: "xlsx", mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", app: "gridcraft" },
    Format { ext: "xlsm", mime: "application/vnd.ms-excel.sheet.macroEnabled.12", app: "gridcraft" },
    Format { ext: "xltx", mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.template", app: "gridcraft" },
    Format { ext: "ods", mime: "application/vnd.oasis.opendocument.spreadsheet", app: "gridcraft" },
    Format { ext: "docx", mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document", app: "wordcraft" },
    Format { ext: "docm", mime: "application/vnd.ms-word.document.macroEnabled.12", app: "wordcraft" },
    Format { ext: "odt", mime: "application/vnd.oasis.opendocument.text", app: "wordcraft" },
    Format { ext: "pptx", mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation", app: "deckcraft" },
    Format { ext: "pptm", mime: "application/vnd.ms-powerpoint.presentation.macroEnabled.12", app: "deckcraft" },
    Format { ext: "odp", mime: "application/vnd.oasis.opendocument.presentation", app: "deckcraft" },
];

/// The app that opens a file name, by its extension.
pub fn app_for_name(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    FORMATS.iter().find(|f| f.ext == ext).map(|f| f.app)
}

/// The lower-case extension of a file name.
pub fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// When craft-live saves a room back to the file.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Autosave {
    /// After this many updates since the last save.
    pub updates: u32,
    /// After this long without an update, when there are unsaved edits.
    pub idle_secs: u64,
    /// At least this often while people edit.
    pub max_secs: u64,
    /// When only one editor is left: after this long without an update (so the file is saved
    /// before the last person goes).
    pub last_editor_idle_secs: f64,
    /// After a failed save, wait this long.
    pub retry_secs: u64,
    /// How long a client may take to deliver the file for a save.
    pub timeout_secs: u64,
}

impl Default for Autosave {
    fn default() -> Self {
        Autosave { updates: 200, idle_secs: 30, max_secs: 300, last_editor_idle_secs: 5.0, retry_secs: 30, timeout_secs: 120 }
    }
}

/// Limits.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Largest file opened or saved, in MiB.
    pub max_file_mb: u64,
    pub max_rooms: usize,
    pub max_connections_per_room: usize,
    /// How long a session lasts when the WOPI host gives no expiry (Nextcloud sends
    /// `access_token_ttl=0`), and desktop sessions.
    pub session_hours: u64,
    /// WOPI RefreshLock interval.
    pub lock_refresh_secs: u64,
    /// How long an empty room waits (a reload, a reconnect) before it saves its state, unlocks
    /// the file and closes.
    pub close_grace_secs: u64,
    /// How long a seed lease waits for its client to connect.
    pub seed_lease_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_mb: 100,
            max_rooms: 1000,
            max_connections_per_room: 100,
            session_hours: 10,
            lock_refresh_secs: 600,
            close_grace_secs: 30,
            seed_lease_secs: 120,
        }
    }
}

/// The configuration.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where to listen (`0.0.0.0:9980`).
    pub listen: String,
    /// The address browsers and Nextcloud use (`https://live.example.com`): the discovery's
    /// `urlsrc`, the WebSocket and API addresses. Serve craft-live at the root of its host
    /// (Nextcloud drops a path or a port from it; docs/design.md).
    pub public_url: String,
    /// Room states, sessions, proof keys.
    pub data_dir: PathBuf,
    /// The Nextcloud servers allowed to open files (their base URLs). A WOPISrc must start with
    /// one of them; desktop sessions name one of them.
    pub wopi_hosts: Vec<String>,
    /// Sign WOPI requests with proof keys (and publish the keys in the discovery).
    pub proof_keys: bool,
    /// Check the WOPI hosts' TLS certificates.
    pub verify_tls: bool,
    /// Each app's web build (the folder with its `index.html`), by app id.
    pub apps: BTreeMap<String, PathBuf>,
    /// Accept desktop sessions (nextcloud-live) that prove a Nextcloud identity with an app
    /// password.
    pub desktop_sessions: bool,
    /// The version `/hosting/capabilities` reports (Nextcloud gates some features on it).
    pub compat_version: String,
    pub autosave: Autosave,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:9980".into(),
            public_url: "http://127.0.0.1:9980".into(),
            data_dir: PathBuf::from("craft-live-data"),
            wopi_hosts: Vec::new(),
            proof_keys: true,
            verify_tls: true,
            apps: BTreeMap::new(),
            desktop_sessions: true,
            compat_version: "25.04.0.0".into(),
            autosave: Autosave::default(),
            limits: Limits::default(),
        }
    }
}

/// An allowed Nextcloud server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WopiHost {
    /// `https://cloud.example.com/nextcloud` (no trailing slash).
    pub base: String,
    /// `cloud.example.com/nextcloud`: the room ids' host part (scheme left out, so http and
    /// https reach the same rooms).
    pub key: String,
}

/// `HTTPS://Cloud.Example.com:443/x/` → `https://cloud.example.com/x`.
pub fn normalize_base(input: &str) -> Result<String, String> {
    let s = input.trim();
    let (scheme, rest) = s.split_once("://").ok_or_else(|| format!("{s:?} isn't an http(s) address"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("{s:?} isn't an http(s) address"));
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = match rest.find('/') {
        Some(i) => (rest.get(..i).unwrap_or(""), rest.get(i..).unwrap_or("")),
        None => (rest, ""),
    };
    let mut host = host.to_ascii_lowercase();
    if host.is_empty() || host.contains('@') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{s:?} has no host"));
    }
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    if host.ends_with(default_port) {
        host.truncate(host.len() - default_port.len());
    }
    let path = path.trim_end_matches('/');
    if path.contains("..") {
        return Err(format!("{s:?} has a bad path"));
    }
    Ok(format!("{scheme}://{host}{path}"))
}

impl Config {
    /// Reads the file (if any), then the environment.
    pub fn load(path: Option<PathBuf>) -> Result<Config, String> {
        let path = path.or_else(|| std::env::var_os("CRAFT_LIVE_CONFIG").map(PathBuf::from));
        let mut cfg = match &path {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
                toml::from_str::<Config>(&text).map_err(|e| format!("{}: {e}", p.display()))?
            }
            None => Config::default(),
        };
        cfg.apply_env(|k| std::env::var(k).ok())?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// `CRAFT_LIVE_*` variables.
    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        let flag = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        if let Some(v) = get("CRAFT_LIVE_LISTEN") {
            self.listen = v;
        }
        if let Some(v) = get("CRAFT_LIVE_PUBLIC_URL") {
            self.public_url = v;
        }
        if let Some(v) = get("CRAFT_LIVE_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("CRAFT_LIVE_WOPI_HOSTS") {
            self.wopi_hosts = v.split([',', ' ']).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string()).collect();
        }
        if let Some(v) = get("CRAFT_LIVE_PROOF_KEYS") {
            self.proof_keys = flag(&v);
        }
        if let Some(v) = get("CRAFT_LIVE_VERIFY_TLS") {
            self.verify_tls = flag(&v);
        }
        if let Some(v) = get("CRAFT_LIVE_DESKTOP_SESSIONS") {
            self.desktop_sessions = flag(&v);
        }
        for app in APPS {
            if let Some(v) = get(&format!("CRAFT_LIVE_APP_{}", app.to_ascii_uppercase())) {
                self.apps.insert(app.to_string(), PathBuf::from(v));
            }
        }
        Ok(())
    }

    pub fn validate(&mut self) -> Result<(), String> {
        self.listen_addr()?;
        self.public_url = normalize_base(&self.public_url).map_err(|e| format!("public_url: {e}"))?;
        for a in self.apps.keys() {
            if !APPS.contains(&a.as_str()) {
                return Err(format!("apps: unknown app {a:?} (gridcraft, wordcraft, deckcraft)"));
            }
        }
        let mut hosts = Vec::new();
        for h in &self.wopi_hosts {
            hosts.push(normalize_base(h).map_err(|e| format!("wopi_hosts: {e}"))?);
        }
        self.wopi_hosts = hosts;
        if self.limits.max_file_mb == 0 || self.limits.max_file_mb > 4096 {
            return Err("limits.max_file_mb must be 1–4096".into());
        }
        if self.limits.session_hours == 0 || self.limits.session_hours > 24 * 30 {
            return Err("limits.session_hours must be 1–720".into());
        }
        if !(0.0..=3600.0).contains(&self.autosave.last_editor_idle_secs) {
            return Err("autosave.last_editor_idle_secs must be 0–3600".into());
        }
        if self.autosave.updates == 0 {
            return Err("autosave.updates must be at least 1".into());
        }
        Ok(())
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen.parse().map_err(|_| format!("listen: {:?} isn't an address like 0.0.0.0:9980", self.listen))
    }

    pub fn max_file(&self) -> u64 {
        self.limits.max_file_mb << 20
    }

    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.limits.session_hours * 3600)
    }

    /// The allowed hosts.
    pub fn hosts(&self) -> Vec<WopiHost> {
        self.wopi_hosts.iter().map(|b| WopiHost { base: b.clone(), key: b.split_once("://").map(|(_, r)| r.to_string()).unwrap_or_default() }).collect()
    }

    /// The allowed host a Nextcloud base URL names.
    pub fn host_for_base(&self, base: &str) -> Option<WopiHost> {
        let b = normalize_base(base).ok()?;
        self.hosts().into_iter().find(|h| h.base == b)
    }

    /// The allowed host a WOPISrc lives on, and the file id; `None` when it isn't allowed.
    /// The WOPISrc must be `<base>/…/wopi/files/<id>` on an allowed base, with no query.
    pub fn check_wopi_src(&self, src: &str) -> Option<(WopiHost, String)> {
        if src.len() > 2048 || src.contains(['?', '#']) || src.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        let (scheme, rest) = src.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let (host, path) = rest.split_once('/')?;
        let normalized = normalize_base(&format!("{scheme}://{host}")).ok()?;
        let full = format!("{normalized}/{path}");
        if full.contains("/../") || full.contains("/./") {
            return None;
        }
        let h = self.hosts().into_iter().find(|h| full.starts_with(&format!("{}/", h.base)))?;
        let (_, id) = full.rsplit_once("/wopi/files/")?;
        if id.is_empty() || id.contains('/') {
            return None;
        }
        Some((h, id.to_string()))
    }
}

/// The room of a file on a Nextcloud: `nc:<host key>:<file id>`. A WOPI file id
/// `<fileid>_<instanceid>` (what Nextcloud sends) counts by its numeric file id, so the web and
/// the desktop (which knows WebDAV's `oc:fileid`) meet in the same room.
pub fn room_id(host: &WopiHost, file_id: &str) -> String {
    let numeric: String = file_id.chars().take_while(char::is_ascii_digit).collect();
    let rest = file_id.get(numeric.len()..).unwrap_or("");
    let id = if !numeric.is_empty() && (rest.is_empty() || rest.starts_with('_')) {
        numeric.trim_start_matches('0').to_string()
    } else {
        crate::ids::clean(file_id, 128)
    };
    let id = if id.is_empty() { "0".to_string() } else { id };
    let room = format!("nc:{}:{id}", host.key);
    room.chars().take(256).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config { wopi_hosts: vec!["HTTPS://Cloud.Example.com:443/".into(), "http://10.0.0.5:8080/nc".into()], ..Config::default() };
        c.validate().unwrap();
        c
    }

    #[test]
    fn hosts_and_wopi_src() {
        let c = cfg();
        assert_eq!(c.wopi_hosts, ["https://cloud.example.com", "http://10.0.0.5:8080/nc"]);
        let (h, id) = c.check_wopi_src("https://cloud.example.com/index.php/apps/richdocuments/wopi/files/288_oct9lk1km6wy").unwrap();
        assert_eq!(h.key, "cloud.example.com");
        assert_eq!(id, "288_oct9lk1km6wy");
        assert_eq!(room_id(&h, &id), "nc:cloud.example.com:288");
        assert_eq!(room_id(&h, "288"), "nc:cloud.example.com:288");
        assert_eq!(room_id(&h, "abc"), "nc:cloud.example.com:abc");
        assert!(c.check_wopi_src("https://cloud.example.com.evil.org/wopi/files/1").is_none());
        assert!(c.check_wopi_src("https://evil.org/https://cloud.example.com/wopi/files/1").is_none());
        assert!(c.check_wopi_src("https://cloud.example.com/wopi/files/1?x=1").is_none());
        assert!(c.check_wopi_src("https://cloud.example.com/a/../wopi/files/1").is_none());
        assert!(c.check_wopi_src("http://10.0.0.5:8080/nc/index.php/apps/richdocuments/wopi/files/7_x").is_some());
        assert!(c.check_wopi_src("http://10.0.0.5:8080/other/wopi/files/7").is_none());
        assert_eq!(c.host_for_base("https://cloud.example.com/").unwrap().key, "cloud.example.com");
    }

    #[test]
    fn env_and_validation() {
        let mut c = Config::default();
        c.apply_env(|k| match k {
            "CRAFT_LIVE_WOPI_HOSTS" => Some("https://a.example, https://b.example".into()),
            "CRAFT_LIVE_PROOF_KEYS" => Some("off".into()),
            "CRAFT_LIVE_APP_GRIDCRAFT" => Some("/srv/grid".into()),
            _ => None,
        })
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.wopi_hosts.len(), 2);
        assert!(!c.proof_keys);
        assert_eq!(c.apps.get("gridcraft").unwrap(), &PathBuf::from("/srv/grid"));
        let bad: Result<Config, _> = toml::from_str("nope = 1");
        assert!(bad.is_err());
        let mut c = Config { listen: "x".into(), ..Config::default() };
        assert!(c.validate().is_err());
        assert_eq!(app_for_name("Budget.XLSX"), Some("gridcraft"));
        assert_eq!(app_for_name("a.pdf"), None);
    }
}
