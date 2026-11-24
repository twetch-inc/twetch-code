//! `twetch-wallet`: started by TwetchCode, one JSON line on stdout when ready.
//!
//! It stops when its stdin closes (the app is gone) or on SIGTERM / Ctrl-C.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use twetch_wallet::{secrets::FileSecrets, Config, DEFAULT_GATEWAY, DEFAULT_PORT, DEFAULT_WOC};

#[derive(Serialize)]
struct Ready<'a> {
    ready: bool,
    port: u16,
    /// Only on this pipe, never on disk: it opens the wallet page.
    admin_token: &'a str,
    proxy_token: &'a str,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let user_home = env("HOME").or_else(|| env("USERPROFILE")).map(PathBuf::from);
    let default_home = user_home.map(|h| h.join(".twetch-code"));
    let home = env("TWETCHCODE_HOME").map(PathBuf::from).or_else(|| default_home.clone());
    let home = home.ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let secrets = FileSecrets::new(home.join("wallet").join("secrets.json"));
    // The same port every start: OpenCode's config names it.
    let saved = home.join("wallet").join("port");
    let port = match env("TWETCH_WALLET_PORT") {
        Some(p) => p.parse()?,
        None => std::fs::read_to_string(&saved).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(DEFAULT_PORT),
    };
    let cfg = Config {
        home,
        port,
        gateway: env("TWETCH_AI_URL").unwrap_or_else(|| DEFAULT_GATEWAY.into()),
        woc: env("TWETCH_WALLET_WOC_URL").unwrap_or_else(|| DEFAULT_WOC.into()),
    };
    let running = twetch_wallet::start(cfg, Arc::new(secrets)).await?;
    std::fs::write(&saved, running.addr.port().to_string())?;
    let line = Ready { ready: true, port: running.addr.port(), admin_token: &running.admin_token, proxy_token: &running.proxy_token };
    println!("{}", serde_json::to_string(&line)?);
    until_told().await?;
    running.abort();
    Ok(())
}

async fn until_told() -> anyhow::Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 256];
    let parent_gone = async { while stdin.read(&mut buf).await.is_ok_and(|n| n > 0) {} };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { () = parent_gone => {}, _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    tokio::select! { () = parent_gone => {}, _ = tokio::signal::ctrl_c() => {} }
    Ok(())
}
