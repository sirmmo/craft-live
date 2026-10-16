//! nextcloud-live end to end: the add-in driven over stdio as an app drives it (initialize with
//! `push` and `collab`, commands, `collab/*`), against a real craft-live server and a fake
//! Nextcloud. Two desktop add-ins converge and save through WebDAV; a desktop and a web client
//! meet in one room and save through WOPI; Share and Join by link; reconnecting after the server
//! restarts. When `CRAFT_COLLAB_CONFORMANCE` names craft-collab's conformance binary, its
//! transport checker runs against the add-in too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use craft_live::config::Config;
use craft_live_testkit::{FakeNextcloud, YClient};
use serde_json::{Value, json};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, Update};

const T: Duration = Duration::from_secs(15);

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nextcloud-live-e2e-{name}-{}-{}", std::process::id(), craft_live::ids::token(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An app with a Yjs document (a map `cells`), driving a nextcloud-live process.
struct SimApp {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    doc: Doc,
    room: String,
    next_id: u64,
    log: Vec<Value>,
    snapshots: u32,
}

impl SimApp {
    fn start(data: &Path, live: &str) -> SimApp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nextcloud-live"))
            .env("CRAFT_NEXTCLOUD_LIVE_DATA", data)
            .env("CRAFT_NEXTCLOUD_DATA", data.join("no-craft-nextcloud"))
            .env("CRAFT_LIVE_URL", live)
            .env("NEXTCLOUD_LIVE_NO_BROWSER", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut app = SimApp { child, stdin, rx, doc: Doc::new(), room: String::new(), next_id: 1, log: Vec::new(), snapshots: 0 };
        let host = json!({"id": "gridcraft", "name": "Sim", "version": "1", "language": "en", "protocol": "2.0",
            "features": ["dialogs", "open", "events", "document", "push", "collab"],
            "collab": {"formats": ["yjs-v1"], "app": "gridcraft", "schema": 1, "encryption": ["aes-256-gcm"]}});
        let r = app.request("initialize", json!({"protocolVersion": 1, "host": host, "plugin": {"id": "x", "dir": "."}}));
        assert!(r.get("result").is_some(), "{r}");
        app
    }

    fn send(&mut self, v: &Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(50)) {
                if m.get("id").and_then(Value::as_u64) == Some(id) && m.get("method").is_none() {
                    return m;
                }
                self.handle(m);
            }
        }
        panic!("no answer to {method}");
    }

    fn command(&mut self, command: &str, params: Value, doc_path: Option<&str>) -> Value {
        let ctx = json!({"document": {"path": doc_path, "name": "Book1.xlsx", "dirty": false}});
        let r = self.request("commands/run", json!({"command": command, "params": params, "context": ctx}));
        match r.get("result") {
            Some(v) => v.clone(),
            None => panic!("{command} failed: {r}"),
        }
    }

    fn update_since(&self, before: &StateVector) -> Vec<u8> {
        self.doc.transact().encode_diff_v1(before)
    }

    /// Joins as `collab.join` says: seeds from `path` (mode share), or from the open document.
    fn join(&mut self, join: &Value) {
        self.room = join["room"].as_str().unwrap().to_string();
        if join["mode"] == "share"
            && let Some(p) = join["path"].as_str()
        {
            let file: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
            let map = self.doc.get_or_insert_map("cells");
            let mut t = self.doc.transact_mut();
            for (k, v) in file.as_object().unwrap() {
                map.insert(&mut t, k.as_str(), v.as_str().unwrap_or(""));
            }
        }
        let cid = self.doc.client_id().get();
        let room = self.room.clone();
        self.notify("collab/joined", json!({"room": room, "clientId": cid, "user": join["user"], "app": "gridcraft", "schema": 1, "encrypted": false}));
        let sv = STANDARD.encode(self.doc.transact().state_vector().encode_v1());
        self.notify("collab/sync1", json!({"room": room, "stateVector": sv}));
    }

    fn handle(&mut self, m: Value) {
        let method = m.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let p = m.get("params").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "collab/sync1" => {
                let sv = StateVector::decode_v1(&STANDARD.decode(p["stateVector"].as_str().unwrap()).unwrap()).unwrap();
                let u = STANDARD.encode(self.doc.transact().encode_diff_v1(&sv));
                let room = self.room.clone();
                self.notify("collab/sync2", json!({"room": room, "update": u}));
            }
            "collab/sync2" | "collab/update" => {
                let u = Update::decode_v1(&STANDARD.decode(p["update"].as_str().unwrap()).unwrap()).unwrap();
                self.doc.transact_mut().apply_update(u).unwrap();
            }
            "collab/snapshot" => {
                self.snapshots += 1;
                let whole = STANDARD.encode(self.doc.transact().encode_state_as_update_v1(&StateVector::default()));
                let file = STANDARD.encode(serde_json::to_vec(&self.cells()).unwrap());
                let id = m["id"].clone();
                self.send(&json!({"jsonrpc": "2.0", "id": id, "result": {"update": whole, "file": file, "name": "Budget.xlsx"}}));
            }
            _ => {}
        }
        self.log.push(m);
    }

    fn pump(&mut self, dur: Duration) {
        let end = Instant::now() + dur;
        while Instant::now() < end {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(10)) {
                self.handle(m);
            }
        }
    }

    fn edit(&mut self, k: &str, v: &str) {
        let before = self.doc.transact().state_vector();
        let map = self.doc.get_or_insert_map("cells");
        map.insert(&mut self.doc.transact_mut(), k, v);
        let u = STANDARD.encode(self.update_since(&before));
        let room = self.room.clone();
        self.notify("collab/update", json!({"room": room, "update": u}));
    }

    fn cells(&self) -> BTreeMap<String, String> {
        let map = self.doc.get_or_insert_map("cells");
        let t = self.doc.transact();
        map.iter(&t).map(|(k, v)| (k.to_string(), v.to_string(&t))).collect()
    }

    fn states(&self) -> Vec<String> {
        self.log.iter().filter(|m| m["method"] == "collab/status").map(|m| m["params"]["state"].as_str().unwrap_or("").to_string()).collect()
    }

    fn last_status(&self) -> Value {
        self.log.iter().rev().find(|m| m["method"] == "collab/status").map(|m| m["params"].clone()).unwrap_or(Value::Null)
    }
}

impl Drop for SimApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pump_all(apps: &mut [&mut SimApp], timeout: Duration, done: impl Fn(&[&mut SimApp]) -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        for a in apps.iter_mut() {
            a.pump(Duration::from_millis(20));
        }
        if done(apps) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
    }
}

struct World {
    fake: FakeNextcloud,
    cfg: Config,
    server: Option<craft_live::Running>,
    file: u64,
    account: String,
    dir: PathBuf,
}

async fn world(name: &str, tweak: impl FnOnce(&mut Config)) -> World {
    let fake = FakeNextcloud::start().await;
    let dir = temp(name);
    let port = free_port();
    let mut cfg = Config {
        listen: format!("127.0.0.1:{port}"),
        public_url: format!("http://127.0.0.1:{port}"),
        data_dir: dir.join("server"),
        wopi_hosts: vec![fake.base()],
        proof_keys: false,
        ..Config::default()
    };
    let apps = dir.join("grid");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join("index.html"), "<!doctype html>").unwrap();
    cfg.apps.insert("gridcraft".into(), apps);
    cfg.autosave.updates = 1000;
    cfg.autosave.idle_secs = 3600;
    cfg.autosave.max_secs = 3600;
    cfg.autosave.last_editor_idle_secs = 3600.0;
    cfg.autosave.retry_secs = 1;
    cfg.limits.close_grace_secs = 1;
    tweak(&mut cfg);
    cfg.validate().unwrap();
    let server = craft_live::start(cfg.clone()).await.unwrap();
    let file = fake.add_file("ada", "/Budget.xlsx", br#"{"A1":"Item","B1":"Cost"}"#);
    fake.add_account("ada", "app-pass", "ada", "Ada Lovelace");
    let account = "acc1".to_string();
    World { fake, cfg, server: Some(server), file, account, dir }
}

impl World {
    /// A plugin data folder holding the account.
    fn plugin_data(&self, who: &str) -> PathBuf {
        let d = self.dir.join(format!("plugin-{who}"));
        std::fs::create_dir_all(&d).unwrap();
        let accounts = json!({"accounts": [{"id": self.account, "server": self.fake.base(), "loginName": "ada", "userId": "ada", "appPassword": "app-pass"}]});
        std::fs::write(d.join("accounts.json"), accounts.to_string()).unwrap();
        d
    }

    fn app(&self, who: &str) -> SimApp {
        SimApp::start(&self.plugin_data(who), &self.cfg.public_url)
    }

    fn file_cells(&self) -> Value {
        serde_json::from_slice(&self.fake.file(self.file).bytes).unwrap_or(Value::Null)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_desktops_converge_save_and_reconnect() {
    let mut w = world("two", |c| c.autosave.updates = 5).await;
    let mut a = w.app("a");
    let mut b = w.app("b");
    // A opens the file: first in the room, so it seeds from its download.
    let r = a.command("open", json!({"account": w.account, "id": "/Budget.xlsx"}), None);
    let join = r["collab"]["join"].clone();
    assert_eq!(join["mode"], "share", "{r}");
    assert!(join["path"].as_str().unwrap().ends_with("Budget.xlsx"));
    assert_eq!(join["user"]["name"], "Ada Lovelace");
    assert_eq!(join["role"], "edit");
    assert_eq!(r["result"]["link"], format!("{}/f/{}", w.fake.base(), w.file));
    a.join(&join);
    assert!(pump_all(&mut [&mut a], T, |x| x[0].states().contains(&"online".to_string())), "A online: {:?}", a.states());
    assert_eq!(a.states().first().map(String::as_str), Some("connecting"));
    // B opens it too: joins.
    let r = b.command("open", json!({"account": w.account, "id": "/Budget.xlsx"}), None);
    let join_b = r["collab"]["join"].clone();
    assert_eq!(join_b["mode"], "join");
    assert!(join_b.get("path").is_none());
    b.join(&join_b);
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[1].cells().get("A1").map(String::as_str) == Some("Item")), "B gets the document");
    // Edits both ways and concurrently.
    a.edit("A2", "Coffee");
    b.edit("B2", "3.5");
    for i in 0..10 {
        a.edit(&format!("C{i}"), "a");
        b.edit(&format!("C{i}"), "b");
    }
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[0].cells() == x[1].cells() && x[0].cells().len() >= 14), "converge");
    // Peers: A sees B (the same person, another client).
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[0].last_status()["peers"].as_array().is_some_and(|p| !p.is_empty())));
    // No web session in the room: the save goes through WebDAV (mode webdav) from an add-in.
    let expected = serde_json::to_value(a.cells()).unwrap();
    assert!(
        pump_all(&mut [&mut a, &mut b], T, |_| w.file_cells() == expected),
        "the file has the room's content: {:?} / log {:?}",
        w.file_cells(),
        w.fake.log()
    );
    assert!(a.snapshots + b.snapshots >= 1);
    assert!(w.fake.log().iter().any(|l| l.starts_with("DAV PUT /Budget.xlsx")));
    // The server restarts: the add-ins reconnect with their sessions and go on.
    w.server.take().unwrap().stop().await;
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[0].last_status()["state"] == "offline"));
    let server = craft_live::start(w.cfg.clone()).await.unwrap();
    w.server = Some(server);
    assert!(
        pump_all(&mut [&mut a, &mut b], Duration::from_secs(40), |x| x[0].last_status()["state"] == "online" && x[1].last_status()["state"] == "online"),
        "back online"
    );
    a.edit("D1", "after restart");
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[1].cells().get("D1").map(String::as_str) == Some("after restart")));
    // Leave and status.
    let s = b.command("status", json!({}), None);
    assert_eq!(s["result"]["rooms"].as_array().unwrap().len(), 1);
    let left = b.command("leave", json!({}), None);
    assert_eq!(left["result"]["room"], b.room.as_str());
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[1].log.iter().any(|m| m["method"] == "collab/leave")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_and_web_meet_and_save_through_wopi() {
    let w = world("meet", |c| c.autosave.last_editor_idle_secs = 0.5).await;
    // The web opens first (Nextcloud Office) and seeds.
    let token = w.fake.token("bob", "Bob", w.file, true);
    let http = reqwest::Client::new();
    let url = format!("{}/edit/gridcraft?WOPISrc={}", w.cfg.public_url, pct(&w.fake.wopi_src(w.file)));
    let html = http.post(url).form(&[("access_token", token.as_str())]).send().await.unwrap().text().await.unwrap();
    let cfg: Value =
        serde_json::from_str(&html.split("id=\"craft-live-config\">").nth(1).unwrap().split("</script>").next().unwrap().replace("<\\/", "</")).unwrap();
    let embed = cfg["embed"].clone();
    assert_eq!(embed["seed"], true);
    let mut web = YClient::connect(embed["ws"].as_str().unwrap(), embed["token"].as_str().unwrap()).await.unwrap();
    web.hello().await;
    let bytes = http.get(embed["file"].as_str().unwrap()).bearer_auth(embed["token"].as_str().unwrap()).send().await.unwrap().bytes().await.unwrap();
    web.seed_from_file(&bytes);
    web.sync1().await;
    assert!(web.pump_until(T, |c| c.synced).await);
    // The desktop joins the same room.
    let mut a = w.app("a");
    let r = a.command("open", json!({"account": w.account, "id": "/Budget.xlsx"}), None);
    let join = r["collab"]["join"].clone();
    assert_eq!(join["mode"], "join", "{r}");
    assert_eq!(join["room"], embed["room"]);
    a.join(&join);
    let mut ok = false;
    for _ in 0..300 {
        a.pump(Duration::from_millis(20));
        web.pump(Duration::from_millis(20)).await;
        if a.cells().get("A1").map(String::as_str) == Some("Item") {
            ok = true;
            break;
        }
    }
    assert!(ok, "the desktop gets the web's seed");
    a.edit("W1", "from desktop");
    web.edit("W2", "from web").await;
    let mut both = false;
    for _ in 0..300 {
        a.pump(Duration::from_millis(20));
        web.pump(Duration::from_millis(20)).await;
        if a.cells() == web.cells() && a.cells().contains_key("W2") && web.cells().contains_key("W1") {
            both = true;
            break;
        }
    }
    assert!(both, "desktop {:?} / web {:?}", a.cells(), web.cells());
    // The room has a WOPI token: saving goes through craft-live (upload) and WOPI PutFile.
    web.close().await;
    let mut saved = false;
    for _ in 0..300 {
        a.pump(Duration::from_millis(30));
        if w.file_cells()["W1"] == "from desktop" && w.file_cells()["W2"] == "from web" {
            saved = true;
            break;
        }
    }
    assert!(saved, "{:?} / {:?}", w.file_cells(), w.fake.log());
    assert!(w.fake.log().iter().any(|l| l.starts_with("PutFile")), "{:?}", w.fake.log());
    assert!(a.snapshots >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_the_open_document_and_join_by_link() {
    let w = world("share", |_| {}).await;
    let mut a = w.app("a");
    let mut b = w.app("b");
    // Share: the open document seeds the room (no path in the join).
    a.edit("X1", "open document");
    let r = a.command("share", json!({"account": w.account, "path": "/Budget.xlsx", "name": "Ada", "color": "#3B7DD8", "encrypt": false}), None);
    let join = r["collab"]["join"].clone();
    assert_eq!(join["mode"], "share");
    assert!(join.get("path").is_none());
    assert!(join.get("key").is_none());
    let link = r["result"]["link"].as_str().unwrap().to_string();
    a.join(&join);
    assert!(pump_all(&mut [&mut a], T, |x| x[0].states().contains(&"online".to_string())));
    // Join from the link.
    let r = b.command("join", json!({"link": link}), None);
    let jb = r["collab"]["join"].clone();
    assert_eq!(jb["mode"], "join", "{r}");
    assert_eq!(jb["room"], join["room"]);
    b.join(&jb);
    assert!(pump_all(&mut [&mut a, &mut b], T, |x| x[1].cells().get("X1").map(String::as_str) == Some("open document")));
    // Share refuses end-to-end encryption, and odd params get answers, not crashes.
    let r = a.request("commands/run", json!({"command": "share", "params": {"encrypt": true, "path": "/Budget.xlsx"}}));
    assert!(r.get("error").is_some());
    for cmd in ["open", "share", "join", "status", "connect", "finishLogin", "chooseAccount", "settings", "saveSettings", "dismiss", "nope"] {
        for p in [json!({}), json!({"fields": 42, "room": [1], "link": {"x": 1}, "path": "../x", "id": 5})] {
            let r = a.request("commands/run", json!({"command": cmd, "params": p}));
            assert!(r.get("result").is_some() || r.get("error").is_some(), "{cmd}: {r}");
        }
    }
    // Garbage on standard input.
    a.send(&json!({"jsonrpc": "2.0", "method": "collab/update", "params": {"room": join["room"], "update": "!!!"}}));
    a.send(&json!({"jsonrpc": "2.0", "method": "collab/joined", "params": {}}));
    writeln!(a.stdin, "not json").unwrap();
    let s = a.command("status", json!({}), None);
    assert_eq!(s["result"]["rooms"].as_array().unwrap().len(), 1);
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// craft-collab's transport checker against nextcloud-live (when its binary is given).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn craft_collab_conformance() {
    let Some(checker) = std::env::var_os("CRAFT_COLLAB_CONFORMANCE") else {
        eprintln!("skipped: set CRAFT_COLLAB_CONFORMANCE to craft-collab-conformance's path");
        return;
    };
    let w = world("conformance", |c| {
        c.autosave.updates = 20;
        c.autosave.last_editor_idle_secs = 0.2;
        c.limits.close_grace_secs = 30;
    })
    .await;
    w.fake.add_file("ada", "/Book1.xlsx", b"{}");
    let data = w.plugin_data("conf");
    let share = json!({"account": w.account, "path": "/Book1.xlsx"}).to_string();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(checker)
            .args(["transport", "--timeout", "20", "--share", &share])
            .args(["--env", &format!("CRAFT_NEXTCLOUD_LIVE_DATA={}", data.display())])
            .args(["--env", &format!("CRAFT_NEXTCLOUD_DATA={}", data.join("none").display())])
            .args(["--env", &format!("CRAFT_LIVE_URL={}", w.cfg.public_url)])
            .args(["--", env!("CARGO_BIN_EXE_nextcloud-live")])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let report = String::from_utf8_lossy(&out.stdout);
    println!("{report}");
    assert!(out.status.success(), "conformance failed:\n{report}\n{}", String::from_utf8_lossy(&out.stderr));
}
