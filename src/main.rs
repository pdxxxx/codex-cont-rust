use std::{net::SocketAddr, path::PathBuf};

use codex_cont::{app::create_router, config::load_config, logging};

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
    let bound_addr = listener.local_addr().unwrap_or(addr);
    logging::info(&cfg, format!("listening=http://{bound_addr}"));
    logging::info(
        &cfg,
        format!(
            "paths={}",
            cfg.server
                .listen_paths
                .iter()
                .map(|path| format!("http://{bound_addr}{path}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    let dump = if cfg.log.dump_rounds_dir.is_empty() {
        String::new()
    } else {
        format!(" dump_rounds_dir={}", cfg.log.dump_rounds_dir)
    };
    logging::info(
        &cfg,
        format!(
            "config upstream.mode={} auth.mode={} continue.enabled={} continue.method={} continue.max_continue={} log.level={}{}",
            cfg.upstream.mode,
            cfg.auth.mode,
            cfg.cont.enabled,
            cfg.cont.method,
            cfg.cont.max_continue,
            logging::configured_level_name(&cfg),
            dump
        ),
    );
    if let Err(err) = axum::serve(listener, create_router(cfg)).await {
        eprintln!("server error: {err}");
    }
}
