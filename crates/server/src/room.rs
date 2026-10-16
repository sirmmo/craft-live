//! Rooms: one per file, each an actor task that owns the shared Yjs document, the connections,
//! awareness, the seed lease, saving, the WOPI lock and persistence (docs/design.md, "Rooms").
//!
//! The server reads the document only as generic Yjs (the `yrs` crate): it answers sync step 1
//! with what the asker lacks, applies updates and relays them, keeps the whole state on disk,
//! and never interprets the apps' schemas. Turning the document into file bytes (and file bytes
//! into a document) is the apps' job: the first editor seeds the room from the file, and a
//! connected editor hands over the file bytes when the room saves.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, oneshot};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, StateVector, Transact, Update};

use crate::config::{self, Config};
use crate::ids;
use crate::session::{Kind, Role, Session, Sessions};
use crate::store::{self, RoomMeta};
use crate::wopi::{Wopi, WopiError};
use crate::ywire::{self, AwarenessEntry, Frame};

/// Largest update (protocol 2, section 7) and awareness update, before framing.
pub const MAX_UPDATE: usize = (16 << 20) + 64;
pub const MAX_AWARENESS: usize = (64 << 10) + 64;
/// A state vector is a few bytes per client that ever edited.
const MAX_STATE_VECTOR: usize = 1 << 20;
/// Messages queued for one connection before it counts as stuck and is closed.
pub const OUT_QUEUE: usize = 4096;
/// How often a room looks at its timers.
const TICK: Duration = Duration::from_millis(100);
/// How often the state is written to disk at most while people edit.
const PERSIST_EVERY: Duration = Duration::from_secs(2);

/// What the server shares with every room.
pub struct Ctx {
    pub cfg: Config,
    pub wopi: Wopi,
    pub sessions: Sessions,
    pub server_id: String,
}

impl Ctx {
    fn data_dir(&self) -> PathBuf {
        self.cfg.data_dir.clone()
    }
}

/// What a connection's writer sends.
#[derive(Debug)]
pub enum Out {
    Frame(Vec<u8>),
    /// Close with a WebSocket close code (4xxx: craft-live's own) and reason.
    Close(u16, String),
}

/// The answer to [`Msg::Prepare`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// This session holds the seed lease: its client must load the file and seed the room.
    pub seed: bool,
    pub seeded: bool,
}

/// A save's outcome, for the client that uploaded the file.
pub type UploadReply = oneshot::Sender<Result<Json, (u16, String)>>;

/// What a room hears.
pub enum Msg {
    /// A session is about to connect: decides the seed lease, checks the stored state against
    /// the file's current `version`, and starts the WOPI lock.
    Prepare {
        session: Arc<Session>,
        version: Option<String>,
        etag: Option<String>,
        supports_locks: bool,
        reply: oneshot::Sender<Prepared>,
    },
    Join {
        conn: u64,
        session: Arc<Session>,
        out: mpsc::Sender<Out>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Frame {
        conn: u64,
        frame: Frame,
    },
    Leave {
        conn: u64,
    },
    /// File bytes for save `save`, from the client the room asked.
    Upload {
        save: u64,
        session: Arc<Session>,
        name: String,
        body: Vec<u8>,
        reply: UploadReply,
    },
    /// A save the person asked for (Nextcloud's `Action_Save`, a client's `saveNow`).
    SaveNow {
        force: bool,
    },
    Status {
        reply: oneshot::Sender<Json>,
    },
    /// Write the state to disk now; with `close`, also close every connection (the server is
    /// stopping: clients reconnect to the next one).
    Flush {
        close: bool,
        reply: oneshot::Sender<()>,
    },
    Done(Job),
}

/// Results of work a room started in the background.
pub enum Job {
    Locked { result: Result<(), WopiError>, lock: String },
    Refreshed(Result<(), WopiError>),
    Unlocked,
    Saved { save: u64, outcome: SaveOutcome, updates_at: u64, reply: Option<UploadReply> },
    Persisted,
}

/// How a save ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written to the file (WOPI); its new version.
    Saved(String),
    /// Written by a desktop client through WebDAV; the file's new etag.
    SavedDav(String),
    /// The file changed outside the room (or is locked by someone else): the room was saved as
    /// a copy with this name instead.
    Copy {
        name: String,
        why: String,
    },
    /// Every token the room had was refused.
    Unauthorized,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LockState {
    None,
    Pending,
    Held(String),
    /// Someone else's lock (another editor, a person in Nextcloud).
    Foreign(String),
    /// The host doesn't do WOPI locks.
    Unsupported,
    Releasing,
}

struct Conn {
    session: Arc<Session>,
    out: mpsc::Sender<Out>,
    /// Its Yjs client id (from `hello`).
    client_id: Option<u64>,
    /// `upload`, `webdav`.
    can: Vec<String>,
    /// It has the document (or is seeding it): it gets updates.
    synced: bool,
    /// Its sync step 1, waiting for the room to be seeded.
    pending_sync1: Option<Vec<u8>>,
    /// The awareness client ids it owns.
    aware: HashSet<u64>,
    last_update: Option<Instant>,
}

struct Lease {
    token: String,
    conn: Option<u64>,
    expires: Instant,
}

struct SaveJob {
    id: u64,
    conn: u64,
    started: Instant,
    autosave: bool,
    /// Write over the file even if it changed outside (the person said so).
    force: bool,
}

struct Room {
    id: String,
    ctx: Arc<Ctx>,
    registry: Registry,
    me: mpsc::Sender<Msg>,
    doc: Doc,
    meta: RoomMeta,
    conns: BTreeMap<u64, Conn>,
    awareness: HashMap<u64, AwarenessEntry>,
    lease: Option<Lease>,
    /// Web sessions whose WOPI token the room may use (newest last).
    tokens: Vec<Arc<Session>>,
    bad_tokens: HashSet<String>,
    supports_locks: bool,
    lock: LockState,
    last_lock: Instant,
    updates_total: u64,
    updates_since_save: u32,
    last_update: Instant,
    last_save: Instant,
    save: Option<SaveJob>,
    save_wanted: Option<bool>,
    next_save: u64,
    retry_at: Option<Instant>,
    paused: bool,
    persist_due: bool,
    persisting: bool,
    last_persist: Instant,
    empty_since: Option<Instant>,
    dropped: u64,
}

/// The rooms, by id.
#[derive(Clone)]
pub struct Registry {
    map: Arc<Mutex<HashMap<String, mpsc::Sender<Msg>>>>,
    ctx: Arc<Ctx>,
}

fn apply_update(doc: &Doc, update: &[u8]) -> Result<(), String> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = Update::decode_v1(update).map_err(|e| e.to_string())?;
        doc.transact_mut().apply_update(u).map_err(|e| e.to_string())
    }))
    .map_err(|_| "the update broke the decoder".to_string())?
}

fn diff(doc: &Doc, sv: &[u8]) -> Result<Vec<u8>, String> {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let sv = StateVector::decode_v1(sv).map_err(|e| e.to_string())?;
        Ok(doc.transact().encode_diff_v1(&sv))
    }))
    .map_err(|_| "the state vector broke the decoder".to_string())?
}

fn whole_state(doc: &Doc) -> Vec<u8> {
    std::panic::catch_unwind(AssertUnwindSafe(|| doc.transact().encode_state_as_update_v1(&StateVector::default()))).unwrap_or_default()
}

fn state_vector(doc: &Doc) -> Vec<u8> {
    std::panic::catch_unwind(AssertUnwindSafe(|| doc.transact().state_vector().encode_v1())).unwrap_or_default()
}

fn doc_is_empty(doc: &Doc) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| doc.transact().state_vector().is_empty())).unwrap_or(true)
}

/// `Budget (live copy 2026-10-16 18-02).xlsx`.
pub fn copy_name(name: &str, ext: &str, now_ms: i64) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let stem = if stem.is_empty() { "Document" } else { stem };
    format!("{stem} (live copy {}).{ext}", ids::stamp(now_ms))
}

impl Registry {
    pub fn new(ctx: Arc<Ctx>) -> Registry {
        Registry { map: Arc::new(Mutex::new(HashMap::new())), ctx }
    }

    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }

    /// The room's inbox, starting the room (from its stored state) when it isn't running.
    fn get(&self, room: &str) -> Result<mpsc::Sender<Msg>, String> {
        let mut map = self.map.lock().map_err(|_| "rooms unavailable".to_string())?;
        if let Some(tx) = map.get(room).filter(|t| !t.is_closed()) {
            return Ok(tx.clone());
        }
        map.retain(|_, t| !t.is_closed());
        if map.len() >= self.ctx.cfg.limits.max_rooms {
            return Err("craft-live has too many open documents; try again later".into());
        }
        let (tx, rx) = mpsc::channel(1024);
        let room = Room::load(room, self.ctx.clone(), self.clone(), tx.clone());
        map.insert(room.id.clone(), tx.clone());
        tokio::spawn(room.run(rx));
        Ok(tx)
    }

    fn remove_if(&self, room: &str, tx: &mpsc::Sender<Msg>) {
        if let Ok(mut map) = self.map.lock()
            && map.get(room).is_some_and(|t| t.same_channel(tx))
        {
            map.remove(room);
        }
    }

    /// Sends a message that has a reply, retrying once if the room was closing.
    pub async fn call<T>(&self, room: &str, make: impl Fn(oneshot::Sender<T>) -> Msg) -> Result<T, String> {
        for _ in 0..3 {
            let tx = self.get(room)?;
            let (reply, rx) = oneshot::channel();
            if tx.send(make(reply)).await.is_err() {
                continue;
            }
            if let Ok(v) = rx.await {
                return Ok(v);
            }
        }
        Err("the document's room isn't answering".into())
    }

    /// Sends a message without a reply (to a running room only).
    pub async fn send(&self, room: &str, msg: Msg) {
        let tx = self.map.lock().ok().and_then(|m| m.get(room).cloned());
        if let Some(tx) = tx {
            let _ = tx.send(msg).await;
        }
    }

    /// Every open room writes its state to disk (and, with `close`, closes its connections).
    pub async fn flush_all(&self, close: bool) {
        let all: Vec<mpsc::Sender<Msg>> = self.map.lock().map(|m| m.values().cloned().collect()).unwrap_or_default();
        for tx in all {
            let (reply, rx) = oneshot::channel();
            if tx.send(Msg::Flush { close, reply }).await.is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(30), rx).await;
            }
        }
    }

    pub fn open_rooms(&self) -> usize {
        self.map.lock().map(|m| m.values().filter(|t| !t.is_closed()).count()).unwrap_or(0)
    }
}

impl Room {
    fn load(id: &str, ctx: Arc<Ctx>, registry: Registry, me: mpsc::Sender<Msg>) -> Room {
        let doc = Doc::new();
        let mut meta = RoomMeta { room: id.to_string(), ..RoomMeta::default() };
        match store::load_room(&ctx.data_dir(), id) {
            Ok(Some((m, state))) => {
                if !state.is_empty()
                    && let Err(e) = apply_update(&doc, &state)
                {
                    tracing::error!(room = id, "the stored state doesn't load ({e}); the room starts empty");
                } else {
                    meta = m;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!(room = id, "can't read the stored room: {e}"),
        }
        if doc_is_empty(&doc) {
            meta.seeded = false;
        }
        let now = Instant::now();
        Room {
            id: id.to_string(),
            ctx,
            registry,
            me,
            doc,
            meta,
            conns: BTreeMap::new(),
            awareness: HashMap::new(),
            lease: None,
            tokens: Vec::new(),
            bad_tokens: HashSet::new(),
            supports_locks: false,
            lock: LockState::None,
            last_lock: now,
            updates_total: 0,
            updates_since_save: 0,
            last_update: now,
            last_save: now,
            save: None,
            save_wanted: None,
            next_save: 1,
            retry_at: None,
            paused: false,
            persist_due: false,
            persisting: false,
            last_persist: now,
            empty_since: Some(now),
            dropped: 0,
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => self.on_msg(m),
                    None => break,
                },
                _ = tick.tick() => {
                    if self.tick() {
                        break;
                    }
                }
            }
        }
        self.registry.remove_if(&self.id, &self.me);
        rx.close();
        // Anything that arrived meanwhile: answer what waits for an answer.
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Upload { reply, .. } => {
                    let _ = reply.send(Err((503, "the room closed".into())));
                }
                Msg::Join { reply, .. } => {
                    let _ = reply.send(Err("the room closed; connect again".into()));
                }
                _ => {}
            }
        }
        self.persist_now();
        tracing::info!(room = %self.id, "room closed");
    }

    // ----- Sending -----

    fn send_to(&mut self, conn: u64, frame: &Frame) {
        let Some(c) = self.conns.get(&conn) else { return };
        if c.out.try_send(Out::Frame(frame.encode())).is_err() {
            tracing::warn!(room = %self.id, conn, "a connection fell behind; closing it");
            let _ = c.out.try_send(Out::Close(4408, "too slow".into()));
            self.drop_conn(conn);
        }
    }

    fn control(&mut self, conn: u64, v: Json) {
        self.send_to(conn, &Frame::Control(v.to_string()));
    }

    fn broadcast(&mut self, frame: &Frame, except: Option<u64>, synced_only: bool) {
        let targets: Vec<u64> = self.conns.iter().filter(|(id, c)| Some(**id) != except && (!synced_only || c.synced)).map(|(id, _)| *id).collect();
        for t in targets {
            self.send_to(t, frame);
        }
    }

    fn broadcast_control(&mut self, v: Json) {
        self.broadcast(&Frame::Control(v.to_string()), None, false);
    }

    fn notice(&mut self, level: &str, text: &str) {
        tracing::info!(room = %self.id, "{text}");
        self.broadcast_control(json!({"type": "notice", "level": level, "text": text}));
    }

    fn send_peers(&mut self) {
        let list: Vec<(u64, Json)> = self
            .conns
            .iter()
            .map(|(id, c)| {
                let mut p = json!({"user": c.session.user.json(), "role": c.session.role.as_str(), "kind": c.session.kind});
                if let (Some(cid), Some(o)) = (c.client_id, p.as_object_mut()) {
                    o.insert("clientId".into(), json!(cid));
                }
                (*id, p)
            })
            .collect();
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for to in ids {
            let others: Vec<&Json> = list.iter().filter(|(id, _)| *id != to).map(|(_, p)| p).collect();
            self.control(to, json!({"type": "peers", "peers": others}));
        }
    }

    // ----- Messages -----

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Prepare { session, version, etag, supports_locks, reply } => {
                let p = self.prepare(&session, version, etag, supports_locks);
                let _ = reply.send(p);
            }
            Msg::Join { conn, session, out, reply } => {
                let r = self.join(conn, session, out);
                let _ = reply.send(r);
            }
            Msg::Frame { conn, frame } => self.on_frame(conn, frame),
            Msg::Leave { conn } => self.drop_conn(conn),
            Msg::Upload { save, session, name, body, reply } => self.on_upload(save, &session, name, body, reply),
            Msg::SaveNow { force } => {
                if force {
                    self.paused = false;
                }
                self.save_wanted = Some(force || self.save_wanted.unwrap_or(false));
                self.retry_at = None;
            }
            Msg::Status { reply } => {
                let _ = reply.send(self.status());
            }
            Msg::Flush { close, reply } => {
                self.persist_now();
                if close {
                    for c in self.conns.values() {
                        let _ = c.out.try_send(Out::Close(1012, "craft-live is restarting".into()));
                    }
                }
                let _ = reply.send(());
            }
            Msg::Done(job) => self.on_job(job),
        }
    }

    fn status(&self) -> Json {
        json!({
            "room": self.id,
            "seeded": self.meta.seeded,
            "dirty": self.meta.dirty,
            "version": self.meta.version,
            "connections": self.conns.len(),
            "lock": match &self.lock {
                LockState::None => "none", LockState::Pending => "pending", LockState::Held(_) => "held",
                LockState::Foreign(_) => "foreign", LockState::Unsupported => "unsupported", LockState::Releasing => "releasing",
            },
            "saving": self.save.is_some(),
            "paused": self.paused,
            "updates": self.updates_total,
            "dropped": self.dropped,
        })
    }

    fn prepare(&mut self, s: &Arc<Session>, version: Option<String>, etag: Option<String>, supports_locks: bool) -> Prepared {
        let now = Instant::now();
        if !self.meta.seeded
            && let Some(e) = etag
        {
            self.meta.etag = e;
        }
        // A file changed outside while nobody was in the room: the stored state is stale.
        if self.conns.is_empty()
            && let Some(v) = version.as_deref().filter(|v| !v.is_empty())
        {
            if self.meta.seeded && !self.meta.version.is_empty() && self.meta.version != v {
                if self.meta.dirty {
                    let state = whole_state(&self.doc);
                    match store::orphan_room(&self.ctx.data_dir(), &self.id, &state, ids::now_ms()) {
                        Ok(p) => {
                            tracing::warn!(room = %self.id, "the file changed outside while the room had unsaved edits; they were set aside in {}", p.display())
                        }
                        Err(e) => tracing::error!(room = %self.id, "the file changed outside; couldn't set the unsaved edits aside: {e}"),
                    }
                } else {
                    tracing::info!(room = %self.id, "the file changed outside; the room loads it again");
                }
                self.doc = Doc::new();
                self.meta.seeded = false;
                self.meta.dirty = false;
                self.awareness.clear();
                self.persist_due = true;
            }
            if !self.meta.seeded {
                self.meta.version = v.to_string();
            }
        }
        if s.kind == Kind::Web && s.role == Role::Edit && s.wopi_usable(ids::now_ms()) {
            self.tokens.retain(|t| t.token != s.token);
            self.tokens.push(s.clone());
            if self.tokens.len() > 64 {
                self.tokens.remove(0);
            }
            self.meta.wopi_src = s.wopi_src.clone();
            self.supports_locks |= supports_locks;
            if self.supports_locks && matches!(self.lock, LockState::None) {
                self.start_lock();
            }
        }
        if self.meta.name.is_empty() {
            self.meta.name = s.file_name.clone();
        }
        self.empty_since = if self.conns.is_empty() { Some(now) } else { None };
        if self.meta.seeded {
            return Prepared { seed: false, seeded: true };
        }
        if let Some(l) = &self.lease
            && l.conn.is_none()
            && now >= l.expires
        {
            self.lease = None;
        }
        let seed = match &mut self.lease {
            Some(l) if l.token == s.token => {
                l.expires = now + Duration::from_secs(self.ctx.cfg.limits.seed_lease_secs);
                true
            }
            Some(_) => false,
            None if s.role == Role::Edit => {
                self.lease = Some(Lease { token: s.token.clone(), conn: None, expires: now + Duration::from_secs(self.ctx.cfg.limits.seed_lease_secs) });
                true
            }
            None => false,
        };
        Prepared { seed, seeded: false }
    }

    fn join(&mut self, conn: u64, session: Arc<Session>, out: mpsc::Sender<Out>) -> Result<(), String> {
        if session.room != self.id {
            return Err("this session is for another document".into());
        }
        if self.conns.len() >= self.ctx.cfg.limits.max_connections_per_room {
            return Err("too many people are in this document".into());
        }
        let seeder = self.lease.as_ref().is_some_and(|l| l.token == session.token && (l.conn.is_none() || l.conn == Some(conn)));
        if seeder && let Some(l) = &mut self.lease {
            l.conn = Some(conn);
        }
        let synced = self.meta.seeded || seeder;
        self.conns.insert(
            conn,
            Conn { session: session.clone(), out, client_id: None, can: Vec::new(), synced, pending_sync1: None, aware: HashSet::new(), last_update: None },
        );
        self.empty_since = None;
        let welcome = json!({
            "type": "welcome",
            "room": self.id,
            "user": session.user.json(),
            "role": session.role.as_str(),
            "app": session.app,
            "fileName": if self.meta.name.is_empty() { session.file_name.clone() } else { self.meta.name.clone() },
            "seeded": self.meta.seeded,
            "seed": seeder && !self.meta.seeded,
            "modified": self.meta.dirty,
            "server": format!("craft-live/{}", env!("CARGO_PKG_VERSION")),
        });
        self.control(conn, welcome);
        if synced {
            let sv = state_vector(&self.doc);
            self.send_to(conn, &Frame::Sync1(sv));
        }
        // Everyone's awareness for the newcomer.
        if !self.awareness.is_empty() {
            let all: Vec<AwarenessEntry> = self.awareness.values().cloned().collect();
            self.send_to(conn, &Frame::Awareness(ywire::encode_awareness(&all)));
        }
        self.send_peers();
        tracing::info!(room = %self.id, conn, user = %session.user.id, role = session.role.as_str(), seeder, "joined");
        Ok(())
    }

    fn drop_conn(&mut self, conn: u64) {
        let Some(c) = self.conns.remove(&conn) else { return };
        if c.session.kind == Kind::Desktop || c.session.kind == Kind::Web {
            tracing::info!(room = %self.id, conn, user = %c.session.user.id, "left");
        }
        if let Some(job) = &self.save
            && job.conn == conn
        {
            self.save = None;
            self.retry_at = None;
        }
        // Its awareness goes: tell the others the states are gone.
        let gone: Vec<AwarenessEntry> = c
            .aware
            .iter()
            .filter_map(|id| self.awareness.remove(id))
            .map(|e| AwarenessEntry { client: e.client, clock: e.clock.saturating_add(1), state: "null".into() })
            .collect();
        if !gone.is_empty() {
            self.broadcast(&Frame::Awareness(ywire::encode_awareness(&gone)), None, false);
        }
        let lost_seeder = self.lease.as_ref().is_some_and(|l| l.conn == Some(conn));
        if lost_seeder && !self.meta.seeded {
            self.lease = None;
            let waiting: Vec<u64> = self.conns.keys().copied().collect();
            for w in waiting {
                self.control(w, json!({"type": "reseed", "message": "The person loading the file left before it was ready. Open the file again."}));
            }
        }
        if self.conns.is_empty() {
            self.empty_since = Some(Instant::now());
        }
        self.send_peers();
    }

    fn on_frame(&mut self, conn: u64, frame: Frame) {
        let Some(c) = self.conns.get(&conn) else { return };
        let role = c.session.role;
        let synced = c.synced;
        match frame {
            Frame::Sync1(sv) => {
                if sv.len() > MAX_STATE_VECTOR {
                    return self.dropped_input(conn, "state vector too large");
                }
                if !synced {
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.pending_sync1 = Some(sv);
                    }
                    return;
                }
                match diff(&self.doc, &sv) {
                    Ok(d) => self.send_to(conn, &Frame::Sync2(d)),
                    Err(e) => self.dropped_input(conn, &e),
                }
            }
            Frame::Sync2(u) | Frame::Update(u) => self.on_update(conn, role, synced, u),
            Frame::Awareness(a) => self.on_awareness(conn, a),
            Frame::QueryAwareness => {
                let all: Vec<AwarenessEntry> = self.awareness.values().cloned().collect();
                self.send_to(conn, &Frame::Awareness(ywire::encode_awareness(&all)));
            }
            Frame::Control(text) => self.on_control(conn, &text),
            Frame::Denied(_) => {}
        }
    }

    fn dropped_input(&mut self, conn: u64, why: &str) {
        self.dropped = self.dropped.saturating_add(1);
        tracing::debug!(room = %self.id, conn, "dropped a message: {why}");
    }

    fn on_update(&mut self, conn: u64, role: Role, synced: bool, update: Vec<u8>) {
        if update.len() > MAX_UPDATE {
            return self.dropped_input(conn, "update too large");
        }
        if role == Role::View {
            return self.dropped_input(conn, "an update from a viewer");
        }
        if !synced {
            return self.dropped_input(conn, "an update before the room has its document");
        }
        // An empty update (no structs, no deletions) changes nothing.
        if update.len() <= 2 && update.iter().all(|b| *b == 0) {
            return;
        }
        if let Err(e) = apply_update(&self.doc, &update) {
            return self.dropped_input(conn, &e);
        }
        if !self.meta.seeded {
            // Only the seeder is synced before the room has its document.
            if !doc_is_empty(&self.doc) {
                self.seeded();
            }
            return;
        }
        let now = Instant::now();
        self.updates_total = self.updates_total.saturating_add(1);
        self.updates_since_save = self.updates_since_save.saturating_add(1);
        self.last_update = now;
        self.persist_due = true;
        if let Some(c) = self.conns.get_mut(&conn) {
            c.last_update = Some(now);
        }
        if !self.meta.dirty {
            self.meta.dirty = true;
            self.broadcast_control(json!({"type": "modified", "modified": true}));
        }
        self.broadcast(&Frame::Update(update), Some(conn), true);
    }

    /// The seeder's document arrived: everyone waiting gets it.
    fn seeded(&mut self) {
        self.meta.seeded = true;
        self.meta.seeded_ms = ids::now_ms();
        self.lease = None;
        // A freshly seeded room matches the file: nothing to save yet.
        self.meta.dirty = false;
        self.updates_since_save = 0;
        self.persist_due = true;
        self.last_persist = Instant::now() - PERSIST_EVERY;
        tracing::info!(room = %self.id, "seeded");
        let waiting: Vec<(u64, Option<Vec<u8>>)> = self.conns.iter_mut().filter(|(_, c)| !c.synced).map(|(id, c)| (*id, c.pending_sync1.take())).collect();
        for (id, sv) in waiting {
            if let Some(c) = self.conns.get_mut(&id) {
                c.synced = true;
            }
            let update = match sv {
                Some(sv) => diff(&self.doc, &sv).unwrap_or_else(|_| whole_state(&self.doc)),
                None => whole_state(&self.doc),
            };
            self.send_to(id, &Frame::Sync2(update));
            let sv = state_vector(&self.doc);
            self.send_to(id, &Frame::Sync1(sv));
        }
        self.broadcast_control(json!({"type": "seeded", "modified": false}));
    }

    fn on_awareness(&mut self, conn: u64, data: Vec<u8>) {
        if data.len() > MAX_AWARENESS {
            return self.dropped_input(conn, "awareness too large");
        }
        let Some(entries) = ywire::parse_awareness(&data) else { return self.dropped_input(conn, "bad awareness") };
        // A connection speaks only for the client ids it owns (the first to claim one owns it).
        let owned_elsewhere = entries.iter().any(|e| self.conns.iter().any(|(id, c)| *id != conn && c.aware.contains(&e.client)));
        if owned_elsewhere {
            return self.dropped_input(conn, "awareness for another connection's client");
        }
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if c.aware.len() + entries.len() > 64 && entries.iter().any(|e| !c.aware.contains(&e.client)) {
            return self.dropped_input(conn, "too many awareness clients");
        }
        for e in &entries {
            c.aware.insert(e.client);
            if e.state == "null" {
                self.awareness.remove(&e.client);
            } else {
                let newer = self.awareness.get(&e.client).is_none_or(|old| e.clock >= old.clock);
                if newer {
                    self.awareness.insert(e.client, e.clone());
                }
            }
        }
        self.broadcast(&Frame::Awareness(data), Some(conn), false);
    }

    fn on_control(&mut self, conn: u64, text: &str) {
        let Ok(v) = serde_json::from_str::<Json>(text) else { return self.dropped_input(conn, "control isn't JSON") };
        match v.get("type").and_then(Json::as_str).unwrap_or("") {
            "hello" => {
                let client_id = v.get("clientId").and_then(Json::as_u64).filter(|c| *c < (1u64 << 53));
                let can: Vec<String> = v
                    .get("can")
                    .and_then(Json::as_array)
                    .map(|a| a.iter().filter_map(Json::as_str).filter(|s| matches!(*s, "upload" | "webdav")).map(str::to_string).collect())
                    .unwrap_or_default();
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.client_id = client_id.or(c.client_id);
                    c.can = can;
                }
                self.send_peers();
            }
            "saveNow" => {
                if self.conns.get(&conn).is_some_and(|c| c.session.role == Role::Edit) {
                    let force = v.get("force").and_then(Json::as_bool).unwrap_or(false);
                    self.on_msg(Msg::SaveNow { force });
                }
            }
            "saveResult" => {
                // A desktop client saved through its own WebDAV access (mode `webdav`).
                let id = v.get("id").and_then(Json::as_u64).unwrap_or(0);
                let Some(job) = self.save.as_ref().filter(|j| j.id == id && j.conn == conn) else { return };
                let updates_at = self.updates_total;
                let _ = job;
                let outcome = if v.get("ok").and_then(Json::as_bool) == Some(true) {
                    match v.get("copy").and_then(Json::as_str) {
                        Some(name) => SaveOutcome::Copy { name: ids::clean(name, 256), why: "the file changed in Nextcloud".into() },
                        None => SaveOutcome::SavedDav(v.get("version").and_then(Json::as_str).map(|s| ids::clean(s, 256)).unwrap_or_default()),
                    }
                } else {
                    SaveOutcome::Failed(v.get("error").and_then(Json::as_str).map(|s| ids::clean(s, 300)).unwrap_or_else(|| "the client couldn't save".into()))
                };
                self.on_job(Job::Saved { save: id, outcome, updates_at, reply: None });
            }
            "bye" => {}
            _ => self.dropped_input(conn, "unknown control"),
        }
    }

    // ----- Saving -----

    /// The WOPI tokens the room may use, best first.
    fn usable_tokens(&self) -> Vec<Arc<Session>> {
        let now = ids::now_ms();
        let mut t: Vec<Arc<Session>> = self.tokens.iter().filter(|s| s.wopi_usable(now) && !self.bad_tokens.contains(&s.token)).cloned().collect();
        // Prefer the people still connected, then the newest.
        let connected: HashSet<&str> = self.conns.values().map(|c| c.session.token.as_str()).collect();
        t.reverse();
        t.sort_by_key(|s| !connected.contains(s.token.as_str()));
        t
    }

    fn editors(&self) -> Vec<(u64, &Conn)> {
        self.conns.iter().filter(|(_, c)| c.session.role == Role::Edit && c.synced).map(|(id, c)| (*id, c)).collect()
    }

    /// Picks who delivers the file: `(conn, mode)`.
    fn pick_saver(&self) -> Option<(u64, &'static str)> {
        let mut eds = self.editors();
        // The most recent editor first.
        eds.sort_by_key(|(_, c)| std::cmp::Reverse(c.last_update));
        if !self.usable_tokens().is_empty() {
            let hello = eds.iter().find(|(_, c)| c.can.iter().any(|x| x == "upload"));
            return hello.map(|(id, _)| (*id, "upload"));
        }
        eds.iter().find(|(_, c)| c.can.iter().any(|x| x == "webdav")).map(|(id, _)| (*id, "webdav"))
    }

    /// `(autosave, force)` when a save is due.
    fn save_due(&self, now: Instant) -> Option<(bool, bool)> {
        if let Some(force) = self.save_wanted {
            return Some((false, force));
        }
        if !self.meta.dirty || self.paused {
            return None;
        }
        let a = &self.ctx.cfg.autosave;
        let idle = now.duration_since(self.last_update);
        let one_editor = self.editors().len() <= 1;
        let due = self.updates_since_save >= a.updates
            || idle >= Duration::from_secs(a.idle_secs)
            || now.duration_since(self.last_save) >= Duration::from_secs(a.max_secs)
            || (one_editor && idle >= Duration::from_secs_f64(a.last_editor_idle_secs));
        due.then_some((true, false))
    }

    fn start_save(&mut self, now: Instant, autosave: bool, force: bool) {
        let Some((conn, mode)) = self.pick_saver() else { return };
        let id = self.next_save;
        self.next_save += 1;
        self.save = Some(SaveJob { id, conn, started: now, autosave, force });
        self.save_wanted = None;
        let name = self.meta.name.clone();
        let format = config::extension(&name);
        let url = format!("{}/api/v1/rooms/{}/file?save={id}", self.ctx.cfg.public_url, ids::pct(&self.id));
        tracing::info!(room = %self.id, conn, mode, id, "asking for the file to save");
        let mut msg = json!({"type": "save", "id": id, "mode": mode, "name": name, "format": format, "url": url, "autosave": autosave});
        if mode == "webdav"
            && let Some(o) = msg.as_object_mut()
        {
            o.insert("etag".into(), json!(self.meta.etag));
        }
        self.control(conn, msg);
    }

    fn on_upload(&mut self, save: u64, session: &Arc<Session>, name: String, body: Vec<u8>, reply: UploadReply) {
        let ok_job = self.save.as_ref().is_some_and(|j| j.id == save && self.conns.get(&j.conn).is_some_and(|c| c.session.token == session.token));
        if !ok_job {
            let _ = reply.send(Err((409, "no such save is waiting for this client".into())));
            return;
        }
        if body.is_empty() || body.len() as u64 > self.ctx.cfg.max_file() {
            let _ = reply.send(Err((413, "the file is empty or too large".into())));
            self.save = None;
            self.retry_at = Some(Instant::now() + Duration::from_secs(self.ctx.cfg.autosave.retry_secs));
            return;
        }
        let tokens: Vec<String> = self.usable_tokens().iter().map(|s| s.access_token.clone()).collect();
        let src = self.meta.wopi_src.clone();
        if tokens.is_empty() || src.is_empty() {
            let _ = reply.send(Err((409, "the room has no WOPI access to save with".into())));
            self.save = None;
            return;
        }
        let lock = match &self.lock {
            LockState::Held(l) => Some(l.clone()),
            _ => None,
        };
        let foreign = matches!(self.lock, LockState::Foreign(_));
        let known = self.meta.version.clone();
        let file_name = self.meta.name.clone();
        let force = self.save.as_ref().is_some_and(|j| j.force);
        let autosave = self.save.as_ref().is_none_or(|j| j.autosave);
        let editors: Vec<String> = self.editors().iter().map(|(_, c)| c.session.user.id.clone()).collect();
        let editors = editors.join(",");
        let updates_at = self.updates_total;
        let ctx = self.ctx.clone();
        let me = self.me.clone();
        let task = SaveTask { src, tokens, lock, foreign, known, file_name, upload_name: name, force, autosave, editors, body };
        tokio::spawn(async move {
            let outcome = task.run(&ctx.wopi).await;
            let _ = me.send(Msg::Done(Job::Saved { save, outcome, updates_at, reply: Some(reply) })).await;
        });
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Saved { save, outcome, updates_at, reply } => {
                if self.save.as_ref().is_some_and(|j| j.id == save) {
                    self.save = None;
                }
                let now = Instant::now();
                let answer = match &outcome {
                    SaveOutcome::Saved(version) | SaveOutcome::SavedDav(version) => {
                        if matches!(outcome, SaveOutcome::SavedDav(_)) {
                            self.meta.etag = version.clone();
                        } else {
                            if !version.is_empty() {
                                self.meta.version = version.clone();
                            }
                            // The WebDAV etag of the new version isn't known here.
                            self.meta.etag.clear();
                        }
                        self.after_save(now, updates_at);
                        self.broadcast_control(json!({"type": "saved", "at": ids::now_ms(), "modified": self.meta.dirty}));
                        Ok(json!({"saved": true, "version": version}))
                    }
                    SaveOutcome::Copy { name, why } => {
                        self.after_save(now, updates_at);
                        // Don't overwrite what changed outside: autosave waits for an explicit save.
                        self.paused = true;
                        let text = format!("{why}: the live session was saved as “{name}”. Edits go on here; Save writes them over the file in Nextcloud.");
                        self.notice("warning", &text);
                        Ok(json!({"saved": true, "copy": name}))
                    }
                    SaveOutcome::Unauthorized => {
                        self.retry_at = Some(now + Duration::from_secs(self.ctx.cfg.autosave.retry_secs));
                        self.notice("error", "Nextcloud refused to save: the editing sessions expired. Reopen the file to go on saving.");
                        Err((401, "the WOPI host refused every token".to_string()))
                    }
                    SaveOutcome::Failed(why) => {
                        self.retry_at = Some(now + Duration::from_secs(self.ctx.cfg.autosave.retry_secs));
                        tracing::warn!(room = %self.id, "save failed: {why}");
                        self.broadcast_control(json!({"type": "saveFailed", "message": why}));
                        Err((502, why.clone()))
                    }
                };
                if let Some(r) = reply {
                    let _ = r.send(answer);
                }
            }
            Job::Locked { result, lock } => match result {
                Ok(()) => {
                    tracing::info!(room = %self.id, "locked the file");
                    self.lock = LockState::Held(lock);
                    self.last_lock = Instant::now();
                }
                Err(WopiError::Conflict { lock: other, .. }) if !other.is_empty() => {
                    self.lock = LockState::Foreign(other);
                    self.notice("warning", "The file is locked in Nextcloud by someone else: saves will go to a copy.");
                }
                Err(WopiError::NotImplemented) | Err(WopiError::Conflict { .. }) => self.lock = LockState::Unsupported,
                Err(e) => {
                    tracing::warn!(room = %self.id, "couldn't lock the file: {e}");
                    self.lock = LockState::None;
                    self.last_lock = Instant::now();
                }
            },
            Job::Refreshed(r) => {
                self.last_lock = Instant::now();
                if let Err(e) = r {
                    tracing::warn!(room = %self.id, "couldn't refresh the lock: {e}");
                    if let WopiError::Conflict { lock, .. } = e {
                        self.lock = if lock.is_empty() { LockState::None } else { LockState::Foreign(lock) };
                    }
                }
            }
            Job::Unlocked => {
                self.lock = LockState::None;
            }
            Job::Persisted => self.persisting = false,
        }
    }

    fn after_save(&mut self, now: Instant, updates_at: u64) {
        self.last_save = now;
        self.retry_at = None;
        self.meta.last_saved_ms = ids::now_ms();
        if self.updates_total == updates_at {
            self.meta.dirty = false;
            self.updates_since_save = 0;
        } else {
            self.updates_since_save = u32::try_from(self.updates_total.saturating_sub(updates_at)).unwrap_or(u32::MAX);
        }
        self.persist_due = true;
    }

    // ----- Locks -----

    fn token_for_lock(&self) -> Option<(String, String)> {
        let src = self.meta.wopi_src.clone();
        self.usable_tokens().first().map(|s| (src, s.access_token.clone())).filter(|(s, _)| !s.is_empty())
    }

    fn start_lock(&mut self) {
        let Some((src, token)) = self.token_for_lock() else { return };
        self.lock = LockState::Pending;
        let lock = format!("craft-live-{}", ids::token(18));
        let (wopi, me) = (self.ctx.wopi.clone(), self.me.clone());
        tokio::spawn(async move {
            let result = wopi.lock(&src, &token, &lock).await;
            let _ = me.send(Msg::Done(Job::Locked { result, lock })).await;
        });
    }

    fn refresh_lock(&mut self) {
        let LockState::Held(lock) = &self.lock else { return };
        let Some((src, token)) = self.token_for_lock() else { return };
        let lock = lock.clone();
        self.last_lock = Instant::now();
        let (wopi, me) = (self.ctx.wopi.clone(), self.me.clone());
        tokio::spawn(async move {
            let r = wopi.refresh_lock(&src, &token, &lock).await;
            let _ = me.send(Msg::Done(Job::Refreshed(r))).await;
        });
    }

    fn release_lock(&mut self) {
        let LockState::Held(lock) = &self.lock else { return };
        let lock = lock.clone();
        let Some((src, token)) = self.token_for_lock() else {
            self.lock = LockState::None;
            return;
        };
        self.lock = LockState::Releasing;
        let (wopi, me) = (self.ctx.wopi.clone(), self.me.clone());
        tokio::spawn(async move {
            if let Err(e) = wopi.unlock(&src, &token, &lock).await {
                tracing::warn!("couldn't unlock: {e}");
            }
            let _ = me.send(Msg::Done(Job::Unlocked)).await;
        });
    }

    // ----- Persistence -----

    fn persist_async(&mut self) {
        self.persist_due = false;
        self.persisting = true;
        self.last_persist = Instant::now();
        let state = if self.meta.seeded { Some(whole_state(&self.doc)) } else { None };
        let meta = self.meta.clone();
        let dir = self.ctx.data_dir();
        let me = self.me.clone();
        tokio::spawn(async move {
            let r = tokio::task::spawn_blocking(move || store::save_room(&dir, &meta, state.as_deref())).await;
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("can't store a room: {e}"),
                Err(e) => tracing::error!("can't store a room: {e}"),
            }
            let _ = me.send(Msg::Done(Job::Persisted)).await;
        });
    }

    fn persist_now(&mut self) {
        if !self.meta.seeded && !self.persist_due {
            return;
        }
        let state = if self.meta.seeded { Some(whole_state(&self.doc)) } else { None };
        if let Err(e) = store::save_room(&self.ctx.data_dir(), &self.meta, state.as_deref()) {
            tracing::error!(room = %self.id, "can't store the room: {e}");
        }
        self.persist_due = false;
    }

    // ----- Timers -----

    /// `true` when the room should close.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let now_ms = ids::now_ms();
        // Sessions that ended.
        let expired: Vec<u64> = self.conns.iter().filter(|(_, c)| c.session.expired(now_ms)).map(|(id, _)| *id).collect();
        for id in expired {
            self.control(id, json!({"type": "expired"}));
            if let Some(c) = self.conns.get(&id) {
                let _ = c.out.try_send(Out::Close(4401, "the session expired".into()));
            }
            self.drop_conn(id);
        }
        // A seed lease nobody took up.
        if let Some(l) = &self.lease
            && l.conn.is_none()
            && now >= l.expires
        {
            self.lease = None;
        }
        // A save that never came.
        if let Some(j) = &self.save
            && now.duration_since(j.started) >= Duration::from_secs(self.ctx.cfg.autosave.timeout_secs)
        {
            tracing::warn!(room = %self.id, "the client didn't deliver the file in time");
            self.save = None;
            self.retry_at = Some(now + Duration::from_secs(self.ctx.cfg.autosave.retry_secs));
        }
        if self.save.is_none()
            && self.meta.seeded
            && self.retry_at.is_none_or(|t| now >= t)
            && let Some((autosave, force)) = self.save_due(now)
        {
            self.start_save(now, autosave, force);
        }
        // The lock.
        match self.lock {
            LockState::Held(_) if now.duration_since(self.last_lock) >= Duration::from_secs(self.ctx.cfg.limits.lock_refresh_secs) => self.refresh_lock(),
            LockState::None
                if self.supports_locks
                    && !self.conns.is_empty()
                    && now.duration_since(self.last_lock) >= Duration::from_secs(self.ctx.cfg.autosave.retry_secs) =>
            {
                self.last_lock = now;
                self.start_lock();
            }
            _ => {}
        }
        if self.persist_due && !self.persisting && now.duration_since(self.last_persist) >= PERSIST_EVERY {
            self.persist_async();
        }
        // Closing an empty room.
        if let Some(since) = self.empty_since
            && self.conns.is_empty()
            && self.save.is_none()
            && now.duration_since(since) >= Duration::from_secs(self.ctx.cfg.limits.close_grace_secs)
        {
            let lease_waiting = self.lease.as_ref().is_some_and(|l| now < l.expires);
            if lease_waiting {
                return false;
            }
            match self.lock {
                LockState::Held(_) => {
                    self.release_lock();
                    return false;
                }
                LockState::Releasing | LockState::Pending => return false,
                _ => {}
            }
            if self.persisting {
                return false;
            }
            if self.meta.dirty {
                tracing::warn!(room = %self.id, "closing with edits that didn't reach the file; they're kept and saved when someone opens it again");
            }
            return true;
        }
        false
    }
}

/// A save in the background: checks the file didn't change outside, then PutFile (or a copy).
struct SaveTask {
    src: String,
    tokens: Vec<String>,
    lock: Option<String>,
    foreign: bool,
    known: String,
    file_name: String,
    upload_name: String,
    force: bool,
    autosave: bool,
    editors: String,
    body: Vec<u8>,
}

impl SaveTask {
    async fn copy(&self, wopi: &Wopi, token: &str, why: &str) -> Result<SaveOutcome, WopiError> {
        let ext = {
            let e = config::extension(&self.upload_name);
            if e.is_empty() { config::extension(&self.file_name) } else { e }
        };
        let name = copy_name(&self.file_name, &ext, ids::now_ms());
        let f = wopi.put_relative_file(&self.src, token, &name, self.body.clone()).await?;
        Ok(SaveOutcome::Copy { name: if f.name.is_empty() { name } else { f.name }, why: why.to_string() })
    }

    async fn run(&self, wopi: &Wopi) -> SaveOutcome {
        let mut last = SaveOutcome::Unauthorized;
        for token in &self.tokens {
            match self.with_token(wopi, token).await {
                Ok(o) => return o,
                Err(WopiError::Unauthorized) => continue,
                Err(e) => {
                    last = SaveOutcome::Failed(e.to_string());
                    break;
                }
            }
        }
        last
    }

    async fn with_token(&self, wopi: &Wopi, token: &str) -> Result<SaveOutcome, WopiError> {
        // The app gave bytes in another format than the file's: never write them over it.
        let file_ext = config::extension(&self.file_name);
        let up_ext = config::extension(&self.upload_name);
        if !up_ext.is_empty() && !file_ext.is_empty() && up_ext != file_ext {
            return self.copy(wopi, token, &format!("The app saved .{up_ext}, not .{file_ext}")).await;
        }
        if self.foreign && !self.force {
            return self.copy(wopi, token, "The file is locked in Nextcloud by someone else").await;
        }
        if !self.known.is_empty() && !self.force {
            let info = wopi.check_file_info(&self.src, token).await?;
            if !info.can_edit() {
                return Ok(SaveOutcome::Failed("this person may no longer change the file".into()));
            }
            if !info.version.is_empty() && info.version != self.known {
                return self.copy(wopi, token, "The file changed in Nextcloud while it was open here").await;
            }
        }
        let mut tried_relock = false;
        loop {
            match wopi.put_file(&self.src, token, self.lock.as_deref(), &self.editors, self.autosave, self.body.clone()).await {
                Ok(v) => {
                    let version = if v.is_empty() { wopi.check_file_info(&self.src, token).await.map(|i| i.version).unwrap_or_default() } else { v };
                    return Ok(SaveOutcome::Saved(version));
                }
                Err(WopiError::Conflict { lock, .. }) => {
                    if let Some(ours) = &self.lock
                        && lock.is_empty()
                        && !tried_relock
                    {
                        // Our lock lapsed on the host: take it again and retry once.
                        tried_relock = true;
                        wopi.lock(&self.src, token, ours).await?;
                        continue;
                    }
                    return self.copy(wopi, token, "The file is locked in Nextcloud by someone else").await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_names() {
        assert_eq!(copy_name("Budget.xlsx", "xlsx", 0), "Budget (live copy 1970-01-01 00-00).xlsx");
        assert_eq!(copy_name(".xlsx", "ods", 0), "Document (live copy 1970-01-01 00-00).ods");
    }

    #[test]
    fn hostile_updates_dont_panic() {
        let doc = Doc::new();
        for bad in [vec![], vec![1], vec![0xff; 40], vec![1, 1, 5, 0, 0xff, 0xff, 0xff, 0xff, 0x0f], vec![2, 0, 0, 0, 0, 0, 0, 0]] {
            let _ = apply_update(&doc, &bad);
            let _ = diff(&doc, &bad);
        }
        assert!(doc_is_empty(&doc));
    }
}
