use std::{io, sync::Arc};

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
    proxy::{collect_body, fold_stream, open_passthrough, open_round, HeaderMapStr},
    store::IdStore,
};

#[derive(Clone)]
pub struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
    id_store: Arc<IdStore>,
}

pub fn create_router(cfg: Config) -> Router {
    let paths = cfg.server.listen_paths.clone();
    let state = AppState {
        cfg: Arc::new(cfg),
        client: make_client(),
        id_store: Arc::new(IdStore::default()),
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
    OriginalUri(_uri): OriginalUri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let cfg = state.cfg.clone();
    let mut body: Value = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };
    if !body.is_object() {
        return json_error(StatusCode::BAD_REQUEST, "body must be a JSON object");
    }

    let Some(url) = resolve_upstream_url(&cfg, &headers) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Responses-API-Base header is required (upstream mode=header_required)",
        );
    };

    if url_is_from_header(&cfg, &headers)
        && would_inject_authorization(&cfg, headers.get("authorization").is_some())
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "When overriding the upstream base (Responses-API-Base), the request must provide its own Authorization; the proxy will not send its configured credentials to an externally supplied URL.",
        );
    }

    let collision = cfg.cont.method == "tool_pair"
        && declares_continue_tool(&body, &cfg.cont.continue_tool_name);
    let should_fold = cfg.cont.enabled && truthy(body.get("stream")) && reasoning_enabled(&body) && !collision;

    if !should_fold {
        return passthrough(&state.client, &cfg, &headers, raw, &url).await;
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
        Ok(resp) => resp,
        Err(err) => return json_error(StatusCode::BAD_GATEWAY, &err),
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
        )),
    )
}

async fn passthrough(
    client: &reqwest::Client,
    cfg: &Config,
    headers: &HeaderMap,
    raw: Bytes,
    url: &str,
) -> Response {
    let upstream_headers = upstream_headers(headers, cfg);
    match open_passthrough(client, url, raw, &upstream_headers).await {
        Ok(resp) => {
            let status = status(resp.status);
            let content_type = resp.content_type.clone();
            let stream = resp.body.map(|r| r.map_err(|e| io::Error::new(io::ErrorKind::Other, e)));
            body_response(status, content_type, Body::from_stream(stream))
        }
        Err(err) => json_error(StatusCode::BAD_GATEWAY, &err),
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
