use std::{future::Future, net::SocketAddr};

use codex_cont::{
    app::create_router,
    config::{default_config_path, load_config},
    logging,
};

mod service;

fn main() {
    if let Err(err) = run_cli() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run_cli() -> Result<(), String> {
    let mut args = std::env::args();
    let _exe = args.next();
    let cmd = args.next();
    match cmd.as_deref() {
        None => block_on_server(std::future::pending(), || Ok(())),
        Some("service") => {
            ensure_no_extra(args)?;
            service::run()
        }
        Some("install") => {
            ensure_no_extra(args)?;
            service::install()
        }
        Some("uninstall") => {
            ensure_no_extra(args)?;
            service::uninstall()
        }
        Some("start") => {
            ensure_no_extra(args)?;
            service::start()
        }
        Some("stop") => {
            ensure_no_extra(args)?;
            service::stop()
        }
        Some("restart") => {
            ensure_no_extra(args)?;
            service::restart()
        }
        Some("-h") | Some("--help") | Some("help") => Err(usage().to_string()),
        Some(other) => Err(format!("unknown command: {other}\n{}", usage())),
    }
}

fn ensure_no_extra(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    match args.next() {
        Some(arg) => Err(format!("unexpected argument: {arg}\n{}", usage())),
        None => Ok(()),
    }
}

fn usage() -> &'static str {
    "usage: codex-cont.exe [service|install|uninstall|start|stop|restart]"
}

pub(crate) fn block_on_server(
    shutdown: impl Future<Output = ()> + Send + 'static,
    on_listening: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start async runtime: {e}"))?
        .block_on(run_server(shutdown, on_listening))
}

async fn run_server(
    shutdown: impl Future<Output = ()> + Send + 'static,
    on_listening: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let config_path = default_config_path()?;
    let cfg = load_config(config_path.clone())
        .map_err(|e| format!("failed to load {}: {e}", config_path.display()))?;
    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port)
        .parse()
        .map_err(|e| format!("invalid listen address: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;
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
    on_listening()?;
    axum::serve(listener, create_router(cfg))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| format!("server error: {e}"))
}
