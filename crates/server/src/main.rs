//! `craft-live`: the server. `craft-live --help` lists the commands.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use craft_live::config::{APPS, Config};

const USAGE: &str = "craft-live: a WOPI editor for Nextcloud Office with the craft apps

usage:
  craft-live [--config FILE]                 serve (the default)
  craft-live check [--config FILE]           print the configuration and what's missing
  craft-live rooms [--config FILE]           list the stored rooms (unsaved edits are marked)
  craft-live rotate-proof-key [--config FILE] make a new proof key (the old one stays valid
                                             for hosts that cached the discovery)
  craft-live --version

The configuration file is TOML (see craft-live.example.toml); CRAFT_LIVE_* environment
variables override it (docs/deploy.md).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("craft-live {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut config: Option<PathBuf> = None;
    let mut command = "serve".to_string();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => match it.next() {
                Some(p) => config = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--config needs a file");
                    return ExitCode::from(2);
                }
            },
            "serve" | "check" | "rooms" | "rotate-proof-key" => command = a.clone(),
            other => {
                eprintln!("unknown argument {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let cfg = match Config::load(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("craft-live: {e}");
            return ExitCode::from(2);
        }
    };
    match command.as_str() {
        "check" => check(&cfg),
        "rooms" => {
            for r in craft_live::store::list_rooms(&cfg.data_dir) {
                println!("{}{}  {}  version {}  {}", if r.dirty { "* " } else { "  " }, r.room, r.name, r.version, if r.seeded { "seeded" } else { "empty" });
            }
            ExitCode::SUCCESS
        }
        "rotate-proof-key" => match craft_live::proof::ProofKeys::rotate(&cfg.data_dir) {
            Ok(_) => {
                println!(
                    "New proof key in {}; restart craft-live and let Nextcloud fetch the discovery again (occ richdocuments:activate-config).",
                    cfg.data_dir.display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("craft-live: {e}");
                ExitCode::FAILURE
            }
        },
        _ => serve(cfg),
    }
}

fn check(cfg: &Config) -> ExitCode {
    let mut ok = true;
    println!("listen      {}", cfg.listen);
    println!("public_url  {}", cfg.public_url);
    println!("data_dir    {}", cfg.data_dir.display());
    println!("proof keys  {}", if cfg.proof_keys { "on" } else { "off" });
    if cfg.wopi_hosts.is_empty() {
        println!("wopi_hosts  (none: no Nextcloud can open files)");
        ok = false;
    }
    for h in &cfg.wopi_hosts {
        println!("wopi_host   {h}");
    }
    for app in APPS {
        match cfg.apps.get(app) {
            Some(dir) if dir.join("index.html").is_file() => println!("{app:<11} {}", dir.display()),
            Some(dir) => {
                println!("{app:<11} {} (no index.html there)", dir.display());
                ok = false;
            }
            None => println!("{app:<11} (not configured: its formats aren't in the discovery)"),
        }
    }
    if cfg.public_url.split("://").nth(1).is_some_and(|r| r.contains('/') || r.contains(':')) {
        println!("note: Nextcloud drops a path or port from the editor's address; serve craft-live at the root of its own host on 443");
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn serve(cfg: Config) -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CRAFT_LIVE_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("craft-live: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let running = match craft_live::start(cfg).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("craft-live: {e}");
                return ExitCode::FAILURE;
            }
        };
        let stop = async {
            #[cfg(unix)]
            {
                if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                    return;
                }
            }
            let _ = tokio::signal::ctrl_c().await;
        };
        stop.await;
        tracing::info!("stopping: saving the rooms' state");
        running.stop().await;
        ExitCode::SUCCESS
    })
}
