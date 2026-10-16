//! craft-live: a WOPI editor server for Nextcloud Office that opens spreadsheets, documents and
//! presentations in the craft apps (GridCraft, WordCraft, DeckCraft), with live co-editing over
//! the y-websocket binding of craft plugin protocol 2. `docs/design.md` explains how it works.
//!
//! - [`config`]: the configuration and the allowed WOPI hosts.
//! - [`discovery`]: `/hosting/discovery` and `/hosting/capabilities`.
//! - [`wopi`], [`proof`]: the WOPI client and its proof keys.
//! - [`http`], [`shell`]: the endpoints and the editor page.
//! - [`room`], [`ywire`]: rooms (the shared Yjs document, seeding, saving, locks) and the wire.
//! - [`session`], [`nextcloud`], [`store`], [`ids`]: sessions, desktop identity checks, the data
//!   folder, small helpers.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing))]
#![forbid(unsafe_code)]

pub mod config;
pub mod discovery;
pub mod http;
pub mod ids;
pub mod nextcloud;
pub mod proof;
pub mod room;
pub mod session;
pub mod shell;
pub mod store;
pub mod wopi;
pub mod ywire;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::room::{Ctx, Registry};

/// A running server.
pub struct Running {
    /// Where it listens (useful with port 0).
    pub addr: SocketAddr,
    pub rooms: Registry,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Running {
    /// Stops accepting requests, writes every room to disk and waits for the server to end.
    pub async fn stop(mut self) {
        self.rooms.flush_all(true).await;
        if let Some(s) = self.shutdown.take() {
            let _ = s.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.task).await;
        self.task.abort();
    }

    /// Waits until the server ends (it ends on its own only if it fails).
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// The server's id in `/hosting/capabilities`, kept in the data folder.
fn server_id(cfg: &Config) -> String {
    let path = cfg.data_dir.join("server-id");
    if let Ok(Some(b)) = store::read_limited(&path, 64) {
        let id = String::from_utf8_lossy(&b).trim().to_string();
        if (4..=32).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return id;
        }
    }
    let id: String = ids::token(6).chars().filter(char::is_ascii_alphanumeric).take(8).collect();
    let _ = store::write_atomic(&path, id.as_bytes());
    id
}

/// Starts the server on `cfg.listen`.
pub async fn start(cfg: Config) -> Result<Running, String> {
    if !ids::os_random_works() {
        return Err("the operating system's random number generator doesn't work".into());
    }
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("can't create {}: {e}", cfg.data_dir.display()))?;
    let proof = if cfg.proof_keys { Some(Arc::new(proof::ProofKeys::load_or_create(&cfg.data_dir)?)) } else { None };
    let wopi = wopi::Wopi::new(proof.clone(), cfg.verify_tls, Duration::from_secs(120))?;
    let sessions = session::Sessions::open(Some(cfg.data_dir.clone()));
    let addr = cfg.listen_addr()?;
    let ctx = Arc::new(Ctx { server_id: server_id(&cfg), cfg, wopi, sessions });
    let rooms = Registry::new(ctx);
    let app = Arc::new(http::App::new(rooms.clone(), proof));
    let router = http::router(app);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("can't listen on {addr}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = rx.await;
        });
        if let Err(e) = serve.await {
            tracing::error!("the server stopped: {e}");
        }
    });
    tracing::info!("craft-live {} listening on {addr}", env!("CARGO_PKG_VERSION"));
    Ok(Running { addr, rooms, shutdown: Some(tx), task })
}
