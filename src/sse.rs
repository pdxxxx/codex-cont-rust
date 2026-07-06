use async_stream::stream;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

pub const DONE: &str = "[DONE]";

#[derive(Debug, Clone, PartialEq)]
pub enum SseItem {
    Event(Value),
    Done,
}

pub fn incremental_sse<S>(byte_iter: S) -> impl Stream<Item = Result<SseItem, String>>
where
    S: Stream<Item = Result<Bytes, String>> + Send + 'static,
{
    stream! {
        futures_util::pin_mut!(byte_iter);
        let mut buffer = Vec::<u8>::new();
        let mut data_lines = Vec::<String>::new();

        while let Some(chunk) = byte_iter.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = buffer.drain(..=pos).collect();
                let line = decode_line(&raw[..raw.len() - 1]);
                if line.is_empty() {
                    if let Some(item) = flush_event(&mut data_lines) {
                        yield Ok(item);
                    }
                    continue;
                }
                if line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
            }
        }
        if let Some(item) = flush_event(&mut data_lines) {
            yield Ok(item);
        }
    }
}

fn decode_line(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim_end_matches('\r').to_string()
}

fn flush_event(data_lines: &mut Vec<String>) -> Option<SseItem> {
    if data_lines.is_empty() {
        return None;
    }
    let payload = data_lines.join("\n");
    data_lines.clear();
    if payload == DONE {
        return Some(SseItem::Done);
    }
    serde_json::from_str::<Value>(&payload).ok().map(SseItem::Event)
}

pub fn serialize_event(event: &Value) -> Bytes {
    let etype = event.get("type").and_then(Value::as_str).unwrap_or("message");
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("event: {etype}\ndata: {data}\n\n"))
}

pub fn serialize_done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}
