//! `twetch-stub [port]`: the stub gateway on 127.0.0.1, for trying TwetchCode without money.

#[tokio::main]
async fn main() {
    let port = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(48300u16);
    let world = twetch_stub::Shared::default();
    if let Ok(mut w) = world.lock() {
        w.chunk_delay = std::time::Duration::from_millis(60);
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("stub gateway on http://127.0.0.1:{port} (coins on /woc)");
    axum::serve(listener, twetch_stub::router(world)).await.expect("serve");
}
