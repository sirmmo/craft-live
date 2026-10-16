//! The commands: Open, Share, Join, Leave, Status, Connect, Settings.
//!
//! A live session for a Nextcloud file is a craft-live room. The add-in gets a session for the
//! file from craft-live (proving the person's Nextcloud identity with the account's app
//! password: docs/design.md, "Desktop clients"), then answers `collab.join`: `mode: share` when
//! this person is the first and must seed the room (from the file it downloaded, or from the
//! open document for Share), `mode: join` otherwise.

use std::time::Duration;

use serde_json::{Value as Json, json};

use crate::net::{self, Fail as NetFail, Nc};
use crate::runtime::{Commands, Cx, RoomCfg};
use crate::store::{self, Account, Store};

type Fail = (i64, String);

fn fail(m: impl Into<String>) -> Fail {
    (1, m.into())
}

/// Items in one list dialog at most.
const MAX_ITEMS: usize = 2000;
const COMMAND_TIME: Duration = Duration::from_secs(50);

fn message(title: &str, prompt: &str) -> Json {
    json!({"dialog": {"title": title, "kind": "message", "prompt": prompt.chars().take(4000).collect::<String>()}})
}

fn inputs(params: &Json) -> &Json {
    params.get("fields").filter(|f| f.is_object()).unwrap_or(params)
}

fn text(v: &Json, k: &str) -> Option<String> {
    v.get(k).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 4096).map(str::to_string)
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The add-in's commands.
pub struct Live {
    store: Option<Store>,
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

/// What a room is started for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Open a Nextcloud file: seed (if first) from the downloaded file.
    Open,
    /// Make the open document live: seed (if first) from it.
    Share,
    /// Join from a link: like Open.
    Join,
}

/// The extensions each app opens.
fn opens(app: &str, name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let list: &[&str] = match app {
        "gridcraft" => &["xlsx", "xlsm", "xltx", "ods"],
        "wordcraft" => &["docx", "docm", "odt"],
        "deckcraft" => &["pptx", "pptm", "odp"],
        _ => &["xlsx", "xlsm", "xltx", "ods", "docx", "docm", "odt", "pptx", "pptm", "odp"],
    };
    list.contains(&ext.as_str())
}

impl Live {
    pub fn new() -> Live {
        Live { store: Store::open() }
    }

    fn store(&self) -> Result<&Store, Fail> {
        self.store.as_ref().ok_or_else(|| fail("There's no folder for the add-in's data (no HOME or APPDATA)."))
    }

    fn connect_form(&self, prompt: &str, value: &str) -> Json {
        json!({"dialog": {
            "title": "Connect to Nextcloud", "kind": "form", "prompt": prompt,
            "fields": [{"id": "server", "label": "Nextcloud address", "value": value}],
            "command": "connect", "okLabel": "Connect",
        }})
    }

    /// `connect`: the form, then Login Flow v2 in the browser.
    fn connect(&self, params: &Json) -> Result<Json, Fail> {
        let Some(input) = text(inputs(params), "server") else {
            return Ok(self.connect_form("Type your Nextcloud's address; you'll sign in in the browser.", ""));
        };
        let server = match net::normalize_server(&input) {
            Ok(s) => s,
            Err(e) => return Ok(self.connect_form(&format!("“{input}”: {e}"), &input)),
        };
        let (login, token, endpoint) = match net::login_start(&server) {
            Ok(x) => x,
            Err(e) => return Ok(self.connect_form(&format!("{server}: {}", e.text()), &input)),
        };
        open_browser(&login);
        Ok(self.waiting(&server, &login, &token, &endpoint, &format!("Sign in in the browser window that opened ({login}), then click Continue.")))
    }

    fn waiting(&self, server: &str, login: &str, token: &str, endpoint: &str, prompt: &str) -> Json {
        json!({
            "dialog": {
                "title": "Connect to Nextcloud", "kind": "message", "prompt": prompt,
                "command": "finishLogin", "params": {"server": server, "login": login, "token": token, "endpoint": endpoint},
                "okLabel": "Continue",
            },
            "result": {"loginUrl": login},
        })
    }

    /// `finishLogin`: polls Login Flow v2 and saves the account.
    fn finish_login(&self, params: &Json) -> Result<Json, Fail> {
        let get = |k: &str| text(params, k).ok_or_else(|| (-32602, format!("`{k}` is required")));
        let (server, login, token, endpoint) = (get("server")?, get("login")?, get("token")?, get("endpoint")?);
        let answer = match net::login_poll(&endpoint, &token) {
            Ok(Some(x)) => x,
            Ok(None) => return Ok(self.waiting(&server, &login, &token, &endpoint, "Still waiting for the sign-in in the browser. Click Continue once done.")),
            Err(e) => return Ok(self.connect_form(&format!("The sign-in didn't finish: {}", e.text()), &server)),
        };
        let (srv, login_name, password) = answer;
        let server = net::normalize_server(&srv).unwrap_or(server);
        let mut acc = Account {
            id: store::stable_id(&format!("{server}\n{login_name}")),
            server: server.clone(),
            login_name: login_name.clone(),
            user_id: String::new(),
            app_password: password,
            source: "nextcloud-live".into(),
        };
        acc.user_id = Nc::new(&acc, COMMAND_TIME).user_id().unwrap_or_else(|_| login_name.clone());
        self.store()?.save_account(&acc).map_err(fail)?;
        Ok(json!({"message": format!("Connected to {server} as {login_name}"), "result": {"account": acc.id}}))
    }

    fn account(&self, params: &Json) -> Option<Account> {
        self.store.as_ref()?.account(text(params, "account").as_deref())
    }

    fn choose_account(&self, cx: &mut Cx, params: &Json) -> Result<Json, Fail> {
        if let Some(id) = text(params, "id") {
            return self.open(cx, &json!({"account": id, "path": "/"}));
        }
        let accounts = self.store()?.accounts();
        if accounts.is_empty() {
            return Ok(self.connect_form("Connect to your Nextcloud first.", ""));
        }
        let items: Vec<Json> = accounts.iter().map(|a| json!({"id": a.id, "label": clip(&a.label(), 256), "detail": a.source, "icon": "folder"})).collect();
        Ok(json!({"dialog": {
            "title": "Nextcloud accounts", "kind": "list", "prompt": "Pick the account to open a file from.", "items": items,
            "command": "chooseAccount", "okLabel": "Choose", "buttons": [{"label": "Add Account…", "command": "connect"}],
        }}))
    }

    /// `open {account?, path?, id?, nav?}`: a folder listing; picking a file opens it live.
    fn open(&self, cx: &mut Cx, params: &Json) -> Result<Json, Fail> {
        let Some(acc) = self.account(params) else { return Ok(self.connect_form("Connect to your Nextcloud first.", "")) };
        let nav = params.get("nav").and_then(Json::as_bool) == Some(true);
        let mut path = text(params, "path").unwrap_or_else(|| "/".into());
        if !nav && let Some(id) = text(params, "id") {
            if let Some(folder) = id.strip_suffix('/') {
                path = if folder.is_empty() { "/".into() } else { folder.to_string() };
            } else {
                return self.start_room(cx, &acc, &id, Purpose::Open, None);
            }
        }
        if !path.starts_with('/') || path.split('/').any(|s| s == "..") {
            return Err(fail("That isn't a path in Nextcloud."));
        }
        let nc = Nc::new(&acc, COMMAND_TIME);
        let mut items = match nc.propfind(&path, 1) {
            Ok(i) => i,
            Err(e) => return Ok(message("Open Live", &format!("{}: {}", acc.label(), e.text()))),
        };
        let here = path.trim_end_matches('/').to_string();
        items.retain(|i| i.path.trim_end_matches('/') != here && i.path != "/" && (i.folder || opens(&cx.session.app, &i.name)));
        items.sort_by(|a, b| b.folder.cmp(&a.folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        items.truncate(MAX_ITEMS);
        let list: Vec<Json> = items
            .iter()
            .map(|i| {
                if i.folder {
                    json!({"id": format!("{}/", i.path), "label": clip(&i.name, 256), "detail": "Folder", "icon": "folder"})
                } else {
                    let detail = i.size.map(|s| format!("{} KB", s.div_ceil(1024))).unwrap_or_default();
                    json!({"id": i.path, "label": clip(&i.name, 256), "detail": detail, "icon": "link"})
                }
            })
            .collect();
        let mut buttons = Vec::new();
        if path != "/" {
            let parent = path.trim_end_matches('/').rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap_or("/").to_string();
            buttons.push(json!({"label": "Up", "command": "open", "params": {"account": acc.id, "path": parent, "nav": true}}));
        }
        if self.store()?.accounts().len() > 1 {
            buttons.push(json!({"label": "Switch Account", "command": "chooseAccount"}));
        }
        let prompt = format!("{} · {path}\nPick a file: everyone who opens it (here or in Nextcloud Office) edits it together.", acc.label());
        Ok(json!({"dialog": {
            "title": "Open Live (Nextcloud)", "kind": "list", "prompt": prompt, "items": list,
            "command": "open", "params": {"account": acc.id, "path": path}, "okLabel": "Open", "buttons": buttons,
        }}))
    }

    /// The craft-live API for an account's Nextcloud: `CRAFT_LIVE_URL`, the settings, or the
    /// Office server Nextcloud names (when it's craft-live).
    fn live_api(&self, acc: &Account, nc: &Nc) -> Option<String> {
        let as_api = |u: &str| {
            let u = u.trim().trim_end_matches('/');
            if u.ends_with("/api/v1") { u.to_string() } else { format!("{u}/api/v1") }
        };
        if let Ok(u) = std::env::var("CRAFT_LIVE_URL")
            && !u.trim().is_empty()
        {
            return Some(as_api(&u));
        }
        let store = self.store.as_ref()?;
        if let Some(u) = store.live_url(&acc.server) {
            return Some(as_api(&u));
        }
        let office = nc.office_server()?;
        let api = net::live_api(&office)?;
        let base = api.trim_end_matches("/api/v1").to_string();
        let _ = store.set_live_url(&acc.server, Some(&base));
        Some(api)
    }

    /// Gets a craft-live session for a file and answers `collab.join`.
    fn start_room(&self, cx: &mut Cx, acc: &Account, path: &str, purpose: Purpose, item: Option<net::Item>) -> Result<Json, Fail> {
        if !cx.session.can_collab() {
            return Err(fail("This app doesn't support live co-editing yet (it needs craft plugin protocol 2)."));
        }
        let nc = Nc::new(acc, COMMAND_TIME);
        let item = match item {
            Some(i) => i,
            None => nc.stat(path).map_err(|e| fail(format!("{path}: {}", e.text())))?,
        };
        if item.folder {
            return self.open(cx, &json!({"account": acc.id, "path": path}));
        }
        if !cx.session.app.is_empty() && !opens(&cx.session.app, &item.name) {
            return Err(fail(format!("{} isn't a file this app opens.", item.name)));
        }
        let Some(api) = self.live_api(acc, &nc) else {
            return Ok(json!({"dialog": {
                "title": "Live (Nextcloud)", "kind": "form",
                "prompt": format!("Which craft-live server does {} use? (Its administrator knows: it's Nextcloud Office's server address.)", acc.server),
                "fields": [{"id": "liveUrl", "label": "craft-live address", "value": ""}, {"id": "server", "label": "Nextcloud", "value": acc.server}],
                "command": "saveSettings", "okLabel": "Save",
            }}));
        };
        let s = match net::create_session(&api, acc, &item.path, &item.file_id) {
            Ok(s) => s,
            Err(NetFail::Status(401, m)) => return Err(fail(format!("craft-live couldn't confirm your Nextcloud sign-in ({m}). Connect again."))),
            Err(e) => return Err(fail(format!("craft-live: {}", e.text()))),
        };
        let room = s.get("room").and_then(Json::as_str).unwrap_or("").to_string();
        if room.is_empty() || room.chars().count() > 256 {
            return Err(fail("craft-live gave no room"));
        }
        let seed = s.get("seed").and_then(Json::as_bool).unwrap_or(false);
        let mut join = json!({
            "room": room,
            "mode": if seed { "share" } else { "join" },
            "user": s.get("user").cloned().unwrap_or(Json::Null),
            "role": s.get("role").and_then(Json::as_str).unwrap_or("view"),
            "title": clip(&item.name, 200),
        });
        let mut etag = item.etag.clone();
        if seed && purpose != Purpose::Share {
            let local = self.store()?.cache_path(&acc.id, &item.path).map_err(fail)?;
            etag = nc.download(&item.path, &local).map_err(|e| fail(format!("Couldn't download {}: {}", item.name, e.text())))?;
            if let Some(o) = join.as_object_mut() {
                o.insert("path".into(), json!(local.to_string_lossy()));
            }
        }
        cx.prepared.insert(
            room.clone(),
            RoomCfg {
                room: room.clone(),
                session: s.clone(),
                account: acc.clone(),
                api,
                path: item.path.clone(),
                file_id: item.file_id.clone(),
                etag,
                seeding: seed,
            },
        );
        let link = format!("{}/f/{}", acc.server, item.file_id.trim_start_matches('0'));
        let msg = if seed { format!("Opening {} live", item.name) } else { format!("Joining the live session of {}…", item.name) };
        Ok(json!({"collab": {"join": join}, "message": msg, "result": {"room": room, "link": link, "seed": seed}}))
    }

    /// The Nextcloud file a local document is a copy of (opened by this add-in or by
    /// craft-nextcloud): `(account id, remote path)`.
    fn remote_for_local(&self, local: &str) -> Option<(String, String)> {
        let store = self.store.as_ref()?;
        let local = std::path::Path::new(local);
        for base in [Some(store.dir.join("files")), store.nextcloud_dir.as_ref().map(|d| d.join("files"))].into_iter().flatten() {
            if let Ok(rest) = local.strip_prefix(&base) {
                let mut parts = rest.iter().map(|p| p.to_string_lossy().into_owned());
                let account = parts.next()?;
                let remote: Vec<String> = parts.collect();
                if remote.is_empty() {
                    return None;
                }
                return Some((account, format!("/{}", remote.join("/"))));
            }
        }
        None
    }

    /// `share {account?, path?}`: the open document goes live as a Nextcloud file.
    fn share(&self, cx: &mut Cx, params: &Json, context: &Json) -> Result<Json, Fail> {
        let i = inputs(params);
        if i.get("encrypt").and_then(Json::as_bool) == Some(true) {
            return Err(fail("Nextcloud live sessions can't be end-to-end encrypted: craft-live keeps the document to save it to Nextcloud."));
        }
        let (account, path) = match text(i, "path") {
            Some(p) => (text(i, "account"), p),
            None => {
                let local = context.get("document").and_then(|d| d.get("path")).and_then(Json::as_str).unwrap_or("");
                match self.remote_for_local(local) {
                    Some((a, p)) => (Some(a), p),
                    None => {
                        return Ok(message(
                            "Share Live (Nextcloud)",
                            "This document isn't a Nextcloud file. Open it with Live › Open (or from Nextcloud), or save it to Nextcloud first.",
                        ));
                    }
                }
            }
        };
        if !path.starts_with('/') || path.split('/').any(|s| s == "..") {
            return Err(fail("That isn't a path in Nextcloud."));
        }
        let Some(acc) = self.store()?.account(account.as_deref()) else { return Ok(self.connect_form("Connect to your Nextcloud first.", "")) };
        self.start_room(cx, &acc, &path, Purpose::Share, None)
    }

    /// `join {link}`: a file's Nextcloud link (`https://cloud.example/f/288`).
    fn join(&self, cx: &mut Cx, params: &Json) -> Result<Json, Fail> {
        let i = inputs(params);
        let Some(link) = text(i, "link") else {
            if params.get("fields").is_some() {
                return Err(fail("Paste the file's Nextcloud link."));
            }
            return Ok(json!({"dialog": {
                "title": "Join Live (Nextcloud)", "kind": "form",
                "prompt": "Paste the file's Nextcloud link (Nextcloud › file › Copy internal link: https://…/f/<number>).",
                "fields": [{"id": "link", "label": "Link", "value": ""}], "command": "join", "okLabel": "Join",
            }}));
        };
        let Some((server, file_id)) = net::parse_file_link(&link) else {
            return Err(fail("That isn't a Nextcloud file link (https://cloud.example/f/<number>)."));
        };
        let accounts = self.store()?.accounts();
        let acc = match text(i, "account") {
            Some(id) => accounts.into_iter().find(|a| a.id == id),
            None => accounts.into_iter().find(|a| a.server.eq_ignore_ascii_case(&server)),
        };
        let Some(acc) = acc else { return Ok(self.connect_form(&format!("Connect to {server} first."), &server)) };
        let nc = Nc::new(&acc, COMMAND_TIME);
        let item = nc.find_by_id(&file_id).map_err(|e| fail(format!("That file isn't in your Nextcloud ({}).", e.text())))?;
        let path = item.path.clone();
        self.start_room(cx, &acc, &path, Purpose::Join, Some(item))
    }

    fn leave(&self, cx: &mut Cx, params: &Json) -> Result<Json, Fail> {
        let rooms = cx.rooms();
        let room = match text(inputs(params), "room") {
            Some(r) => r,
            None => match rooms.as_slice() {
                [] => return Ok(json!({"message": "You're not in a live session."})),
                [one] => one.room.clone(),
                _ => return Err(fail("You're in several live sessions: leave from the status bar, or name the room.")),
            },
        };
        if cx.leave(&room, "You left the live session.") {
            Ok(json!({"message": "Left the live session.", "result": {"room": room}}))
        } else {
            Err(fail(format!("Not in the live session {room}.")))
        }
    }

    fn status(&self, cx: &Cx) -> Json {
        let rooms = cx.rooms();
        if rooms.is_empty() {
            let mut m = message("Live (Nextcloud)", "You're not in a live session.");
            if let Some(o) = m.as_object_mut() {
                o.insert("result".into(), json!({"rooms": []}));
            }
            return m;
        }
        let mut prompt = String::new();
        let mut list = Vec::new();
        for r in &rooms {
            let names: Vec<&str> = r.peers.iter().filter_map(|u| u.get("name").and_then(Json::as_str)).collect();
            prompt.push_str(&format!(
                "{}{}\nNextcloud: {}\nState: {}{}\nRole: {}\nWith: {}\n\n",
                r.path,
                "",
                r.server,
                r.state,
                r.message.as_deref().map(|m| format!(" ({m})")).unwrap_or_default(),
                r.role,
                if names.is_empty() { "nobody else yet".to_string() } else { names.join(", ") }
            ));
            list.push(json!({"room": r.room, "state": r.state, "peers": r.peers, "path": r.path, "server": r.server, "role": r.role}));
        }
        let mut m = message("Live (Nextcloud)", prompt.trim_end());
        if let Some(o) = m.as_object_mut() {
            o.insert("result".into(), json!({"rooms": list}));
        }
        m
    }

    fn settings(&self) -> Result<Json, Fail> {
        let store = self.store()?;
        let accounts = store.accounts();
        let Some(acc) = accounts.first() else { return Ok(self.connect_form("Connect to your Nextcloud first.", "")) };
        let url = store.live_url(&acc.server).unwrap_or_default();
        Ok(json!({"dialog": {
            "title": "Live (Nextcloud) Settings", "kind": "form",
            "prompt": format!("The craft-live server of {}. Leave it empty to ask Nextcloud (its Office server). Saved in {}.", acc.server, store.dir.display()),
            "fields": [{"id": "liveUrl", "label": "craft-live address", "value": url}, {"id": "server", "label": "Nextcloud", "value": acc.server}],
            "command": "saveSettings", "okLabel": "Save",
        }}))
    }

    fn save_settings(&self, params: &Json) -> Result<Json, Fail> {
        let f = inputs(params);
        let store = self.store()?;
        let server = match text(f, "server") {
            Some(s) => net::normalize_server(&s).map_err(fail)?,
            None => return Err(fail("Name the Nextcloud.")),
        };
        match text(f, "liveUrl") {
            Some(u) => {
                let lower = u.to_ascii_lowercase();
                if !(lower.starts_with("https://") || lower.starts_with("http://")) || u.chars().any(char::is_whitespace) {
                    return Err(fail(format!("“{u}” isn't an http(s) address.")));
                }
                store.set_live_url(&server, Some(u.trim_end_matches('/'))).map_err(fail)?;
            }
            None => store.set_live_url(&server, None).map_err(fail)?,
        }
        Ok(json!({"message": "Live (Nextcloud) settings saved."}))
    }
}

/// Opens a page in the person's browser (Login Flow v2).
fn open_browser(url: &str) {
    if std::env::var_os("NEXTCLOUD_LIVE_NO_BROWSER").is_some() {
        return;
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return;
    }
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = r {
        eprintln!("nextcloud-live: couldn't open the browser: {e}");
    }
}

impl Commands for Live {
    fn run(&mut self, cx: &mut Cx, command: &str, params: &Json, context: &Json) -> Result<Json, Fail> {
        match command {
            "open" => self.open(cx, params),
            "share" => self.share(cx, params, context),
            "join" => self.join(cx, params),
            "leave" => self.leave(cx, params),
            "status" => Ok(self.status(cx)),
            "connect" => self.connect(params),
            "finishLogin" => self.finish_login(params),
            "chooseAccount" => self.choose_account(cx, params),
            "settings" => self.settings(),
            "saveSettings" => self.save_settings(params),
            "dismiss" => Ok(json!({})),
            other => Err(fail(format!("nextcloud-live has no command {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_app_opens() {
        assert!(opens("gridcraft", "a.XLSX"));
        assert!(!opens("gridcraft", "a.docx"));
        assert!(opens("deckcraft", "a.odp"));
        assert!(opens("", "a.docm"));
    }
}
