//! A y-websocket client that plays a craft app's web build in the tests: a Yjs document with a
//! map `cells`, the protocol-2 Appendix B framing, craft-live's control messages, and saving by
//! uploading the document as "file bytes" (the test app's file format is the JSON of its cells).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact, Update};

/// A y-websocket message (an independent copy of the server's framing, so the tests check it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WFrame {
    Sync1(Vec<u8>),
    Sync2(Vec<u8>),
    Update(Vec<u8>),
    Awareness(Vec<u8>),
    Denied(String),
    Control(Value),
    Raw(Vec<u8>),
}

fn var(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn bytes(out: &mut Vec<u8>, d: &[u8]) {
    var(out, d.len() as u64);
    out.extend_from_slice(d);
}

fn read_var(d: &[u8], pos: &mut usize) -> Option<u64> {
    let mut n = 0u64;
    let mut shift = 0;
    loop {
        let b = *d.get(*pos)?;
        *pos += 1;
        n |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn read_bytes<'a>(d: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let n = read_var(d, pos)? as usize;
    let out = d.get(*pos..*pos + n)?;
    *pos += n;
    Some(out)
}

impl WFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            WFrame::Sync1(d) => {
                o.extend_from_slice(&[0, 0]);
                bytes(&mut o, d);
            }
            WFrame::Sync2(d) => {
                o.extend_from_slice(&[0, 1]);
                bytes(&mut o, d);
            }
            WFrame::Update(d) => {
                o.extend_from_slice(&[0, 2]);
                bytes(&mut o, d);
            }
            WFrame::Awareness(d) => {
                o.push(1);
                bytes(&mut o, d);
            }
            WFrame::Denied(r) => {
                o.extend_from_slice(&[2, 0]);
                bytes(&mut o, r.as_bytes());
            }
            WFrame::Control(v) => {
                var(&mut o, 100);
                bytes(&mut o, v.to_string().as_bytes());
            }
            WFrame::Raw(d) => o.extend_from_slice(d),
        }
        o
    }

    pub fn decode(d: &[u8]) -> Option<WFrame> {
        let mut p = 0;
        Some(match read_var(d, &mut p)? {
            0 => {
                let step = read_var(d, &mut p)?;
                let b = read_bytes(d, &mut p)?.to_vec();
                match step {
                    0 => WFrame::Sync1(b),
                    1 => WFrame::Sync2(b),
                    2 => WFrame::Update(b),
                    _ => return None,
                }
            }
            1 => WFrame::Awareness(read_bytes(d, &mut p)?.to_vec()),
            2 => {
                read_var(d, &mut p)?;
                WFrame::Denied(String::from_utf8_lossy(read_bytes(d, &mut p)?).into_owned())
            }
            100 => WFrame::Control(serde_json::from_slice(read_bytes(d, &mut p)?).ok()?),
            _ => return None,
        })
    }
}

/// An awareness update with one client's state.
pub fn awareness(client: u64, clock: u64, state: &Value) -> Vec<u8> {
    let mut o = Vec::new();
    var(&mut o, 1);
    var(&mut o, client);
    var(&mut o, clock);
    bytes(&mut o, state.to_string().as_bytes());
    o
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A simulated web client.
pub struct YClient {
    pub doc: Doc,
    ws: Ws,
    pub token: String,
    pub controls: Vec<Value>,
    pub awareness: Vec<Vec<u8>>,
    /// Got a sync step 2 (the room's document).
    pub synced: bool,
    pub closed: Option<String>,
    /// Answer `save` requests by uploading the document.
    pub auto_save: bool,
    pub uploads: Vec<Value>,
    pub denied: Option<String>,
    http: reqwest::Client,
    /// The name given with uploads (default: the room's file name from `welcome`).
    pub upload_name: Option<String>,
}

impl YClient {
    /// Connects to `ws` (`ws://…/ws/<room>`) with a session token.
    pub async fn connect(ws: &str, token: &str) -> Result<YClient, String> {
        let url = format!("{ws}?token={token}");
        let (ws, _) = tokio_tungstenite::connect_async(url).await.map_err(|e| e.to_string())?;
        Ok(YClient {
            doc: Doc::new(),
            ws,
            token: token.into(),
            controls: Vec::new(),
            awareness: Vec::new(),
            synced: false,
            closed: None,
            auto_save: true,
            uploads: Vec::new(),
            denied: None,
            http: reqwest::Client::new(),
            upload_name: None,
        })
    }

    pub fn client_id(&self) -> u64 {
        self.doc.client_id().get()
    }

    pub async fn send(&mut self, f: WFrame) {
        let _ = self.ws.send(Message::Binary(f.encode())).await;
    }

    pub async fn hello(&mut self) {
        let id = self.client_id();
        self.send(WFrame::Control(json!({"type": "hello", "clientId": id, "can": ["upload"], "client": "test"}))).await;
    }

    /// Sync step 1 with the document's state vector.
    pub async fn sync1(&mut self) {
        let sv = self.doc.transact().state_vector().encode_v1();
        self.send(WFrame::Sync1(sv)).await;
    }

    /// The last control message of a type.
    pub fn control(&self, t: &str) -> Option<&Value> {
        self.controls.iter().rev().find(|c| c["type"] == t)
    }

    pub fn has_control(&self, t: &str) -> bool {
        self.control(t).is_some()
    }

    async fn handle(&mut self, f: WFrame) {
        match f {
            WFrame::Sync1(sv) => {
                let d = StateVector::decode_v1(&sv).map(|sv| self.doc.transact().encode_diff_v1(&sv)).unwrap_or_default();
                self.send(WFrame::Sync2(d)).await;
            }
            WFrame::Sync2(u) | WFrame::Update(u) => {
                if let Ok(u) = Update::decode_v1(&u) {
                    let _ = self.doc.transact_mut().apply_update(u);
                }
                self.synced = true;
            }
            WFrame::Awareness(a) => self.awareness.push(a),
            WFrame::Denied(r) => self.denied = Some(r),
            WFrame::Control(v) => {
                if v["type"] == "save" && v["mode"] == "upload" && self.auto_save {
                    let url = v["url"].as_str().unwrap_or("").to_string();
                    let name = self.upload_name.clone().or_else(|| v["name"].as_str().map(str::to_string)).unwrap_or_default();
                    let body = self.file_bytes();
                    let r = self
                        .http
                        .put(url)
                        .bearer_auth(&self.token)
                        .header(
                            "X-Craft-File-Name",
                            name.bytes()
                                .map(|b| if b.is_ascii_alphanumeric() || b"-._".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
                                .collect::<String>(),
                        )
                        .body(body)
                        .send()
                        .await;
                    let answer = match r {
                        Ok(r) => {
                            let status = r.status().as_u16();
                            let mut j: Value = r.json().await.unwrap_or(Value::Null);
                            j["status"] = json!(status);
                            j
                        }
                        Err(e) => json!({"error": e.to_string()}),
                    };
                    self.uploads.push(answer);
                }
                self.controls.push(v);
            }
            WFrame::Raw(_) => {}
        }
    }

    /// Handles what arrives for `dur`.
    pub async fn pump(&mut self, dur: Duration) {
        self.pump_until(dur, |_| false).await;
    }

    /// Handles what arrives until `done` holds (true) or `timeout` passes (false).
    pub async fn pump_until(&mut self, timeout: Duration, done: impl Fn(&YClient) -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if done(self) {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || self.closed.is_some() {
                return done(self);
            }
            match tokio::time::timeout(left.min(Duration::from_millis(50)), self.ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => {
                    if let Some(f) = WFrame::decode(&b) {
                        self.handle(f).await;
                    }
                }
                Ok(Some(Ok(Message::Close(c)))) => self.closed = Some(c.map(|c| format!("{} {}", c.code, c.reason)).unwrap_or_default()),
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => self.closed = Some(e.to_string()),
                Ok(None) => self.closed = Some("closed".into()),
                Err(_) => {}
            }
        }
    }

    /// Sets a cell and sends the update (one transaction = one update).
    pub async fn edit(&mut self, key: &str, value: &str) {
        let u = self.set_local(&[(key, value)]);
        self.send(WFrame::Update(u)).await;
    }

    /// Sets cells locally; the update.
    pub fn set_local(&self, cells: &[(&str, &str)]) -> Vec<u8> {
        let map = self.doc.get_or_insert_map("cells");
        let before = self.doc.transact().state_vector();
        {
            let mut t = self.doc.transact_mut();
            for (k, v) in cells {
                map.insert(&mut t, *k, *v);
            }
        }
        self.doc.transact().encode_diff_v1(&before)
    }

    /// Seeds the document from file bytes (the test app's format) as the first client does.
    pub fn seed_from_file(&self, file: &[u8]) {
        let v: Value = serde_json::from_slice(file).unwrap_or(json!({}));
        let meta = self.doc.get_or_insert_map("meta");
        let map = self.doc.get_or_insert_map("cells");
        let mut t = self.doc.transact_mut();
        meta.insert(&mut t, "app", "testcraft");
        if let Some(o) = v.as_object() {
            for (k, val) in o {
                map.insert(&mut t, k.as_str(), val.as_str().unwrap_or(""));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let map = self.doc.get_or_insert_map("cells");
        let t = self.doc.transact();
        map.get(&t, key).map(|o| o.to_string(&t))
    }

    pub fn cells(&self) -> BTreeMap<String, String> {
        let map = self.doc.get_or_insert_map("cells");
        let t = self.doc.transact();
        map.iter(&t).map(|(k, v)| (k.to_string(), v.to_string(&t))).collect()
    }

    /// The document as the test app's file: the JSON of its cells.
    pub fn file_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.cells()).unwrap_or_default()
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}
