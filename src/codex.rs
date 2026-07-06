use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::store::IdStore;

pub const DEFAULT_TRUNCATION_STEP: i64 = 518;
pub const ENCRYPTED_INCLUDE: &str = "reasoning.encrypted_content";

pub fn is_truncation_pattern(tokens: Option<i64>, step: i64) -> bool {
    tokens.is_some_and(|t| t >= step - 2 && (t + 2) % step == 0)
}

pub fn tier_n(tokens: Option<i64>, step: i64) -> Option<i64> {
    let tokens = tokens.filter(|t| is_truncation_pattern(Some(*t), step))?;
    Some((tokens + 2) / step)
}

pub fn should_continue(tokens: Option<i64>, min_n: i64, max_n: i64, step: i64) -> bool {
    let Some(n) = tier_n(tokens, step) else {
        return false;
    };
    n >= min_n && (max_n == 0 || n <= max_n)
}

pub fn reasoning_tokens(usage: Option<&Value>) -> Option<i64> {
    usage?
        .get("output_tokens_details")?
        .get("reasoning_tokens")?
        .as_i64()
}

pub fn continue_call_id(reasoning_id: &str) -> String {
    let mut h = Sha1::new();
    h.update(reasoning_id.as_bytes());
    let digest = h.finalize();
    let hex = hex_lower(&digest[..]);
    format!("call_{}", &hex[..24])
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

pub fn continue_pair(reasoning_id: &str, tool_name: &str, output_text: &str) -> (Value, Value) {
    let call_id = continue_call_id(reasoning_id);
    (
        json!({
            "type": "function_call",
            "call_id": call_id,
            "name": tool_name,
            "arguments": "{\"continue\": true}"
        }),
        json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output_text
        }),
    )
}

pub fn commentary_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
        "phase": "commentary"
    })
}

pub fn merge_include(include: Option<&Value>, force_encrypted: bool) -> Vec<Value> {
    let mut items = match include {
        Some(Value::Array(xs)) => xs
            .iter()
            .map(|x| {
                x.as_str()
                    .map(|s| Value::String(s.to_string()))
                    .unwrap_or_else(|| Value::String(x.to_string()))
            })
            .collect(),
        _ => Vec::new(),
    };
    if force_encrypted && !items.iter().any(|x| x.as_str() == Some(ENCRYPTED_INCLUDE)) {
        items.push(Value::String(ENCRYPTED_INCLUDE.to_string()));
    }
    items
}

pub fn build_round_payload(
    base_body: &Value,
    input_items: Vec<Value>,
    force_include_encrypted: bool,
    drop_previous_response_id: bool,
) -> Value {
    let mut body = base_body.as_object().cloned().unwrap_or_default();
    body.insert("stream".to_string(), Value::Bool(true));
    body.insert("input".to_string(), Value::Array(input_items));
    if force_include_encrypted || base_body.get("include").is_some() {
        body.insert(
            "include".to_string(),
            Value::Array(merge_include(base_body.get("include"), force_include_encrypted)),
        );
    }
    if drop_previous_response_id {
        body.remove("previous_response_id");
    }
    Value::Object(body)
}

pub fn declares_continue_tool(body: &Value, tool_name: &str) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
        })
}

pub fn reasoning_enabled(body: &Value) -> bool {
    body.get("reasoning") != Some(&Value::Bool(false))
}

pub fn repair_followup_input(
    input_items: Vec<Value>,
    id_store: &IdStore,
    tool_name: &str,
    output_text: &str,
) -> Vec<Value> {
    let mut out = Vec::with_capacity(input_items.len());
    for (i, item) in input_items.iter().enumerate() {
        out.push(item.clone());
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let Some(rid) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !id_store.contains(rid) {
            continue;
        }
        let call_id = continue_call_id(rid);
        let already = input_items.get(i + 1).is_some_and(|next| {
            next.get("type").and_then(Value::as_str) == Some("function_call")
                && next.get("call_id").and_then(Value::as_str) == Some(call_id.as_str())
        });
        if !already {
            let (call, output) = continue_pair(rid, tool_name, output_text);
            out.push(call);
            out.push(output);
        }
    }
    out
}
