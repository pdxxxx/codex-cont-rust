use std::{net::SocketAddr, path::PathBuf};

use codex_cont::{app::create_router, config::load_config};

#[tokio::main]
async fn main() {
    let cfg = match load_config(PathBuf::from("config.toml")) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("failed to load config.toml: {err}");
            std::process::exit(1);
        }
    };
    let addr: SocketAddr = match format!("{}:{}", cfg.server.host, cfg.server.port).parse() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("invalid listen address: {err}");
            std::process::exit(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = axum::serve(listener, create_router(cfg)).await {
        eprintln!("server error: {err}");
    }
}
