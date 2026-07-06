use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    io::Write,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use async_stream::stream;
use bytes::Bytes;
use futures_util::{stream as fstream, Stream, StreamExt};
use serde_json::{json, Map, Value};

use crate::{
    codex::{
        build_round_payload, commentary_message, continue_pair, is_truncation_pattern,
        reasoning_tokens, should_continue, tier_n,
    },
    config::Config,
    logging,
    sse::{incremental_sse, serialize_done, serialize_event, SseItem},
    store::IdStore,
};

pub type HeaderMapStr = HashMap<String, String>;
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;
pub type OpenFuture = Pin<Box<dyn Future<Output = Result<UpstreamResponse, String>> + Send>>;
pub type OpenNext = Box<dyn FnMut(Value) -> OpenFuture + Send>;

pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: BodyStream,
}

impl UpstreamResponse {
    pub fn from_bytes(data: impl Into<Bytes> + Send + 'static, status: u16) -> Self {
        Self {
            status,
            content_type: Some("text/event-stream".to_string()),
            body: Box::pin(fstream::once(async move { Ok(data.into()) })),
        }
    }
}

pub async fn open_round(
    client: &reqwest::Client,
    url: &str,
    payload: &Value,
    headers: &HeaderMapStr,
) -> Result<UpstreamResponse, String> {
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    send_body(client, url, body, headers).await
}

pub async fn open_passthrough(
    client: &reqwest::Client,
    url: &str,
    raw_body: Bytes,
    headers: &HeaderMapStr,
) -> Result<UpstreamResponse, String> {
    send_body(client, url, raw_body.to_vec(), headers).await
}

async fn send_body(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    headers: &HeaderMapStr,
) -> Result<UpstreamResponse, String> {
    let mut req = client.post(url).body(body);
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = Box::pin(resp.bytes_stream().map(|r| r.map_err(|e| e.without_url().to_string())));
    Ok(UpstreamResponse {
        status,
        content_type,
        body,
    })
}

pub async fn collect_body(mut resp: UpstreamResponse, limit: Option<usize>) -> Bytes {
    let mut out = Vec::new();
    while let Some(item) = resp.body.next().await {
        let Ok(chunk) = item else {
            break;
        };
        out.extend_from_slice(&chunk);
        if let Some(limit) = limit {
            if out.len() >= limit {
                out.truncate(limit);
                break;
            }
        }
    }
    Bytes::from(out)
}

pub fn fold_stream(
    client: reqwest::Client,
    cfg: Arc<Config>,
    base_body: Value,
    headers: HeaderMapStr,
    first_response: UpstreamResponse,
    id_store: Arc<IdStore>,
    url: String,
    request_id: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send {
    let opener: OpenNext = Box::new(move |payload| {
        let client = client.clone();
        let headers = headers.clone();
        let url = url.clone();
        Box::pin(async move { open_round(&client, &url, &payload, &headers).await })
    });
    fold_stream_with_opener(cfg, base_body, first_response, Some(id_store), opener, Some(request_id))
        .map(Ok::<Bytes, Infallible>)
}

pub fn fold_stream_with_opener(
    cfg: Arc<Config>,
    base_body: Value,
    first_response: UpstreamResponse,
    id_store: Option<Arc<IdStore>>,
    mut opener: OpenNext,
    request_id: Option<u64>,
) -> impl Stream<Item = Bytes> + Send {
    stream! {
        let rid = request_id.map(|id| format!("request_id={id} ")).unwrap_or_default();
        let cont = cfg.cont.clone();
        let orig_input = array_clone(base_body.get("input"));
        let mut seq = Seq::default();
        let mut ds_oi = 0_i64;
        let mut base_response: Option<Value> = None;
        let mut saw_done = false;
        let mut final_output: Vec<Value> = Vec::new();
        let mut total_usage = json!({});
        let mut first_usage: Option<Value> = None;
        let mut replay_tail: Vec<Value> = Vec::new();
        let mut rounds_info: Vec<Value> = Vec::new();
        let mut response = first_response;
        let mut round_no = 0_usize;

        loop {
            round_no += 1;
            let mut oi_map: HashMap<String, i64> = HashMap::new();
            let mut item_kind: HashMap<String, &'static str> = HashMap::new();
            let mut out_buffer: Vec<BufferEntry> = Vec::new();
            let mut round_reasoning: Vec<Value> = Vec::new();
            let mut terminal: Option<Value> = None;
            let mut usage: Option<Value> = None;
            let UpstreamResponse { body, .. } = response;
            let body = if cfg.log.dump_rounds_dir.is_empty() {
                body
            } else {
                let path = PathBuf::from(&cfg.log.dump_rounds_dir).join(format!("codex_mw_r{round_no}.sse.txt"));
                logging::debug(cfg.as_ref(), format!("{rid}round={round_no} dump={}", path.display()));
                tee_stream(body, path)
            };
            let events = incremental_sse(body);
            futures_util::pin_mut!(events);
            let mut stream_error = false;

            while let Some(next) = events.next().await {
                let item = match next {
                    Ok(item) => item,
                    Err(_) => {
                        stream_error = true;
                        break;
                    }
                };
                let SseItem::Event(mut ev) = item else {
                    saw_done = true;
                    continue;
                };
                let t = ev_type(&ev).to_string();

                if t == "response.created" || t == "response.in_progress" {
                    if round_no == 1 {
                        if t == "response.created" {
                            base_response = ev.get("response").cloned().or_else(|| Some(json!({})));
                        }
                        set_i64(&mut ev, "sequence_number", seq.next());
                        yield serialize_event(&ev);
                    }
                    continue;
                }

                if is_terminal(&t) {
                    usage = ev.get("response").and_then(|r| r.get("usage")).cloned();
                    terminal = Some(ev);
                    break;
                }

                let up_oi = output_key(&ev);
                if t == "response.output_item.added" {
                    let item = ev.get("item").cloned().unwrap_or_else(|| json!({}));
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        item_kind.insert(up_oi.clone(), "reasoning");
                        oi_map.insert(up_oi, ds_oi);
                        set_i64(&mut ev, "output_index", ds_oi);
                        ds_oi += 1;
                        set_i64(&mut ev, "sequence_number", seq.next());
                        yield serialize_event(&ev);
                    } else {
                        item_kind.insert(up_oi.clone(), "buffered");
                        out_buffer.push(BufferEntry {
                            oi: up_oi,
                            itype: item.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                            events: vec![ev],
                            item,
                        });
                    }
                    continue;
                }

                match item_kind.get(&up_oi).copied() {
                    Some("reasoning") => {
                        if let Some(mapped) = oi_map.get(&up_oi) {
                            set_i64(&mut ev, "output_index", *mapped);
                        }
                        set_i64(&mut ev, "sequence_number", seq.next());
                        if t == "response.output_item.done" {
                            let ritem = ev.get("item").cloned().unwrap_or_else(|| json!({}));
                            round_reasoning.push(ritem.clone());
                            final_output.push(ritem);
                        }
                        yield serialize_event(&ev);
                    }
                    Some("buffered") => {
                        if let Some(entry) = out_buffer.iter_mut().find(|entry| entry.oi == up_oi) {
                            if t == "response.output_item.done" {
                                if let Some(item) = ev.get("item") {
                                    entry.item = item.clone();
                                }
                            }
                            entry.events.push(ev);
                        }
                    }
                    _ => {
                        set_i64(&mut ev, "sequence_number", seq.next());
                        yield serialize_event(&ev);
                    }
                }
            }

            if stream_error {
                logging::error(cfg.as_ref(), format!("{rid}upstream_error round={round_no}"));
                yield serialize_event(&synthetic_incomplete(
                    base_response.as_ref(),
                    &final_output,
                    &agent_usage(first_usage.as_ref(), Some(&total_usage), None, false),
                    seq.next(),
                    "upstream_error",
                    &rounds_info,
                    Some(&total_usage),
                ));
                return;
            }

            let saw_terminal = terminal.is_some();
            sum_usage(&mut total_usage, usage.as_ref());
            if round_no == 1 {
                first_usage = usage.clone();
            }
            let rt = reasoning_tokens(usage.as_ref());
            let n = tier_n(rt, cont.truncation_step);
            rounds_info.push(json!({"round": round_no, "reasoning_tokens": rt, "n": n}));
            let has_enc = round_reasoning
                .last()
                .and_then(|r| r.get("encrypted_content"))
                .is_some_and(truthy);
            let within_caps = cont.max_total_output_tokens == 0
                || number(total_usage.get("output_tokens")) < cont.max_total_output_tokens;
            let do_continue = cont.enabled
                && saw_terminal
                && should_continue(rt, cont.min_n, cont.max_n, cont.truncation_step)
                && has_enc
                && round_no <= cont.max_continue
                && within_caps;
            logging::info(
                cfg.as_ref(),
                format!(
                    "{rid}round={round_no} reasoning_tokens={} n={} continue={do_continue}",
                    opt_i64(rt),
                    opt_i64(n)
                ),
            );

            let stopped_reason = if !do_continue && is_truncation_pattern(rt, cont.truncation_step) {
                Some(if !has_enc {
                    "no_encrypted_content"
                } else if round_no > cont.max_continue {
                    "max_continue"
                } else if !within_caps {
                    "max_total_output_tokens"
                } else {
                    "tier_out_of_window"
                })
            } else {
                None
            };

            if do_continue {
                logging::info(cfg.as_ref(), format!("{rid}continue next_round={}", round_no + 1));
                let last_id = round_reasoning
                    .last()
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let marker_items = if cont.method == "commentary" {
                    vec![commentary_message(&cont.marker_text)]
                } else {
                    if cont.repair_followup == "stateful" {
                        if let Some(store) = &id_store {
                            if !last_id.is_empty() {
                                store.add(&last_id);
                            }
                        }
                    }
                    let (call, output) = continue_pair(
                        &last_id,
                        &cont.continue_tool_name,
                        &cont.continue_output_text,
                    );
                    vec![call, output]
                };
                replay_tail.extend(round_reasoning.clone());
                replay_tail.extend(marker_items);

                if cont.method == "commentary" && cont.forward_marker {
                    let fwd_item = json!({
                        "id": format!("msg_continue_{round_no}"),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": cont.marker_text}],
                        "phase": "commentary"
                    });
                    for chunk in commentary_events(&fwd_item, ds_oi, &mut seq) {
                        yield chunk;
                    }
                    ds_oi += 1;
                    final_output.push(fwd_item);
                }

                let mut input_items = orig_input.clone();
                input_items.extend(replay_tail.clone());
                let payload = build_round_payload(
                    &base_body,
                    input_items,
                    cfg.stream.force_include_encrypted,
                    true,
                );
                match opener(payload).await {
                    Ok(next) if next.status < 400 => {
                        logging::info(
                            cfg.as_ref(),
                            format!("{rid}upstream status={} round={}", next.status, round_no + 1),
                        );
                        response = next;
                        continue;
                    }
                    Ok(next) => {
                        logging::error(
                            cfg.as_ref(),
                            format!("{rid}upstream_error status={} round={}", next.status, round_no + 1),
                        );
                        let _ = collect_body(next, Some(2000)).await;
                        yield serialize_event(&synthetic_incomplete(
                            base_response.as_ref(),
                            &final_output,
                            &agent_usage(first_usage.as_ref(), Some(&total_usage), usage.as_ref(), false),
                            seq.next(),
                            "upstream_error",
                            &rounds_info,
                            Some(&total_usage),
                        ));
                        return;
                    }
                    Err(err) => {
                        logging::error(
                            cfg.as_ref(),
                            format!("{rid}upstream open failed round={}: {err}", round_no + 1),
                        );
                        yield serialize_event(&synthetic_incomplete(
                            base_response.as_ref(),
                            &final_output,
                            &agent_usage(first_usage.as_ref(), Some(&total_usage), usage.as_ref(), false),
                            seq.next(),
                            "upstream_error",
                            &rounds_info,
                            Some(&total_usage),
                        ));
                        return;
                    }
                }
            }

            if !saw_terminal {
                logging::warn(cfg.as_ref(), format!("{rid}upstream_eof round={round_no}"));
                yield serialize_event(&synthetic_incomplete(
                    base_response.as_ref(),
                    &final_output,
                    &agent_usage(first_usage.as_ref(), Some(&total_usage), usage.as_ref(), false),
                    seq.next(),
                    "upstream_eof",
                    &rounds_info,
                    Some(&total_usage),
                ));
                return;
            }
            if let Some(reason) = stopped_reason {
                logging::info(cfg.as_ref(), format!("{rid}stopped reason={reason} round={round_no}"));
            }

            for entry in &out_buffer {
                for chunk in flush_entry(entry, ds_oi, &mut seq, &cfg) {
                    yield chunk;
                }
                ds_oi += 1;
                final_output.push(entry.item.clone());
            }
            yield serialize_event(&reconstruct_terminal(
                terminal.as_ref(),
                base_response.as_ref(),
                &final_output,
                &agent_usage(first_usage.as_ref(), Some(&total_usage), usage.as_ref(), true),
                seq.next(),
                &rounds_info,
                stopped_reason,
                Some(&total_usage),
            ));
            if saw_done {
                yield serialize_done();
            }
            return;
        }
    }
}

fn tee_stream(mut body: BodyStream, path: PathBuf) -> BodyStream {
    Box::pin(stream! {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = std::fs::File::create(path).ok();
        while let Some(item) = body.next().await {
            if let Ok(chunk) = &item {
                if let Some(file) = file.as_mut() {
                    let _ = file.write_all(chunk);
                }
            }
            yield item;
        }
    })
}

#[derive(Default)]
struct Seq {
    n: i64,
}

impl Seq {
    fn next(&mut self) -> i64 {
        let n = self.n;
        self.n += 1;
        n
    }
}

#[derive(Clone)]
struct BufferEntry {
    oi: String,
    itype: String,
    events: Vec<Value>,
    item: Value,
}

fn flush_entry(entry: &BufferEntry, ds_oi: i64, seq: &mut Seq, cfg: &Config) -> Vec<Bytes> {
    let rechunk = cfg.stream.rechunk_final_answer && entry.itype == "message";
    if !rechunk {
        return entry
            .events
            .iter()
            .cloned()
            .map(|mut ev| {
                if ev.get("output_index").is_some() {
                    set_i64(&mut ev, "output_index", ds_oi);
                }
                set_i64(&mut ev, "sequence_number", seq.next());
                serialize_event(&ev)
            })
            .collect();
    }

    let full_text: String = entry
        .events
        .iter()
        .filter(|ev| ev_type(ev) == "response.output_text.delta")
        .filter_map(|ev| ev.get("delta").and_then(Value::as_str))
        .collect();
    let mut emitted = false;
    let mut out = Vec::new();
    for ev in &entry.events {
        if ev_type(ev) == "response.output_text.delta" {
            if !emitted {
                let item_id = ev.get("item_id").cloned().unwrap_or(Value::Null);
                let content_index = ev.get("content_index").cloned().unwrap_or_else(|| json!(0));
                for delta in char_chunks(&full_text, cfg.stream.rechunk_size.max(1)) {
                    out.push(serialize_event(&json!({
                        "type": "response.output_text.delta",
                        "item_id": item_id.clone(),
                        "output_index": ds_oi,
                        "content_index": content_index.clone(),
                        "delta": delta,
                        "sequence_number": seq.next()
                    })));
                }
                emitted = true;
            }
            continue;
        }
        let mut ev = ev.clone();
        if ev.get("output_index").is_some() {
            set_i64(&mut ev, "output_index", ds_oi);
        }
        set_i64(&mut ev, "sequence_number", seq.next());
        out.push(serialize_event(&ev));
    }
    out
}

fn commentary_events(item: &Value, ds_oi: i64, seq: &mut Seq) -> Vec<Bytes> {
    let iid = item.get("id").cloned().unwrap_or(Value::Null);
    let text = item
        .get("content")
        .and_then(Value::as_array)
        .and_then(|xs| xs.first())
        .and_then(|x| x.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let head = json!({
        "id": iid.clone(),
        "type": "message",
        "role": "assistant",
        "phase": "commentary"
    });
    let mut evs = vec![
        json!({"type": "response.output_item.added", "output_index": ds_oi, "item": head}),
        json!({"type": "response.content_part.added", "output_index": ds_oi, "item_id": iid.clone(), "content_index": 0, "part": {"type": "output_text", "text": ""}}),
        json!({"type": "response.output_text.delta", "output_index": ds_oi, "item_id": iid.clone(), "content_index": 0, "delta": text}),
        json!({"type": "response.output_text.done", "output_index": ds_oi, "item_id": iid.clone(), "content_index": 0, "text": text}),
        json!({"type": "response.content_part.done", "output_index": ds_oi, "item_id": iid.clone(), "content_index": 0, "part": {"type": "output_text", "text": text}}),
        json!({"type": "response.output_item.done", "output_index": ds_oi, "item": item}),
    ];
    evs.iter_mut()
        .map(|ev| {
            set_i64(ev, "sequence_number", seq.next());
            serialize_event(ev)
        })
        .collect()
}

fn agent_usage(
    first: Option<&Value>,
    total: Option<&Value>,
    final_round: Option<&Value>,
    flushed_final: bool,
) -> Value {
    let in_tok = number(first.and_then(|u| u.get("input_tokens")));
    let cached = first
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64);
    let reasoning = number(
        total
            .and_then(|u| u.get("output_tokens_details"))
            .and_then(|d| d.get("reasoning_tokens")),
    );
    let final_nonreason = if flushed_final {
        let fo = number(final_round.and_then(|u| u.get("output_tokens")));
        let fr = number(
            final_round
                .and_then(|u| u.get("output_tokens_details"))
                .and_then(|d| d.get("reasoning_tokens")),
        );
        (fo - fr).max(0)
    } else {
        0
    };
    let out_tok = reasoning + final_nonreason;
    let mut usage = json!({
        "input_tokens": in_tok,
        "output_tokens": out_tok,
        "total_tokens": in_tok + out_tok,
        "output_tokens_details": {"reasoning_tokens": reasoning}
    });
    if let Some(cached) = cached {
        usage["input_tokens_details"] = json!({"cached_tokens": cached});
    }
    usage
}

fn reconstruct_terminal(
    terminal: Option<&Value>,
    base_response: Option<&Value>,
    output_items: &[Value],
    usage: &Value,
    seq: i64,
    rounds: &[Value],
    stopped_reason: Option<&str>,
    billed_usage: Option<&Value>,
) -> Value {
    let tresp = terminal
        .and_then(|t| t.get("response"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut resp = base_response.cloned().unwrap_or(tresp.clone());
    resp["output"] = Value::Array(output_items.to_vec());
    resp["usage"] = usage.clone();
    resp["status"] = tresp
        .get("status")
        .cloned()
        .unwrap_or_else(|| Value::String("completed".to_string()));
    if let Some(details) = tresp.get("incomplete_details") {
        resp["incomplete_details"] = details.clone();
    }
    with_proxy_metadata(&mut resp, rounds, stopped_reason, billed_usage);
    json!({
        "type": terminal.and_then(|t| t.get("type")).and_then(Value::as_str).unwrap_or("response.completed"),
        "response": resp,
        "sequence_number": seq
    })
}

fn synthetic_incomplete(
    base_response: Option<&Value>,
    output_items: &[Value],
    usage: &Value,
    seq: i64,
    reason: &str,
    rounds: &[Value],
    billed_usage: Option<&Value>,
) -> Value {
    let mut resp = base_response.cloned().unwrap_or_else(|| json!({}));
    resp["output"] = Value::Array(output_items.to_vec());
    resp["usage"] = usage.clone();
    resp["status"] = Value::String("incomplete".to_string());
    resp["incomplete_details"] = json!({"reason": reason});
    with_proxy_metadata(&mut resp, rounds, Some(reason), billed_usage);
    json!({"type": "response.incomplete", "response": resp, "sequence_number": seq})
}

fn with_proxy_metadata(
    resp: &mut Value,
    rounds: &[Value],
    stopped_reason: Option<&str>,
    billed_usage: Option<&Value>,
) {
    let mut md = resp
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    md.insert("proxy_rounds".to_string(), Value::Array(rounds.to_vec()));
    if let Some(usage) = billed_usage {
        if usage.as_object().is_some_and(|o| !o.is_empty()) {
            md.insert("proxy_billed_usage".to_string(), usage.clone());
        }
    }
    if let Some(reason) = stopped_reason {
        md.insert("proxy_stopped_reason".to_string(), Value::String(reason.to_string()));
    }
    resp["metadata"] = Value::Object(md);
}

fn sum_usage(acc: &mut Value, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    for key in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(v) = usage.get(key).and_then(Value::as_i64) {
            add_number(acc, key, v);
        }
    }
    if let Some(v) = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
    {
        add_nested(acc, "input_tokens_details", "cached_tokens", v);
    }
    if let Some(v) = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_i64)
    {
        add_nested(acc, "output_tokens_details", "reasoning_tokens", v);
    }
}

fn add_number(acc: &mut Value, key: &str, delta: i64) {
    let obj = ensure_object(acc);
    let next = obj.get(key).and_then(Value::as_i64).unwrap_or(0) + delta;
    obj.insert(key.to_string(), json!(next));
}

fn add_nested(acc: &mut Value, section: &str, key: &str, delta: i64) {
    let obj = ensure_object(acc);
    let sec = obj.entry(section.to_string()).or_insert_with(|| json!({}));
    let sec_obj = ensure_object(sec);
    let next = sec_obj.get(key).and_then(Value::as_i64).unwrap_or(0) + delta;
    sec_obj.insert(key.to_string(), json!(next));
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().unwrap()
}

fn char_chunks(s: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in s.chars() {
        buf.push(ch);
        if buf.chars().count() == size {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn array_clone(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn ev_type(ev: &Value) -> &str {
    ev.get("type").and_then(Value::as_str).unwrap_or("")
}

fn is_terminal(t: &str) -> bool {
    matches!(t, "response.completed" | "response.failed" | "response.incomplete")
}

fn output_key(ev: &Value) -> String {
    ev.get("output_index").map(Value::to_string).unwrap_or_default()
}

fn set_i64(ev: &mut Value, key: &str, value: i64) {
    if let Some(obj) = ev.as_object_mut() {
        obj.insert(key.to_string(), json!(value));
    }
}

fn number(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(xs) => !xs.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn opt_i64(v: Option<i64>) -> String {
    v.map_or_else(|| "none".to_string(), |v| v.to_string())
}
