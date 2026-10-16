//! craft-live end to end against a fake Nextcloud: the editor page, seeding, co-editing,
//! saving through WOPI (locks, proof keys, 409 conflicts, files changed outside, expired
//! tokens), persistence across a restart, hostile input and desktop sessions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::time::Duration;

use craft_live::config::Config;
use craft_live_testkit::yclient::awareness;
use craft_live_testkit::{FakeNextcloud, WFrame, YClient};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(10);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("craft-live-test-{name}-{}-{}", std::process::id(), craft_live::ids::token(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(fake: &FakeNextcloud, data: &std::path::Path, proof: bool) -> Config {
    let port = free_port();
    let apps = data.join("apps-gridcraft");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(apps.join("index.html"), "<!doctype html><title>GridCraft</title>").unwrap();
    let mut cfg = Config {
        listen: format!("127.0.0.1:{port}"),
        public_url: format!("http://127.0.0.1:{port}"),
        data_dir: data.join("data"),
        wopi_hosts: vec![fake.base()],
        proof_keys: proof,
        ..Config::default()
    };
    cfg.apps.insert("gridcraft".into(), apps);
    cfg.autosave.updates = 1000;
    cfg.autosave.idle_secs = 3600;
    cfg.autosave.max_secs = 3600;
    cfg.autosave.last_editor_idle_secs = 3600.0;
    cfg.autosave.retry_secs = 1;
    cfg.limits.close_grace_secs = 1;
    cfg.validate().unwrap();
    cfg
}

struct Env {
    fake: FakeNextcloud,
    cfg: Config,
    server: Option<craft_live::Running>,
    http: reqwest::Client,
    file: u64,
}

impl Env {
    async fn new(name: &str, proof: bool, tweak: impl FnOnce(&mut Config)) -> Env {
        let fake = FakeNextcloud::start().await;
        let data = temp(name);
        let mut cfg = config(&fake, &data, proof);
        tweak(&mut cfg);
        let server = craft_live::start(cfg.clone()).await.unwrap();
        let http = reqwest::Client::new();
        if proof {
            let xml = http.get(format!("{}/hosting/discovery", cfg.public_url)).send().await.unwrap().text().await.unwrap();
            fake.trust_discovery(&xml);
            assert!(fake.st().proof_key.is_some());
        }
        let file = fake.add_file("ada", "/Budget.xlsx", br#"{"A1":"Item","B1":"Cost"}"#);
        Env { fake, cfg, server: Some(server), http, file }
    }

    /// Opens the editor page as Nextcloud does; the embed configuration in it.
    async fn open(&self, user: &str, write: bool) -> Value {
        let token = self.fake.token(user, &format!("{user} name"), self.file, write);
        self.open_with(&token, "").await.1.expect("an editor page")
    }

    async fn open_with(&self, token: &str, extra: &str) -> (u16, Option<Value>, String) {
        let src = self.fake.wopi_src(self.file);
        let url = format!("{}/edit/gridcraft?WOPISrc={}&lang=en&closebutton=1{extra}", self.cfg.public_url, pct(&src));
        let r = self.http.post(url).form(&[("access_token", token), ("access_token_ttl", "0")]).send().await.unwrap();
        let status = r.status().as_u16();
        let html = r.text().await.unwrap();
        let cfg = html.split("id=\"craft-live-config\">").nth(1).and_then(|r| r.split("</script>").next()).map(|j| j.replace("<\\/", "</"));
        let cfg: Option<Value> = cfg.and_then(|j| serde_json::from_str(&j).ok());
        let embed = cfg.map(|c| c["embed"].clone()).filter(|e| e.get("ws").is_some());
        (status, embed, html)
    }

    async fn connect(&self, embed: &Value) -> YClient {
        let mut c = YClient::connect(embed["ws"].as_str().unwrap(), embed["token"].as_str().unwrap()).await.unwrap();
        c.hello().await;
        c
    }

    /// Opens, connects and (when it holds the lease) seeds from the file as an app would.
    async fn join(&self, user: &str, write: bool) -> YClient {
        let embed = self.open(user, write).await;
        let mut c = self.connect(&embed).await;
        assert!(c.pump_until(T, |c| c.has_control("welcome")).await, "welcome");
        if embed["seed"] == true {
            let file = self.http.get(embed["file"].as_str().unwrap()).bearer_auth(embed["token"].as_str().unwrap()).send().await.unwrap();
            assert_eq!(file.status(), 200);
            let bytes = file.bytes().await.unwrap();
            c.seed_from_file(&bytes);
        }
        c.sync1().await;
        assert!(c.pump_until(T, |c| c.synced).await, "{user} never got the document");
        c
    }

    async fn status(&self, c: &YClient) -> Value {
        let room = c.control("welcome").unwrap()["room"].as_str().unwrap().to_string();
        self.http.get(format!("{}/api/v1/rooms/{}/status", self.cfg.public_url, pct(&room))).bearer_auth(&c.token).send().await.unwrap().json().await.unwrap()
    }

    async fn save_now(&self, c: &YClient) {
        let r = self.http.post(format!("{}/api/v1/save", self.cfg.public_url)).bearer_auth(&c.token).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }

    fn file_json(&self) -> Value {
        serde_json::from_slice(&self.fake.file(self.file).bytes).unwrap_or(Value::Null)
    }
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

async fn settle(clients: &mut [&mut YClient], dur: Duration) {
    let end = std::time::Instant::now() + dur;
    while std::time::Instant::now() < end {
        for c in clients.iter_mut() {
            c.pump(Duration::from_millis(30)).await;
        }
    }
}

async fn converge(clients: &mut [&mut YClient]) -> bool {
    for _ in 0..200 {
        for c in clients.iter_mut() {
            c.pump(Duration::from_millis(20)).await;
        }
        let first = clients[0].cells();
        if clients.iter().all(|c| c.cells() == first) {
            return true;
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_capabilities_and_access_check() {
    let env = Env::new("disc", true, |_| {}).await;
    let xml = env.http.get(format!("{}/hosting/discovery", env.cfg.public_url)).send().await.unwrap().text().await.unwrap();
    assert!(xml.contains("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"));
    assert!(xml.contains(&format!("{}/edit/gridcraft?", env.cfg.public_url)));
    assert!(!xml.contains("wordprocessingml"), "WordCraft isn't configured");
    let caps: Value = env.http.get(format!("{}/hosting/capabilities", env.cfg.public_url)).send().await.unwrap().json().await.unwrap();
    assert_eq!(caps["productName"], "craft-live");
    let ok: Value = env
        .http
        .post(format!("{}/hosting/wopiAccessCheck", env.cfg.public_url))
        .json(&json!({"callbackUrl": env.fake.base()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok["status"], "Ok");
    let no: Value = env
        .http
        .post(format!("{}/hosting/wopiAccessCheck", env.cfg.public_url))
        .json(&json!({"callbackUrl": "http://169.254.169.254"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(no["status"], "Ok");
    let h: Value = env.http.get(format!("{}/healthz", env.cfg.public_url)).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["ok"], true);
    // The app's build is served for the editor page; nothing outside it.
    let r = env.http.get(format!("{}/apps/gridcraft/index.html", env.cfg.public_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = env.http.get(format!("{}/apps/gridcraft/..%2F..%2Fdata%2Fproof-key.pem", env.cfg.public_url)).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_coedit_save_lock_and_proof() {
    let env = Env::new("flow", true, |_| {}).await;
    let mut a = env.join("ada", true).await;
    assert_eq!(a.control("welcome").unwrap()["seed"], true);
    assert_eq!(a.control("welcome").unwrap()["room"], format!("nc:127.0.0.1:{}:{}", env.fake.addr.port(), env.file));
    let mut b = env.join("bob", true).await;
    assert_eq!(b.control("welcome").unwrap()["seed"], false);
    assert_eq!(b.get("A1").as_deref(), Some("Item"), "Bob gets Ada's seed");
    let mut c = env.join("carol", false).await;
    assert_eq!(c.control("welcome").unwrap()["role"], "view");
    // Peers: Ada sees Bob and Carol with their clientIds.
    let bob_id = b.client_id();
    assert!(a.pump_until(T, |a| a.control("peers").is_some_and(|p| p["peers"].as_array().unwrap().iter().any(|x| x["clientId"] == bob_id))).await);
    // Edits both ways, concurrent ones, and a viewer who can't edit.
    a.edit("A2", "Coffee").await;
    b.edit("B2", "3.5").await;
    for i in 0..20 {
        a.edit(&format!("C{i}"), &format!("a{i}")).await;
        b.edit(&format!("C{i}"), &format!("b{i}")).await;
    }
    assert!(converge(&mut [&mut a, &mut b, &mut c]).await, "the three converge");
    c.edit("Z9", "viewer").await;
    settle(&mut [&mut a, &mut b, &mut c], Duration::from_millis(300)).await;
    assert_eq!(a.get("Z9"), None, "a viewer's edit is dropped");
    assert_eq!(b.get("Z9"), None);
    assert_eq!(a.get("B2").as_deref(), Some("3.5"));
    // Awareness reaches the others.
    let aw = awareness(a.client_id(), 1, &json!({"user": {"name": "ada"}, "selection": {"sheet": "s", "ranges": []}}));
    a.send(WFrame::Awareness(aw.clone())).await;
    assert!(b.pump_until(T, |b| b.awareness.contains(&aw)).await);
    // Save: the room asks one editor for the file and writes it with PutFile, under its lock.
    env.save_now(&a).await;
    let expected = serde_json::to_value(a.cells()).unwrap();
    let mut saved = false;
    for _ in 0..200 {
        settle(&mut [&mut a, &mut b, &mut c], Duration::from_millis(30)).await;
        if env.file_json() == expected {
            saved = true;
            break;
        }
    }
    assert!(saved, "the file in Nextcloud has the room's content: {:?}", env.fake.log());
    let log = env.fake.log();
    assert!(log.iter().any(|l| l.starts_with("LOCK")), "{log:?}");
    assert!(log.iter().any(|l| l.starts_with("PutFile") && l.contains("lock=true") && l.contains("autosave=false")), "{log:?}");
    assert!(a.pump_until(T, |a| a.has_control("saved")).await || b.has_control("saved"));
    assert!(env.fake.file(env.file).lock.is_some(), "locked while open");
    let st = env.status(&a).await;
    assert_eq!(st["dirty"], false);
    assert_eq!(st["lock"], "held");
    // Proof keys: every WOPI request was signed and checked.
    assert!(env.fake.st().proofs_ok >= 4);
    assert_eq!(env.fake.st().proof_failures, 0);
    // Everyone leaves: the room unlocks the file and closes.
    a.close().await;
    b.close().await;
    c.close().await;
    for _ in 0..100 {
        if env.fake.file(env.file).lock.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(env.fake.file(env.file).lock.is_none(), "unlocked after everyone left: {:?}", env.fake.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autosave_when_the_last_editor_is_alone() {
    let env = Env::new("autosave", false, |c| c.autosave.last_editor_idle_secs = 0.3).await;
    let mut a = env.join("ada", true).await;
    a.edit("A5", "alone").await;
    for _ in 0..100 {
        a.pump(Duration::from_millis(50)).await;
        if env.file_json()["A5"] == "alone" {
            break;
        }
    }
    assert_eq!(env.file_json()["A5"], "alone", "{:?}", env.fake.log());
    assert!(env.fake.log().iter().any(|l| l.contains("autosave=true")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_lock_saves_a_copy() {
    let env = Env::new("conflict", false, |_| {}).await;
    let mut a = env.join("ada", true).await;
    a.pump(Duration::from_millis(300)).await;
    // Someone else takes the lock (the host lost ours, then another editor locked it).
    env.fake.st().files.get_mut(&env.file).unwrap().lock = Some("someone-else".into());
    a.edit("A3", "mine").await;
    env.save_now(&a).await;
    assert!(a.pump_until(T, |a| a.has_control("notice")).await, "{:?}", env.fake.log());
    let notice = a.control("notice").unwrap()["text"].as_str().unwrap().to_string();
    assert!(notice.contains("live copy"), "{notice}");
    let log = env.fake.log();
    assert!(log.iter().any(|l| l.starts_with("PutFile")) && log.iter().any(|l| l.starts_with("PUT_RELATIVE")), "{log:?}");
    let copy = env.fake.st().files.values().find(|f| f.path.contains("live copy")).cloned().unwrap();
    assert!(copy.path.ends_with(".xlsx"));
    assert_eq!(serde_json::from_slice::<Value>(&copy.bytes).unwrap()["A3"], "mine");
    assert_eq!(env.file_json()["A3"], Value::Null, "the locked file wasn't overwritten");
    // Autosave pauses; an explicit forced save writes over the file once the lock is gone.
    env.fake.st().files.get_mut(&env.file).unwrap().lock = None;
    let r = env.http.post(format!("{}/api/v1/save?force=1", env.cfg.public_url)).bearer_auth(&a.token).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut ok = false;
    for _ in 0..100 {
        a.pump(Duration::from_millis(50)).await;
        if env.file_json()["A3"] == "mine" {
            ok = true;
            break;
        }
    }
    assert!(ok, "{:?}", env.fake.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_outside_saves_a_copy() {
    let env = Env::new("outside", false, |c| c.limits.lock_refresh_secs = 3600).await;
    env.fake.st().supports_locks = false;
    let mut a = env.join("ada", true).await;
    env.fake.change_outside(env.file, br#"{"A1":"changed in Nextcloud"}"#);
    a.edit("A4", "live").await;
    env.save_now(&a).await;
    assert!(a.pump_until(T, |a| a.has_control("notice")).await, "{:?}", env.fake.log());
    assert_eq!(env.file_json()["A1"], "changed in Nextcloud", "the outside change is kept");
    assert!(env.fake.st().files.values().any(|f| f.path.contains("live copy")));
    assert_eq!(env.status(&a).await["lock"], "none", "no lock when the host has none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_tokens() {
    let env = Env::new("expired", false, |_| {}).await;
    let t = env.fake.token("ada", "Ada", env.file, true);
    env.fake.st().tokens.get_mut(&t).unwrap().expired = true;
    let (status, embed, html) = env.open_with(&t, "").await;
    assert_eq!(status, 403);
    assert!(embed.is_none());
    assert!(html.contains("Frame_Ready"), "the problem page still answers Nextcloud's handshake");
    // A token that expires while the room is open: saving is refused and people are told.
    let mut a = env.join("ada", true).await;
    for tok in env.fake.st().tokens.values_mut() {
        tok.expired = true;
    }
    a.edit("A6", "late").await;
    env.save_now(&a).await;
    assert!(a.pump_until(T, |a| a.control("notice").is_some_and(|n| n["level"] == "error")).await, "{:?}", a.controls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_allow_list_and_bad_requests() {
    let env = Env::new("allow", false, |_| {}).await;
    let url = format!("{}/edit/gridcraft?WOPISrc={}", env.cfg.public_url, pct("https://evil.example/index.php/apps/richdocuments/wopi/files/1_x"));
    let r = env.http.post(url).form(&[("access_token", "x")]).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let (status, embed, _) = env.open_with("no-such-token", "").await;
    assert_eq!(status, 403);
    assert!(embed.is_none());
    let r = env.http.post(format!("{}/edit/nope?WOPISrc=x", env.cfg.public_url)).send().await.unwrap();
    assert_eq!(r.status(), 404);
    // A WebSocket without a session, with another room's session, from another origin.
    assert!(YClient::connect(&format!("ws://{}/ws/nc%3Ax%3A1", env.server.as_ref().unwrap().addr), "nope").await.is_err());
    let embed = env.open("ada", true).await;
    let other = format!("ws://{}/ws/nc%3Aelse%3A1", env.server.as_ref().unwrap().addr);
    assert!(YClient::connect(&other, embed["token"].as_str().unwrap()).await.is_err());
    // Multipart form posts (what Nextcloud sends) work too.
    let token = env.fake.token("ada", "Ada", env.file, true);
    let form = reqwest::multipart::Form::new().text("access_token", token).text("access_token_ttl", "0");
    let url = format!("{}/edit/gridcraft?WOPISrc={}", env.cfg.public_url, pct(&env.fake.wopi_src(env.file)));
    let r = env.http.post(url).multipart(form).send().await.unwrap();
    assert_eq!(r.status(), 200);
    // View mode from the discovery's view action.
    let token = env.fake.token("dan", "Dan", env.file, true);
    let (_, embed, _) = env.open_with(&token, "&mode=view").await;
    assert_eq!(embed.unwrap()["role"], "view");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_input_is_dropped() {
    let env = Env::new("hostile", false, |_| {}).await;
    let mut a = env.join("ada", true).await;
    let mut b = env.join("bob", true).await;
    for raw in [vec![], vec![0], vec![0, 0], vec![0, 2, 200, 1], vec![0xff; 20], vec![1, 3, 1, 2], vec![100, 3, b'{', b'x', b'}']] {
        b.send(WFrame::Raw(raw)).await;
    }
    b.send(WFrame::Update(vec![1, 2, 3, 4, 5, 6, 7])).await;
    b.send(WFrame::Sync1(vec![0xff, 0xff, 0xff])).await;
    b.send(WFrame::Awareness(vec![0xff, 0xff, 0xff, 0x7f])).await;
    // Bob claims Ada's awareness client id.
    let aw = awareness(a.client_id(), 1, &json!({"user": {"name": "ada"}}));
    a.send(WFrame::Awareness(aw)).await;
    a.pump(Duration::from_millis(200)).await;
    let spoof = awareness(a.client_id(), 99, &json!({"user": {"name": "not ada"}}));
    b.send(WFrame::Awareness(spoof.clone())).await;
    b.send(WFrame::Control(json!({"type": "saveResult", "id": 999, "ok": true}))).await;
    b.send(WFrame::Control(json!({"type": "hello", "clientId": "x", "can": [1, "rm -rf"]}))).await;
    b.edit("ok", "still works").await;
    assert!(converge(&mut [&mut a, &mut b]).await);
    assert_eq!(a.get("ok").as_deref(), Some("still works"));
    assert!(!a.awareness.contains(&spoof), "a spoofed awareness isn't relayed");
    let st = env.status(&a).await;
    assert!(st["dropped"].as_u64().unwrap() >= 3, "{st}");
    // An upload nobody asked for.
    let room = a.control("welcome").unwrap()["room"].as_str().unwrap().to_string();
    let r = env.http.put(format!("{}/api/v1/rooms/{}/file?save=77", env.cfg.public_url, pct(&room))).bearer_auth(&a.token).body("x").send().await.unwrap();
    assert_eq!(r.status(), 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_room_survives_a_restart() {
    let mut env = Env::new("restart", false, |_| {}).await;
    let mut a = env.join("ada", true).await;
    a.edit("R1", "before restart").await;
    a.pump(Duration::from_millis(300)).await;
    a.close().await;
    env.server.take().unwrap().stop().await;
    // A new server on the same data folder (a crash would lose at most the last 2 seconds).
    let server = craft_live::start(env.cfg.clone()).await.unwrap();
    env.server = Some(server);
    let b = env.join("bob", true).await;
    assert_eq!(b.control("welcome").unwrap()["seed"], false, "the room was seeded before the restart");
    assert_eq!(b.get("R1").as_deref(), Some("before restart"));
    assert_eq!(b.get("A1").as_deref(), Some("Item"));
    let st = env.status(&b).await;
    assert_eq!(st["dirty"], true, "the unsaved edit is still to be saved");
    b.close().await;
    // A file changed in Nextcloud while the room was closed: the room loads it again, and the
    // unsaved edits are set aside, never lost.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    env.fake.change_outside(env.file, br#"{"A1":"new version"}"#);
    let c = env.join("carol", true).await;
    assert_eq!(c.control("welcome").unwrap()["seed"], true);
    assert_eq!(c.get("A1").as_deref(), Some("new version"));
    assert_eq!(c.get("R1"), None);
    let orphans = std::fs::read_dir(env.cfg.data_dir.join("rooms"))
        .unwrap()
        .flatten()
        .any(|d| std::fs::read_dir(d.path()).unwrap().flatten().any(|f| f.file_name().to_string_lossy().starts_with("orphan-")));
    assert!(orphans, "the unsaved edits were set aside");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_seeder_leaving_hands_over() {
    let env = Env::new("reseed", false, |_| {}).await;
    let e1 = env.open("ada", true).await;
    assert_eq!(e1["seed"], true);
    let mut s1 = env.connect(&e1).await;
    s1.pump_until(T, |c| c.has_control("welcome")).await;
    let e2 = env.open("bob", true).await;
    assert_eq!(e2["seed"], false, "one seeder at a time");
    let mut w = env.connect(&e2).await;
    w.sync1().await;
    w.pump(Duration::from_millis(300)).await;
    assert!(!w.synced, "a joiner waits for the seed");
    s1.close().await;
    assert!(w.pump_until(T, |w| w.has_control("reseed")).await);
    let again: Value = env.http.post(format!("{}/api/v1/prepare", env.cfg.public_url)).bearer_auth(&w.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(again["seed"], true, "the next one gets the lease");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_sessions_prove_identity() {
    let env = Env::new("desktop", false, |_| {}).await;
    env.fake.add_account("ada@example", "app-pass", "ada", "Ada Lovelace");
    let url = format!("{}/api/v1/sessions", env.cfg.public_url);
    let body = json!({"nextcloud": env.fake.base(), "path": "/Budget.xlsx", "fileId": env.file.to_string()});
    let r = env.http.post(&url).basic_auth("ada@example", Some("wrong")).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let bad = json!({"nextcloud": env.fake.base(), "path": "/Budget.xlsx", "fileId": "9999"});
    let r = env.http.post(&url).basic_auth("ada@example", Some("app-pass")).json(&bad).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let other = json!({"nextcloud": "https://other.example", "path": "/Budget.xlsx", "fileId": "1"});
    let r = env.http.post(&url).basic_auth("ada@example", Some("app-pass")).json(&other).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let s: Value = env.http.post(&url).basic_auth("ada@example", Some("app-pass")).json(&body).send().await.unwrap().json().await.unwrap();
    assert_eq!(s["user"]["name"], "Ada Lovelace");
    assert_eq!(s["role"], "edit");
    assert_eq!(s["seed"], true);
    // The web and the desktop meet in the same room.
    let web = env.open("bob", true).await;
    assert_eq!(web["room"], s["room"]);
    // A desktop session can't download through WOPI: it has its own WebDAV access.
    let r = env.http.get(s["file"].as_str().unwrap()).bearer_auth(s["token"].as_str().unwrap()).send().await.unwrap();
    assert_eq!(r.status(), 409);
    // Turned off.
    let mut env2 = Env::new("desktop-off", false, |c| c.desktop_sessions = false).await;
    let r = env2.http.post(format!("{}/api/v1/sessions", env2.cfg.public_url)).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403);
    env2.server.take().unwrap().stop().await;
}
