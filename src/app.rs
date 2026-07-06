use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    codex::{build_round_payload, declares_continue_tool, reasoning_enabled, repair_followup_input},
    config::Config,
    creds::{build_upstream_headers, would_inject_authorization},
    logging,
    proxy::{collect_body, fold_stream, open_passthrough, open_round, HeaderMapStr},
    store::IdStore,
};

#[derive(Clone)]
pub struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
    id_store: Arc<IdStore>,
    request_seq: Arc<AtomicU64>,
}

pub fn create_router(cfg: Config) -> Router {
    let paths = cfg.server.listen_paths.clone();
    let state = AppState {
        cfg: Arc::new(cfg),
        client: make_client(),
        id_store: Arc::new(IdStore::default()),
        request_seq: Arc::new(AtomicU64::new(1)),
    };
    let mut router = Router::new();
    for path in paths {
        router = router.route(&path, post(handle_responses));
    }
    router.with_state(state).layer(DefaultBodyLimit::disable())
}

pub fn make_client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .expect("reqwest client build failed")
}

pub fn resolve_upstream_url(cfg: &Config, headers: &HeaderMap) -> Option<String> {
    if cfg.upstream.mode == "header" || cfg.upstream.mode == "header_required" {
        if let Some(base) = header_base(headers) {
            return Some(join_responses(&base));
        }
        if cfg.upstream.mode == "header_required" {
            return None;
        }
    }
    Some(cfg.upstream.url.clone())
}

pub fn url_is_from_header(cfg: &Config, headers: &HeaderMap) -> bool {
    (cfg.upstream.mode == "header" || cfg.upstream.mode == "header_required")
        && header_base(headers).is_some()
}

async fn handle_responses(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let cfg = state.cfg.clone();
    let request_id = state.request_seq.fetch_add(1, Ordering::Relaxed);
    logging::info(
        cfg.as_ref(),
        format!("request_id={request_id} path={} body_bytes={}", uri.path(), raw.len()),
    );
    let mut body: Value = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(_) => {
            logging::warn(cfg.as_ref(), format!("request_id={request_id} invalid JSON body"));
            return json_error(StatusCode::BAD_REQUEST, "invalid JSON body");
        }
    };
    if !body.is_object() {
        logging::warn(cfg.as_ref(), format!("request_id={request_id} body must be a JSON object"));
        return json_error(StatusCode::BAD_REQUEST, "body must be a JSON object");
    }

    let Some(url) = resolve_upstream_url(&cfg, &headers) else {
        logging::warn(
            cfg.as_ref(),
            format!("request_id={request_id} missing Responses-API-Base header"),
        );
        return json_error(
            StatusCode::BAD_REQUEST,
            "Responses-API-Base header is required (upstream mode=header_required)",
        );
    };

    if url_is_from_header(&cfg, &headers)
        && would_inject_authorization(&cfg, headers.get("authorization").is_some())
    {
        logging::warn(
            cfg.as_ref(),
            format!("request_id={request_id} auth conflict with Responses-API-Base"),
        );
        return json_error(
            StatusCode::BAD_REQUEST,
            "When overriding the upstream base (Responses-API-Base), the request must provide its own Authorization; the proxy will not send its configured credentials to an externally supplied URL.",
        );
    }

    let stream_requested = truthy(body.get("stream"));
    let reasoning_requested = reasoning_enabled(&body);
    let collision = cfg.cont.method == "tool_pair"
        && declares_continue_tool(&body, &cfg.cont.continue_tool_name);
    let should_fold = cfg.cont.enabled && stream_requested && reasoning_requested && !collision;
    logging::info(
        cfg.as_ref(),
        format!(
            "request_id={request_id} stream={stream_requested} reasoning={reasoning_requested} mode={}",
            if should_fold { "fold" } else { "passthrough" }
        ),
    );

    if !should_fold {
        return passthrough(&state.client, &cfg, &headers, raw, &url, request_id).await;
    }

    if cfg.cont.repair_followup == "stateful" && cfg.cont.method == "tool_pair" {
        let repaired = repair_followup_input(
            body.get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            state.id_store.as_ref(),
            &cfg.cont.continue_tool_name,
            &cfg.cont.continue_output_text,
        );
        body["input"] = Value::Array(repaired);
    }

    let upstream_headers = upstream_headers(&headers, &cfg);
    let payload = build_round_payload(
        &body,
        body.get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        cfg.stream.force_include_encrypted,
        false,
    );
    let first = match open_round(&state.client, &url, &payload, &upstream_headers).await {
        Ok(resp) => {
            logging::info(
                cfg.as_ref(),
                format!("request_id={request_id} upstream status={}", resp.status),
            );
            resp
        }
        Err(err) => {
            logging::error(
                cfg.as_ref(),
                format!("request_id={request_id} upstream open failed: {err}"),
            );
            return json_error(StatusCode::BAD_GATEWAY, &err);
        }
    };
    if first.status >= 400 {
        let status = status(first.status);
        let content_type = first.content_type.clone();
        let body = collect_body(first, None).await;
        return body_response(status, content_type, Body::from(body));
    }

    body_response(
        StatusCode::OK,
        Some("text/event-stream".to_string()),
        Body::from_stream(fold_stream(
            state.client.clone(),
            cfg,
            body,
            upstream_headers,
            first,
            state.id_store.clone(),
            url,
            request_id,
        )),
    )
}

async fn passthrough(
    client: &reqwest::Client,
    cfg: &Config,
    headers: &HeaderMap,
    raw: Bytes,
    url: &str,
    request_id: u64,
) -> Response {
    let upstream_headers = upstream_headers(headers, cfg);
    match open_passthrough(client, url, raw, &upstream_headers).await {
        Ok(resp) => {
            logging::info(
                cfg,
                format!("request_id={request_id} upstream status={}", resp.status),
            );
            let status = status(resp.status);
            let content_type = resp.content_type.clone();
            let stream = resp.body.map(|r| r.map_err(|e| io::Error::new(io::ErrorKind::Other, e)));
            body_response(status, content_type, Body::from_stream(stream))
        }
        Err(err) => {
            logging::error(cfg, format!("request_id={request_id} upstream open failed: {err}"));
            json_error(StatusCode::BAD_GATEWAY, &err)
        }
    }
}

fn upstream_headers(headers: &HeaderMap, cfg: &Config) -> HeaderMapStr {
    build_upstream_headers(
        headers.iter().filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        }),
        cfg,
    )
}

fn header_base(headers: &HeaderMap) -> Option<String> {
    headers
        .get("responses-api-base")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn join_responses(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}/responses")
    }
}

fn json_error(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn body_response(status: StatusCode, content_type: Option<String>, body: Body) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn status(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Null) | None => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(xs)) => !xs.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}
