//! Browser diagnostics receiver used by isolated validation.
//!
//! This binary intentionally compiles only the SQLite/browser-diagnostics
//! modules and does not link the packet-capture stack. That lets Windows E2E
//! validate Receiver behavior without requiring the Npcap runtime DLL.

#[path = "../app_storage.rs"]
mod app_storage;
#[path = "../browser_activity.rs"]
mod browser_activity;
#[path = "../model.rs"]
mod model;
#[path = "../storage.rs"]
mod storage;

use browser_activity::BrowserActivityServer;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut database: Option<PathBuf> = None;
    let mut port: u16 = 0;

    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "--db" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--db requires a path"))?;
                database = Some(PathBuf::from(value));
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--port requires a value"))?;
                port = value
                    .to_string_lossy()
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("--port must be in 0..=65535"))?;
                if port != 0 && port < 1024 {
                    anyhow::bail!("--port must be 0 or in 1024..=65535");
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: win-domain-flow-browser-receiver --db <PATH> [--port <PORT>]\n\n\
                     PORT 0 asks Windows for an ephemeral loopback port."
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let database = database.ok_or_else(|| anyhow::anyhow!("--db is required"))?;
    let server = BrowserActivityServer::spawn_on(database, port)?;
    let status = server.status();
    println!(
        "pid={}\tport={}\tdb={}",
        status.pid, status.port, status.database_path
    );

    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })?;

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    server.shutdown()?;
    Ok(())
}
