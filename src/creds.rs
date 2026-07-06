use std::collections::HashMap;

use crate::config::Config;

const CLIENT_OWNED: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "accept-encoding",
];
const AUTH: &str = "authorization";
const ACCOUNT: &str = "chatgpt-account-id";
pub const RESPONSES_API_BASE: &str = "responses-api-base";

pub fn would_inject_authorization(cfg: &Config, agent_has_authorization: bool) -> bool {
    !cfg.auth.access_token.is_empty()
        && should_inject(&cfg.auth.mode, agent_has_authorization)
}

pub fn build_upstream_headers<I>(agent_headers: I, cfg: &Config) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = HashMap::new();
    for (name, value) in agent_headers {
        let lname = name.to_ascii_lowercase();
        if CLIENT_OWNED.contains(&lname.as_str()) || lname == RESPONSES_API_BASE {
            continue;
        }
        out.insert(name, value);
    }

    if would_inject_authorization(cfg, has(&out, AUTH)) {
        set(&mut out, "Authorization", format!("Bearer {}", cfg.auth.access_token));
    }
    if !cfg.auth.chatgpt_account_id.is_empty() && should_inject(&cfg.auth.mode, has(&out, ACCOUNT)) {
        set(
            &mut out,
            "chatgpt-account-id",
            cfg.auth.chatgpt_account_id.clone(),
        );
    }
    for (name, value) in &cfg.upstream.headers {
        set(&mut out, name, value.clone());
    }
    out
}

fn should_inject(mode: &str, header_present: bool) -> bool {
    match mode {
        "inject" => true,
        "passthrough_then_inject" => !header_present,
        _ => false,
    }
}

fn has(headers: &HashMap<String, String>, name: &str) -> bool {
    headers.keys().any(|k| k.eq_ignore_ascii_case(name))
}

fn set(headers: &mut HashMap<String, String>, name: &str, value: String) {
    headers.retain(|k, _| !k.eq_ignore_ascii_case(name));
    headers.insert(name.to_string(), value);
}
