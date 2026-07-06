use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use codex_cont::{
    app::{create_router, resolve_upstream_url, url_is_from_header},
    codex::{
        continue_call_id, is_truncation_pattern, reasoning_enabled, repair_followup_input,
        should_continue, tier_n,
    },
    config::Config,
    creds::{build_upstream_headers, would_inject_authorization},
    proxy::{fold_stream_with_opener, OpenNext, UpstreamResponse},
    sse::{incremental_sse, SseItem},
    store::IdStore,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

const R1: &[u8] = include_bytes!("fixtures/codex_poc_r1.sse.txt");
const R2: &[u8] = include_bytes!("fixtures/codex_poc_r2.sse.txt");

fn make_sse(events: Vec<Value>) -> Bytes {
    let mut out = Vec::new();
    for ev in events {
        out.extend_from_slice(format!("event: {}\r\n", typ(&ev)).as_bytes());
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(serde_json::to_string(&ev).unwrap().as_bytes());
        out.extend_from_slice(b"\r\n\r\n");
    }
    Bytes::from(out)
}

async fn parse_events(data: Bytes) -> Vec<Value> {
    let events = incremental_sse(stream::once(async move { Ok(data) }));
    futures_util::pin_mut!(events);
    let mut out = Vec::new();
    while let Some(item) = events.next().await {
        if let SseItem::Event(ev) = item.unwrap() {
            out.push(ev);
        }
    }
    out
}

async fn collect_fold(
    cfg: Config,
    base_body: Value,
    first: UpstreamResponse,
    later: Vec<UpstreamResponse>,
) -> (Vec<Value>, Vec<Value>) {
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let responses = Arc::new(Mutex::new(VecDeque::from(later)));
    let opener_payloads = payloads.clone();
    let opener_responses = responses.clone();
    let opener: OpenNext = Box::new(move |payload| {
        opener_payloads.lock().unwrap().push(payload);
        let resp = opener_responses.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { Ok(resp) })
    });
    let folded = fold_stream_with_opener(
        Arc::new(cfg),
        base_body,
        first,
        Some(Arc::new(IdStore::default())),
        opener,
        None,
    );
    futures_util::pin_mut!(folded);
    let mut out = Vec::new();
    while let Some(chunk) = folded.next().await {
        out.extend_from_slice(&chunk);
    }
    let events = parse_events(Bytes::from(out)).await;
    let payloads = payloads.lock().unwrap().clone();
    (events, payloads)
}

fn round(rs_id: &str, enc: &str, reasoning_tokens: i64, msg: Option<&str>) -> Bytes {
    let mut evs = vec![
        json!({"type":"response.created","response":{"id":"resp_x","status":"in_progress","model":"gpt-5.5","metadata":{}}}),
        json!({"type":"response.in_progress","response":{"id":"resp_x"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":rs_id,"type":"reasoning"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":rs_id,"type":"reasoning","encrypted_content":enc}}),
    ];
    if let Some(msg) = msg {
        evs.extend([
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_x","type":"message"}}),
            json!({"type":"response.output_text.delta","output_index":1,"item_id":"msg_x","content_index":0,"delta":msg}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_x","type":"message","content":[{"type":"output_text","text":msg}]}}),
        ]);
    }
    evs.push(json!({"type":"response.completed","response":{"id":"resp_x","status":"completed","usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150,"output_tokens_details":{"reasoning_tokens":reasoning_tokens}}}}));
    make_sse(evs)
}

fn typ(ev: &Value) -> &str {
    ev.get("type").and_then(Value::as_str).unwrap_or("")
}

#[tokio::test]
async fn large_invalid_json_reaches_handler() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, create_router(Config::default())).await.unwrap();
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .body(vec![b'{'; 2 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn truncation_headers_url_and_repair() {
    assert!(is_truncation_pattern(Some(516), 518));
    assert_eq!(tier_n(Some(1034), 518), Some(2));
    assert!(should_continue(Some(516), 1, 0, 518));
    assert!(!should_continue(Some(2588), 1, 3, 518));
    assert!(reasoning_enabled(&json!({"reasoning": {"effort": "high"}})));
    assert!(!reasoning_enabled(&json!({"reasoning": false})));

    let cfg = Config::default();
    let out = build_upstream_headers(
        [
            ("Authorization".to_string(), "Bearer agent".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Host".to_string(), "drop.me".to_string()),
            ("Responses-API-Base".to_string(), "https://x/v1".to_string()),
            ("X-Custom".to_string(), "keep".to_string()),
        ],
        &cfg,
    );
    let low = out
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(low.get("content-type").unwrap(), "application/json");
    assert_eq!(low.get("x-custom").unwrap(), "keep");
    assert!(!low.contains_key("host"));
    assert!(!low.contains_key("responses-api-base"));

    let mut cfg2 = Config::default();
    cfg2.upstream.mode = "header".to_string();
    cfg2.upstream.url = "https://cfg/responses".to_string();
    let mut headers = HeaderMap::new();
    headers.insert("responses-api-base", "https://override/v1".parse().unwrap());
    assert_eq!(
        resolve_upstream_url(&cfg2, &headers).unwrap(),
        "https://override/v1/responses"
    );
    assert!(url_is_from_header(&cfg2, &headers));
    cfg2.auth.mode = "passthrough_then_inject".to_string();
    cfg2.auth.access_token = "TOK".to_string();
    assert!(would_inject_authorization(&cfg2, false));
    assert!(!would_inject_authorization(&cfg2, true));

    let store = IdStore::default();
    store.add("rs_keep");
    let repaired = repair_followup_input(
        vec![
            json!({"type":"reasoning","id":"rs_keep"}),
            json!({"type":"message","id":"msg"}),
        ],
        &store,
        "continue_thinking",
        "go",
    );
    let call_id = continue_call_id("rs_keep");
    assert_eq!(repaired[1]["type"], "function_call");
    assert_eq!(repaired[1]["call_id"].as_str(), Some(call_id.as_str()));
    assert_eq!(repaired[2]["type"], "function_call_output");
}

#[tokio::test]
async fn sse_chunking_and_real_two_round_fold() {
    let whole = parse_events(Bytes::from_static(R1)).await;
    let chunks = stream::iter(
        R1.chunks(7)
            .map(|chunk| Ok::<_, String>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>(),
    );
    let events = incremental_sse(chunks);
    futures_util::pin_mut!(events);
    let mut chunked = Vec::new();
    while let Some(item) = events.next().await {
        if let SseItem::Event(ev) = item.unwrap() {
            chunked.push(ev);
        }
    }
    assert_eq!(whole.iter().map(typ).collect::<Vec<_>>(), chunked.iter().map(typ).collect::<Vec<_>>());

    let mut cfg = Config::default();
    cfg.cont.max_continue = 1;
    let (evs, _) = collect_fold(
        cfg,
        json!({"model":"gpt-5.5","input":[{"role":"user","content":"q"}]}),
        UpstreamResponse::from_bytes(Bytes::from_static(R1), 200),
        vec![UpstreamResponse::from_bytes(Bytes::from_static(R2), 200)],
    )
    .await;

    assert_eq!(evs.iter().filter(|e| typ(e) == "response.created").count(), 1);
    assert_eq!(evs.iter().filter(|e| typ(e) == "response.in_progress").count(), 1);
    assert_eq!(evs.iter().filter(|e| typ(e) == "response.completed").count(), 1);
    assert_eq!(
        evs.iter()
            .map(|e| e["sequence_number"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        (0..evs.len() as i64).collect::<Vec<_>>()
    );

    let deltas = evs
        .iter()
        .filter(|e| typ(e) == "response.output_text.delta")
        .filter_map(|e| e.get("delta").and_then(Value::as_str))
        .collect::<String>();
    assert!(deltas.contains("答案是") || deltas.contains("21"));
    assert!(!deltas.contains("最少需要取出"));

    let completed = evs.last().unwrap();
    let usage = &completed["response"]["usage"];
    assert_eq!(usage["input_tokens"], 4582);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 3840);
    assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 516 + 2588);
    assert_eq!(completed["response"]["metadata"]["proxy_stopped_reason"], "max_continue");
    assert_eq!(
        completed["response"]["metadata"]["proxy_billed_usage"]["input_tokens"],
        4582 + 5140
    );
}

#[tokio::test]
async fn commentary_payload_and_forward_marker() {
    let cfg = Config::default();
    let (evs, payloads) = collect_fold(
        cfg.clone(),
        json!({"model":"gpt-5.5","input":[{"role":"user","content":"q"}]}),
        UpstreamResponse::from_bytes(round("rs_a", "ENC_A", 516, Some("trunc")), 200),
        vec![UpstreamResponse::from_bytes(round("rs_b", "ENC_B", 999, Some("done")), 200)],
    )
    .await;
    let input = payloads[0]["input"].as_array().unwrap();
    let last = input.last().unwrap();
    assert_eq!(last["type"], "message");
    assert_eq!(last["phase"], "commentary");
    assert!(input.iter().any(|x| x["type"] == "reasoning" && x.get("encrypted_content").is_some()));
    assert!(!input.iter().any(|x| x["type"] == "function_call"));
    assert!(!evs.iter().any(|e| e["item"]["phase"] == "commentary"));

    let mut cfg = Config::default();
    cfg.cont.forward_marker = true;
    let (evs, _) = collect_fold(
        cfg,
        json!({"model":"gpt-5.5","input":[{"role":"user","content":"q"}]}),
        UpstreamResponse::from_bytes(round("rs_a", "ENC_A", 516, Some("trunc")), 200),
        vec![UpstreamResponse::from_bytes(round("rs_b", "ENC_B", 999, Some("done")), 200)],
    )
    .await;
    assert!(evs.iter().any(|e| e["item"]["phase"] == "commentary"));
    assert!(evs
        .last()
        .unwrap()["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["phase"] == "commentary"));
}

#[tokio::test]
async fn eof_returns_incomplete_without_tentative_message() {
    let events = vec![
        json!({"type":"response.created","response":{"id":"resp_e","status":"in_progress"}}),
        json!({"type":"response.in_progress","response":{"id":"resp_e"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_e","type":"reasoning"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_e","type":"reasoning","encrypted_content":"E"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_e","type":"message"}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"msg_e","content_index":0,"delta":"partial"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_e","type":"message"}}),
    ];
    let (evs, _) = collect_fold(
        Config::default(),
        json!({"model":"gpt-5.5","input":[{"role":"user","content":"q"}]}),
        UpstreamResponse::from_bytes(make_sse(events), 200),
        vec![],
    )
    .await;
    let terminal = evs.last().unwrap();
    assert_eq!(typ(terminal), "response.incomplete");
    assert_eq!(terminal["response"]["incomplete_details"]["reason"], "upstream_eof");
    assert!(!evs.iter().any(|e| typ(e) == "response.output_text.delta"));
    assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 1);
    assert_eq!(terminal["response"]["output"][0]["type"], "reasoning");
}
