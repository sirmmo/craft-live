//! The push channel (protocol 2, section 1) and the rooms the add-in holds.
//!
//! One thread reads the app's lines; each room's link thread reports from craft-live; workers
//! (uploads, renewals) report when done; the main loop owns standard output and writes whole
//! lines. The app's requests are answered in order; answers to the add-in's own requests
//! (`collab/snapshot`) are matched by id.
//!
//! Mapping (Appendix B): `collab/sync1|sync2|update|awareness` ↔ y-websocket sync step 1, step 2,
//! update and awareness. craft-live's own control messages become `collab/status` (online,
//! peers, "Saved"), `collab/leave` (access refused, the seeder left) and save requests, which the
//! add-in fulfils with `collab/snapshot {file: true}`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value as Json, json};

use crate::link::{self, LinkHandle, NetEvent};
use crate::net;
use crate::store::Account;
use crate::wire::Frame;

pub const MAX_LINE: usize = 64 << 20;
const MAX_UPDATE: usize = (16 << 20) + 64;
const MAX_AWARENESS: usize = (64 << 10) + 64;
const MAX_ROOMS: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// What a share, join or open set up, until the app's `collab/joined`.
#[derive(Clone, Debug)]
pub struct RoomCfg {
    pub room: String,
    /// craft-live's session answer (`token`, `ws`, `user`, `role`, `name`, `format`…).
    pub session: Json,
    pub account: Account,
    pub api: String,
    /// The file in Nextcloud.
    pub path: String,
    pub file_id: String,
    pub etag: String,
    /// The app was asked to seed the room (`mode: share`).
    pub seeding: bool,
}

impl RoomCfg {
    pub fn token(&self) -> String {
        self.session.get("token").and_then(Json::as_str).unwrap_or("").to_string()
    }

    pub fn name(&self) -> String {
        self.session.get("name").and_then(Json::as_str).unwrap_or("").to_string()
    }
}

/// What the main loop waits for.
pub enum Event {
    App(Json),
    AppClosed,
    Net { room: String, epoch: u64, ev: NetEvent },
    Work { room: String, epoch: u64, done: Work },
}

/// Background work that finished.
pub enum Work {
    /// A save in WebDAV mode, or a failed upload: the control message for craft-live.
    SaveResult { result: Json, etag: Option<String>, message: String },
    /// An upload craft-live took.
    Uploaded { message: String },
    /// A new session after the old one expired.
    Renewed(Result<Json, String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Online,
    Offline,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Connecting => "connecting",
            State::Online => "online",
            State::Offline => "offline",
        }
    }
}

struct Room {
    cfg: RoomCfg,
    client_id: u64,
    link: LinkHandle,
    epoch: u64,
    state: State,
    message: Option<String>,
    peers: Vec<Json>,
    saving: bool,
    renewing: bool,
}

/// A room as `status` shows it.
#[derive(Clone, Debug)]
pub struct RoomInfo {
    pub room: String,
    pub state: &'static str,
    pub message: Option<String>,
    pub peers: Vec<Json>,
    pub path: String,
    pub server: String,
    pub role: String,
}

enum Pending {
    Snapshot { room: String, epoch: u64, at: Instant, save: Json },
}

/// What `initialize` said.
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub app: String,
    pub language: String,
    pub features: Vec<String>,
}

impl Session {
    pub fn can_collab(&self) -> bool {
        self.features.iter().any(|f| f == "push") && self.features.iter().any(|f| f == "collab")
    }
}

/// The commands, plugged into the main loop.
pub trait Commands {
    fn run(&mut self, cx: &mut Cx, command: &str, params: &Json, context: &Json) -> Result<Json, (i64, String)>;
}

/// The main loop's state, handed to the commands.
pub struct Cx {
    out: Box<dyn Write>,
    pub session: Session,
    /// Set up by commands, waiting for `collab/joined`.
    pub prepared: HashMap<String, RoomCfg>,
    rooms: BTreeMap<String, Room>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    tx: Sender<Event>,
    epoch: u64,
    broken: bool,
}

fn put(obj: &mut Json, k: &str, v: Json) {
    if let Some(o) = obj.as_object_mut() {
        o.insert(k.into(), v);
    }
}

fn valid_room(r: &str) -> bool {
    !r.is_empty() && r.chars().count() <= 256 && !r.chars().any(char::is_control)
}

fn b64_field(p: &Json, k: &str, max: usize) -> Option<Vec<u8>> {
    let t = p.get(k)?.as_str()?;
    if t.len() > max.saturating_mul(4) / 3 + 8 {
        return None;
    }
    STANDARD.decode(t).ok().filter(|d| d.len() <= max)
}

impl Cx {
    fn new(out: Box<dyn Write>, tx: Sender<Event>) -> Cx {
        Cx {
            out,
            session: Session::default(),
            prepared: HashMap::new(),
            rooms: BTreeMap::new(),
            pending: HashMap::new(),
            next_id: 1,
            tx,
            epoch: 0,
            broken: false,
        }
    }

    fn write(&mut self, msg: &Json) {
        if self.broken {
            return;
        }
        if writeln!(self.out, "{msg}").and_then(|_| self.out.flush()).is_err() {
            self.broken = true;
        }
    }

    pub fn notify(&mut self, method: &str, params: Json) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn log(&self, text: &str) {
        eprintln!("nextcloud-live: {text}");
    }

    pub fn rooms(&self) -> Vec<RoomInfo> {
        self.rooms
            .values()
            .map(|r| RoomInfo {
                room: r.cfg.room.clone(),
                state: r.state.as_str(),
                message: r.message.clone(),
                peers: r.peers.iter().filter_map(|p| p.get("user").cloned()).collect(),
                path: r.cfg.path.clone(),
                server: r.cfg.account.server.clone(),
                role: r.cfg.session.get("role").and_then(Json::as_str).unwrap_or("edit").to_string(),
            })
            .collect()
    }

    /// Leaves a room from the add-in's side.
    pub fn leave(&mut self, room: &str, reason: &str) -> bool {
        let Some(r) = self.rooms.remove(room) else { return false };
        r.link.close();
        self.notify("collab/leave", json!({"room": room, "reason": reason}));
        true
    }

    fn push_status(&mut self, room: &str) {
        let Some(r) = self.rooms.get(room) else { return };
        let peers: Vec<Json> = r
            .peers
            .iter()
            .filter_map(|p| {
                let user = p.get("user")?.clone();
                let mut o = json!({"user": user});
                if let Some(c) = p.get("clientId").and_then(Json::as_u64) {
                    put(&mut o, "clientId", json!(c));
                }
                Some(o)
            })
            .collect();
        let mut params = json!({"room": room, "state": r.state.as_str(), "peers": peers});
        if let Some(m) = &r.message {
            put(&mut params, "message", json!(m));
        }
        self.notify("collab/status", params);
    }

    fn joined(&mut self, params: &Json) {
        let Some(room) = params.get("room").and_then(Json::as_str).filter(|r| valid_room(r)).map(str::to_string) else { return };
        let Some(client_id) = params.get("clientId").and_then(Json::as_u64).filter(|c| *c < (1 << 53)) else { return };
        if let Some(old) = self.rooms.remove(&room) {
            old.link.close();
        }
        if params.get("encrypted").and_then(Json::as_bool) == Some(true) {
            self.notify("collab/leave", json!({"room": room, "reason": "Nextcloud live sessions aren't end-to-end encrypted: craft-live keeps the document so it can save it to Nextcloud."}));
            return;
        }
        let Some(cfg) = self.prepared.get(&room).cloned() else {
            self.notify(
                "collab/leave",
                json!({"room": room, "reason": "Open the file again from Live › Open (Nextcloud): the live session isn't known any more."}),
            );
            return;
        };
        if self.rooms.len() >= MAX_ROOMS {
            self.notify("collab/leave", json!({"room": room, "reason": "Too many live sessions at once: leave one first."}));
            return;
        }
        self.epoch += 1;
        let epoch = self.epoch;
        let tx = self.tx.clone();
        let r2 = room.clone();
        let ws = cfg.session.get("ws").and_then(Json::as_str).unwrap_or("").to_string();
        let started = link::start(ws, cfg.token(), move |ev| tx.send(Event::Net { room: r2.clone(), epoch, ev }).is_ok());
        match started {
            Ok(link) => {
                self.rooms.insert(
                    room.clone(),
                    Room { cfg, client_id, link, epoch, state: State::Connecting, message: None, peers: Vec::new(), saving: false, renewing: false },
                );
                self.push_status(&room);
            }
            Err(e) => self.notify("collab/leave", json!({"room": room, "reason": e})),
        }
    }

    fn on_collab(&mut self, method: &str, params: &Json) {
        if method == "collab/joined" {
            return self.joined(params);
        }
        let Some(room) = params.get("room").and_then(Json::as_str).map(str::to_string) else { return };
        let frame = match method {
            "collab/sync1" => b64_field(params, "stateVector", 1 << 20).map(Frame::Sync1),
            "collab/sync2" => b64_field(params, "update", MAX_UPDATE).map(Frame::Sync2),
            "collab/update" => b64_field(params, "update", MAX_UPDATE).map(Frame::Update),
            "collab/awareness" => b64_field(params, "update", MAX_AWARENESS).map(Frame::Awareness),
            "collab/leave" => {
                if let Some(r) = self.rooms.remove(&room) {
                    r.link.close();
                }
                return;
            }
            _ => None,
        };
        let Some(frame) = frame else {
            self.log(&format!("dropped a malformed {method}"));
            return;
        };
        match self.rooms.get(&room) {
            Some(r) => {
                r.link.send(frame);
            }
            None => self.log(&format!("dropped {method} for a room it isn't in")),
        }
    }

    fn on_control(&mut self, room: &str, text: &str) {
        let Ok(v) = serde_json::from_str::<Json>(text) else { return };
        let kind = v.get("type").and_then(Json::as_str).unwrap_or("").to_string();
        match kind.as_str() {
            "welcome" => {
                let Some(r) = self.rooms.get_mut(room) else { return };
                let seed = v.get("seed").and_then(Json::as_bool).unwrap_or(false);
                let seeded = v.get("seeded").and_then(Json::as_bool).unwrap_or(false);
                if r.cfg.seeding && !seed && !seeded {
                    self.leave(room, "Someone else is opening this file right now: open it again in a moment.");
                    return;
                }
                r.state = State::Online;
                r.message = None;
                let hello = json!({"type": "hello", "clientId": r.client_id, "can": ["upload", "webdav"], "client": concat!("nextcloud-live/", env!("CARGO_PKG_VERSION"))});
                r.link.send(Frame::Control(hello.to_string()));
                self.push_status(room);
            }
            "peers" => {
                if let Some(r) = self.rooms.get_mut(room) {
                    r.peers = v.get("peers").and_then(Json::as_array).cloned().unwrap_or_default().into_iter().take(1000).collect();
                }
                self.push_status(room);
            }
            "save" => self.on_save_request(room, v),
            "saved" => self.set_message(room, Some("Saved to Nextcloud".into())),
            "saveFailed" => {
                let m = v.get("message").and_then(Json::as_str).unwrap_or("").chars().take(300).collect::<String>();
                self.set_message(room, Some(format!("Couldn't save to Nextcloud: {m}")));
            }
            "notice" => {
                let m = v.get("text").and_then(Json::as_str).unwrap_or("").chars().take(500).collect::<String>();
                self.set_message(room, Some(m));
            }
            "reseed" => {
                let m = v.get("message").and_then(Json::as_str).unwrap_or("Open the file again.").chars().take(300).collect::<String>();
                self.leave(room, &m);
            }
            "expired" => self.set_message(room, Some("The session expired; renewing…".into())),
            _ => {}
        }
    }

    fn set_message(&mut self, room: &str, m: Option<String>) {
        if let Some(r) = self.rooms.get_mut(room) {
            r.message = m;
        }
        self.push_status(room);
    }

    /// craft-live wants the file: ask the app for it (`collab/snapshot {file: true}`).
    fn on_save_request(&mut self, room: &str, save: Json) {
        let id = self.next_id;
        let Some(r) = self.rooms.get_mut(room) else { return };
        if r.saving {
            return;
        }
        r.saving = true;
        let epoch = r.epoch;
        self.next_id += 1;
        let mut params = json!({"room": room, "file": true});
        if let Some(f) = save.get("format").and_then(Json::as_str) {
            put(&mut params, "format", json!(f));
        }
        self.pending.insert(id, Pending::Snapshot { room: room.into(), epoch, at: Instant::now(), save });
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": "collab/snapshot", "params": params}));
    }

    fn on_answer(&mut self, id: &Json, msg: &Json) {
        let Some(id) = id.as_u64() else { return };
        let Some(Pending::Snapshot { room, epoch, save, .. }) = self.pending.remove(&id) else { return };
        let Some(r) = self.rooms.get_mut(&room) else { return };
        if r.epoch != epoch {
            return;
        }
        let save_id = save.get("id").and_then(Json::as_u64).unwrap_or(0);
        let result = msg.get("result");
        let file = result.and_then(|x| b64_field(x, "file", usize::try_from(net::MAX_FILE).unwrap_or(usize::MAX))).filter(|f| !f.is_empty());
        let Some(file) = file else {
            r.saving = false;
            let why = msg.get("error").and_then(|e| e.get("message")).and_then(Json::as_str).unwrap_or("the app gave no file bytes").to_string();
            eprintln!("nextcloud-live: no file to save {room}: {why}");
            r.link.send(Frame::Control(json!({"type": "saveResult", "id": save_id, "ok": false, "error": why}).to_string()));
            return;
        };
        let name = result.and_then(|x| x.get("name")).and_then(Json::as_str).map(str::to_string).filter(|n| !n.is_empty()).unwrap_or_else(|| r.cfg.name());
        let mode = save.get("mode").and_then(Json::as_str).unwrap_or("upload").to_string();
        let cfg = r.cfg.clone();
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new().name("live-save".into()).spawn(move || {
            let done = if mode == "webdav" {
                // The etag the room is based on (empty: craft-live doesn't know it; save over).
                let etag = save.get("etag").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| cfg.etag.clone());
                save_webdav(&cfg, save_id, &name, &etag, file)
            } else {
                save_upload(&cfg, &save, save_id, &name, file)
            };
            let _ = tx.send(Event::Work { room: cfg.room.clone(), epoch, done });
        });
        if spawned.is_err() {
            r.saving = false;
        }
    }

    fn on_work(&mut self, room: &str, epoch: u64, done: Work) {
        let Some(r) = self.rooms.get_mut(room) else { return };
        if r.epoch != epoch {
            return;
        }
        match done {
            Work::SaveResult { result, etag, message } => {
                r.saving = false;
                if let Some(e) = etag {
                    r.cfg.etag = e;
                }
                r.link.send(Frame::Control(result.to_string()));
                r.message = Some(message);
                self.push_status(room);
            }
            Work::Uploaded { message } => {
                r.saving = false;
                r.message = Some(message);
                self.push_status(room);
            }
            Work::Renewed(Ok(s)) => {
                r.renewing = false;
                let same_room = s.get("room").and_then(Json::as_str) == Some(room);
                if !same_room || s.get("seed").and_then(Json::as_bool) == Some(true) {
                    self.leave(room, "The live session ended on the server: open the file again.");
                    return;
                }
                r.cfg.session = s;
                let token = r.cfg.token();
                r.link.token(token);
                if let Some(c) = self.prepared.get_mut(room) {
                    c.session = r.cfg.session.clone();
                }
            }
            Work::Renewed(Err(e)) => {
                r.renewing = false;
                self.leave(room, &format!("The live session ended and couldn't be renewed: {e}"));
            }
        }
    }

    fn on_net(&mut self, room: &str, epoch: u64, ev: NetEvent) {
        let Some(r) = self.rooms.get_mut(room) else { return };
        if r.epoch != epoch {
            return;
        }
        let enc = |b: &[u8]| STANDARD.encode(b);
        match ev {
            NetEvent::Connected => {
                // Online once craft-live's welcome arrives.
            }
            NetEvent::Disconnected(m) => {
                if r.state != State::Offline {
                    r.state = State::Offline;
                    r.message = Some(format!("Reconnecting… ({m})"));
                    r.saving = false;
                    self.push_status(room);
                }
            }
            NetEvent::Rejected(status) => {
                if r.renewing {
                    return;
                }
                r.renewing = true;
                r.state = State::Offline;
                r.message = Some(format!("The session was refused ({status}); renewing…"));
                let cfg = r.cfg.clone();
                let tx = self.tx.clone();
                let room2 = room.to_string();
                let _ = std::thread::Builder::new().name("live-renew".into()).spawn(move || {
                    let res = net::create_session(&cfg.api, &cfg.account, &cfg.path, &cfg.file_id).map_err(|e| e.text());
                    let _ = tx.send(Event::Work { room: room2, epoch, done: Work::Renewed(res) });
                });
                self.push_status(room);
            }
            NetEvent::Frame(f) => match f {
                Frame::Sync1(sv) => self.notify("collab/sync1", json!({"room": room, "stateVector": enc(&sv)})),
                // craft-live answers a sync step 1 from the room's stored state.
                Frame::Sync2(u) => self.notify("collab/sync2", json!({"room": room, "update": enc(&u), "snapshot": true})),
                Frame::Update(u) => self.notify("collab/update", json!({"room": room, "update": enc(&u)})),
                Frame::Awareness(a) => self.notify("collab/awareness", json!({"room": room, "update": enc(&a)})),
                Frame::Denied(reason) => {
                    let reason: String = reason.chars().take(300).collect();
                    self.leave(room, &format!("craft-live refused the session: {reason}"));
                }
                Frame::Control(text) => self.on_control(room, &text),
                Frame::QueryAwareness => {}
            },
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let mut expired = Vec::new();
        self.pending.retain(|_, p| {
            let Pending::Snapshot { room, at, .. } = p;
            let keep = now.duration_since(*at) < SNAPSHOT_TIMEOUT;
            if !keep {
                expired.push(room.clone());
            }
            keep
        });
        for room in expired {
            if let Some(r) = self.rooms.get_mut(&room) {
                r.saving = false;
            }
        }
    }

    fn close_all(&mut self) {
        for (_, r) in std::mem::take(&mut self.rooms) {
            r.link.close();
        }
    }

    fn initialize(&mut self, params: &Json) -> Json {
        let host = params.get("host");
        let text = |k: &str| host.and_then(|h| h.get(k)).and_then(Json::as_str).map(|s| s.chars().take(64).collect::<String>());
        if let Some(id) = text("id") {
            self.session.app = id.to_ascii_lowercase();
        }
        if let Some(l) = text("language") {
            self.session.language = l;
        }
        self.session.features = host
            .and_then(|h| h.get("features"))
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(Json::as_str).take(64).map(|x| x.chars().take(64).collect()).collect())
            .unwrap_or_default();
        json!({"protocolVersion": 1, "features": ["push", "collab"]})
    }

    fn on_app<C: Commands>(&mut self, cmds: &mut C, msg: Json) -> bool {
        let method = msg.get("method").and_then(Json::as_str).map(str::to_string);
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        let params = msg.get("params").cloned().unwrap_or(Json::Null);
        match (method, id) {
            (Some(method), Some(id)) => {
                let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match method.as_str() {
                    "initialize" => Ok(self.initialize(&params)),
                    "commands/run" => {
                        let command = params.get("command").and_then(Json::as_str).unwrap_or("").to_string();
                        let p = params.get("params").cloned().filter(Json::is_object).unwrap_or_else(|| json!({}));
                        let ctx = params.get("context").cloned().unwrap_or(Json::Null);
                        cmds.run(self, &command, &p, &ctx)
                    }
                    _ => Err((-32601, format!("nextcloud-live has no method {method}"))),
                }))
                .unwrap_or_else(|_| Err((1, "nextcloud-live hit an internal error".into())));
                let reply = match answer {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
                };
                self.write(&reply);
            }
            (Some(method), None) => {
                if method == "shutdown" || method == "exit" {
                    return false;
                }
                if method.starts_with("collab/") && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.on_collab(&method, &params))).is_err() {
                    self.log(&format!("internal error on {method}"));
                }
            }
            (None, Some(id)) => self.on_answer(&id, &msg),
            (None, None) => {}
        }
        true
    }
}

/// Uploads the file to craft-live (it writes it to Nextcloud with WOPI).
fn save_upload(cfg: &RoomCfg, save: &Json, save_id: u64, name: &str, file: Vec<u8>) -> Work {
    let url = save.get("url").and_then(Json::as_str).unwrap_or("");
    // Only to the craft-live this session belongs to.
    if !url.starts_with(&cfg.api.replace("/api/v1", "")) {
        return Work::SaveResult {
            result: json!({"type": "saveResult", "id": save_id, "ok": false, "error": "upload address isn't craft-live's"}),
            etag: None,
            message: "Couldn't save: a bad upload address".into(),
        };
    }
    match net::upload_save(url, &cfg.token(), name, file) {
        Ok(_) => Work::Uploaded { message: "Saved to Nextcloud".into() },
        Err(e) => Work::SaveResult {
            result: json!({"type": "saveResult", "id": save_id, "ok": false, "error": e.text()}),
            etag: None,
            message: format!("Couldn't save: {}", e.text()),
        },
    }
}

/// Saves through the person's own WebDAV access (no WOPI token in the room): over the version
/// it knows, else as a copy.
fn save_webdav(cfg: &RoomCfg, save_id: u64, name: &str, etag: &str, file: Vec<u8>) -> Work {
    let nc = net::Nc::new(&cfg.account, Duration::from_secs(300));
    let ext = |n: &str| n.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let same_format = ext(name).is_empty() || ext(name) == ext(&cfg.path);
    if same_format {
        match nc.upload(&cfg.path, file.clone(), Some(etag), false) {
            Ok(etag) => {
                return Work::SaveResult {
                    result: json!({"type": "saveResult", "id": save_id, "ok": true, "version": etag}),
                    etag: Some(etag),
                    message: "Saved to Nextcloud".into(),
                };
            }
            Err(net::Fail::Status(412, _)) => {}
            Err(e) => {
                return Work::SaveResult {
                    result: json!({"type": "saveResult", "id": save_id, "ok": false, "error": e.text()}),
                    etag: None,
                    message: format!("Couldn't save: {}", e.text()),
                };
            }
        }
    }
    // The file changed in Nextcloud (or the app saved another format): a copy beside it.
    let dir = cfg.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let stem = cfg.path.rsplit('/').next().and_then(|n| n.rsplit_once('.').map(|(s, _)| s.to_string())).unwrap_or_else(|| "Document".into());
    let e = if ext(name).is_empty() { ext(&cfg.path) } else { ext(name) };
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let copy = format!("{stem} (live copy {secs}).{e}");
    match nc.upload(&format!("{dir}/{copy}"), file, None, true) {
        Ok(_) => Work::SaveResult {
            result: json!({"type": "saveResult", "id": save_id, "ok": true, "copy": copy}),
            etag: None,
            message: format!("The file changed in Nextcloud: saved as “{copy}”"),
        },
        Err(e) => Work::SaveResult {
            result: json!({"type": "saveResult", "id": save_id, "ok": false, "error": e.text()}),
            etag: None,
            message: format!("Couldn't save: {}", e.text()),
        },
    }
}

fn read_lines<R: Read>(input: R, tx: Sender<Event>) {
    let mut reader = BufReader::with_capacity(1 << 16, input);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE && line.last() != Some(&b'\n') {
            eprintln!("nextcloud-live: dropped a line over {} MiB", MAX_LINE >> 20);
            loop {
                let buf = match reader.fill_buf() {
                    Ok([]) | Err(_) => break,
                    Ok(b) => b,
                };
                match buf.iter().position(|b| *b == b'\n') {
                    Some(i) => {
                        reader.consume(i + 1);
                        break;
                    }
                    None => {
                        let n = buf.len();
                        reader.consume(n);
                    }
                }
            }
            continue;
        }
        match serde_json::from_slice::<Json>(&line) {
            Ok(msg) if msg.is_object() => {
                if tx.send(Event::App(msg)).is_err() {
                    return;
                }
            }
            _ => {
                if !String::from_utf8_lossy(&line).trim().is_empty() {
                    eprintln!("nextcloud-live: ignored a line that isn't a JSON object");
                }
            }
        }
    }
    let _ = tx.send(Event::AppClosed);
}

/// Serves the app until its stream closes or it says `shutdown`.
pub fn serve<C: Commands, R: Read + Send + 'static, W: Write + 'static>(mut cmds: C, input: R, output: W) {
    let (tx, rx) = mpsc::channel();
    let reader_tx = tx.clone();
    if std::thread::Builder::new().name("app-reader".into()).spawn(move || read_lines(input, reader_tx)).is_err() {
        eprintln!("nextcloud-live: can't start a thread");
        return;
    }
    let mut cx = Cx::new(Box::new(output), tx);
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::App(msg)) => {
                if !cx.on_app(&mut cmds, msg) {
                    break;
                }
            }
            Ok(Event::AppClosed) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::Net { room, epoch, ev }) => cx.on_net(&room, epoch, ev),
            Ok(Event::Work { room, epoch, done }) => cx.on_work(&room, epoch, done),
            Err(RecvTimeoutError::Timeout) => {}
        }
        cx.tick();
        if cx.broken {
            break;
        }
    }
    cx.close_all();
}
