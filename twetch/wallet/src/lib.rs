//! TwetchCode's wallet sidecar: an OpenAI-compatible proxy on 127.0.0.1 that
//! pays the Twetch inference gateway in BSV, one transaction per request.

pub mod auth;
pub mod chain;
pub mod db;
pub mod engine;
pub mod gateway;
pub mod model;
pub mod proxy;
pub mod secrets;
pub mod signer;
pub mod sse;
pub mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;

/// The only address a payment may go to: Twetch @0.
pub const PAY_TO_PUBKEY: &str = "024d9e9c9804d364c3e79c7f47dc6048b1afa054ff67ac160d44082a8482184eaf";
pub const PAY_TO: &str = "1J82Vbt4Uz8kKr2Xb3Kro9WtCh6udoctCJ";
pub const ACTION: &str = "twetch/inference@0.0.1";
/// Satoshis per 1000 bytes, as the Twetch web wallet pays.
pub const FEE_PER_KB: u64 = 105;
pub const DEFAULT_PORT: u16 = 48213;
pub const DEFAULT_GATEWAY: &str = "https://ai.twetch.com";
/// Empty by default: uses Twetch's P2P wallet endpoints on the gateway instead of WhatsOnChain.
pub const DEFAULT_WOC: &str = "";

#[derive(Clone)]
pub struct Config {
    pub home: PathBuf,
    /// 0 picks a free port.
    pub port: u16,
    pub gateway: String,
    pub woc: String,
}

/// A running sidecar.
pub struct Running {
    pub addr: SocketAddr,
    pub admin_token: String,
    pub proxy_token: String,
    pub engine: std::sync::Arc<engine::Engine>,
    pub server: tokio::task::JoinHandle<()>,
    pub worker: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Stop without finishing anything, as a crash would.
    pub fn abort(&self) {
        self.server.abort();
        self.worker.abort();
    }
}

/// Start the sidecar: open the store, the keychain secret and the listener.
///
/// # Errors
/// When the store is locked by another sidecar or the port cannot be bound.
pub async fn start(cfg: Config, secrets: std::sync::Arc<dyn secrets::SecretStore>) -> anyhow::Result<Running> {
    let dir = cfg.home.join("wallet");
    auth::private_dir(&dir)?;
    let db = db::Db::open(&dir.join("wallet.redb"))?;
    let signer = signer::Signer::open(secrets.clone(), &db)?;
    let gateway = gateway::Gateway::new(&cfg.gateway, secrets, signer.clone())?;
    let chain = (!cfg.woc.trim().is_empty()).then(|| chain::Chain::new(&cfg.woc)).transpose()?;
    let engine = std::sync::Arc::new(engine::Engine::new(db, gateway, chain, signer));
    let proxy_token = auth::load_or_create_token(&dir.join("proxy-token"))?;
    let admin_token = auth::random_token()?;
    let listener = bind(cfg.port).await?;
    let addr = listener.local_addr()?;
    let state = std::sync::Arc::new(proxy::App::new(engine.clone(), auth::Auth::new(addr.port(), &proxy_token, &admin_token)));
    let worker = tokio::spawn(engine.clone().run());
    let server = tokio::spawn(async move {
        use axum::serve::ListenerExt;
        // A token is one small write: never hold it back for the next one.
        let listener = listener.tap_io(|s| {
            let _ = s.set_nodelay(true);
        });
        if let Err(e) = axum::serve(listener, proxy::router(state)).await {
            eprintln!("twetch-wallet: server stopped: {e}");
        }
    });
    Ok(Running { addr, admin_token, proxy_token, engine, server, worker })
}

/// Bind the configured port, or any free one when another program holds it.
async fn bind(port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?),
        Err(e) => Err(e.into()),
    }
}
