//! A room's WebSocket to craft-live: one thread that connects (TLS when `wss://`), reads with a
//! short timeout so it can also write what the main loop hands it, pings, and reconnects with
//! backoff (1 s, 2 s, 4 s … 30 s). What it hears goes to the main loop as [`NetEvent`]s.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::wire::Frame;

/// Frames kept while offline at most (the oldest go first; the app syncs again on `online`).
const MAX_QUEUE: usize = 10_000;
const MAX_MESSAGE: usize = (16 << 20) + (1 << 20);

/// What the link reports.
#[derive(Debug)]
pub enum NetEvent {
    Connected,
    Disconnected(String),
    Frame(Frame),
    /// The server refused the session (HTTP status at the handshake: 401, 403…).
    Rejected(u16),
}

/// What the main loop tells the link.
#[derive(Debug)]
pub enum LinkCmd {
    Send(Frame),
    /// A new session token (after a renewal): reconnect with it.
    Token(String),
    Close,
}

/// The main loop's side of a link.
pub struct LinkHandle {
    tx: Sender<LinkCmd>,
}

impl LinkHandle {
    pub fn send(&self, f: Frame) -> bool {
        self.tx.send(LinkCmd::Send(f)).is_ok()
    }

    pub fn token(&self, t: String) {
        let _ = self.tx.send(LinkCmd::Token(t));
    }

    pub fn close(&self) {
        let _ = self.tx.send(LinkCmd::Close);
    }
}

impl Drop for LinkHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(LinkCmd::Close);
    }
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// Starts a link to `ws` (`wss://…/ws/<room>`) with a session token. `report` gets the events;
/// it returns `false` once the main loop is gone.
pub fn start(ws: String, token: String, report: impl Fn(NetEvent) -> bool + Send + 'static) -> Result<LinkHandle, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("live-link".into()).spawn(move || run(ws, token, rx, report)).map_err(|e| format!("can't start a thread: {e}"))?;
    Ok(LinkHandle { tx })
}

fn set_timeout(ws: &mut Ws, t: Duration) {
    let _ = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(t)),
        MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(t)),
        _ => Ok(()),
    };
}

fn connect(url: &str, token: &str) -> Result<Ws, (Option<u16>, String)> {
    let sep = if url.contains('?') { '&' } else { '?' };
    let full = format!("{url}{sep}token={token}");
    let cfg = WebSocketConfig { max_message_size: Some(MAX_MESSAGE), max_frame_size: Some(MAX_MESSAGE), ..WebSocketConfig::default() };
    match tungstenite::client::connect_with_config(full.as_str(), Some(cfg), 3) {
        Ok((mut ws, _)) => {
            set_timeout(&mut ws, Duration::from_millis(50));
            Ok(ws)
        }
        Err(tungstenite::Error::Http(r)) => Err((Some(r.status().as_u16()), format!("craft-live answered {}", r.status().as_u16()))),
        Err(e) => Err((None, e.to_string())),
    }
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Takes commands for up to `wait`; `false` when the link must stop.
fn take_cmds(rx: &Receiver<LinkCmd>, queue: &mut VecDeque<Frame>, token: &mut String, wait: Duration) -> Result<bool, ()> {
    let end = Instant::now() + wait;
    let mut new_token = false;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(LinkCmd::Send(f)) => {
                if queue.len() >= MAX_QUEUE {
                    queue.pop_front();
                }
                queue.push_back(f);
            }
            Ok(LinkCmd::Token(t)) => {
                *token = t;
                new_token = true;
                return Ok(new_token);
            }
            Ok(LinkCmd::Close) | Err(RecvTimeoutError::Disconnected) => return Err(()),
            Err(RecvTimeoutError::Timeout) => return Ok(new_token),
        }
    }
}

fn run(url: String, mut token: String, rx: Receiver<LinkCmd>, report: impl Fn(NetEvent) -> bool) {
    let mut queue: VecDeque<Frame> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut ws = match connect(&url, &token) {
            Ok(ws) => ws,
            Err((status, msg)) => {
                if let Some(s) = status.filter(|s| matches!(s, 401 | 403 | 404)) {
                    if !report(NetEvent::Rejected(s)) {
                        return;
                    }
                    // Wait for a new token (or the end).
                    loop {
                        match take_cmds(&rx, &mut queue, &mut token, Duration::from_secs(3600)) {
                            Ok(true) => break,
                            Ok(false) => continue,
                            Err(()) => return,
                        }
                    }
                    continue;
                }
                if !report(NetEvent::Disconnected(msg)) {
                    return;
                }
                if take_cmds(&rx, &mut queue, &mut token, backoff).is_err() {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        if !report(NetEvent::Connected) {
            let _ = ws.close(None);
            return;
        }
        let mut last_ping = Instant::now();
        let why = loop {
            // Commands first, without waiting.
            let mut stop = false;
            loop {
                match rx.try_recv() {
                    Ok(LinkCmd::Send(f)) => queue.push_back(f),
                    Ok(LinkCmd::Token(t)) => token = t,
                    Ok(LinkCmd::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            let mut failed = None;
            while let Some(f) = queue.pop_front() {
                if let Err(e) = ws.send(Message::Binary(f.encode())) {
                    queue.push_front(f);
                    failed = Some(e.to_string());
                    break;
                }
            }
            if stop {
                // Say goodbye, then go.
                let _ = ws.send(Message::Binary(Frame::Control("{\"type\":\"bye\"}".into()).encode()));
                let _ = ws.close(None);
                let _ = ws.flush();
                return;
            }
            if let Some(e) = failed {
                break e;
            }
            if last_ping.elapsed() >= Duration::from_secs(25) {
                last_ping = Instant::now();
                if ws.send(Message::Ping(Vec::new())).is_err() {
                    break "the connection dropped".to_string();
                }
            }
            match ws.read() {
                Ok(Message::Binary(b)) => {
                    if let Some(f) = Frame::decode(&b)
                        && !report(NetEvent::Frame(f))
                    {
                        let _ = ws.close(None);
                        return;
                    }
                }
                Ok(Message::Close(c)) => break c.map(|c| format!("closed: {} {}", u16::from(c.code), c.reason)).unwrap_or_else(|| "closed".into()),
                Ok(_) => {}
                Err(e) if would_block(&e) => {}
                Err(e) => break e.to_string(),
            }
        };
        if !report(NetEvent::Disconnected(why)) {
            return;
        }
        if take_cmds(&rx, &mut queue, &mut token, backoff).is_err() {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
