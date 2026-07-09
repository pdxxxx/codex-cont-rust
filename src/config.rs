use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer};

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    pub listen_paths: Vec<String>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8787,
            listen_paths: vec![
                "/backend-api/codex/responses".to_string(),
                "/v1/responses".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct UpstreamCfg {
    pub url: String,
    pub mode: String,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: HashMap<String, String>,
}

impl Default for UpstreamCfg {
    fn default() -> Self {
        Self {
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            mode: "fixed".to_string(),
            headers: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AuthCfg {
    pub mode: String,
    pub access_token: String,
    pub chatgpt_account_id: String,
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self {
            mode: "passthrough_then_inject".to_string(),
            access_token: String::new(),
            chatgpt_account_id: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ContinueCfg {
    pub enabled: bool,
    pub truncation_step: i64,
    pub max_continue: usize,
    pub min_n: i64,
    pub max_n: i64,
    pub method: String,
    pub marker_text: String,
    pub forward_marker: bool,
    pub continue_tool_name: String,
    pub continue_output_text: String,
    pub repair_followup: String,
    pub max_total_output_tokens: i64,
}

impl Default for ContinueCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            truncation_step: 518,
            max_continue: 8,
            min_n: 1,
            max_n: 0,
            method: "commentary".to_string(),
            marker_text: "Continue thinking...".to_string(),
            forward_marker: false,
            continue_tool_name: "continue_thinking".to_string(),
            continue_output_text: "Please continue thinking about the query.".to_string(),
            repair_followup: "off".to_string(),
            max_total_output_tokens: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct StreamCfg {
    pub force_include_encrypted: bool,
    pub rechunk_final_answer: bool,
    pub rechunk_size: usize,
}

impl Default for StreamCfg {
    fn default() -> Self {
        Self {
            force_include_encrypted: true,
            rechunk_final_answer: true,
            rechunk_size: 8,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LogCfg {
    pub level: String,
    pub dump_rounds_dir: String,
}

impl Default for LogCfg {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            dump_rounds_dir: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub upstream: UpstreamCfg,
    pub auth: AuthCfg,
    #[serde(rename = "continue")]
    pub cont: ContinueCfg,
    pub stream: StreamCfg,
    pub log: LogCfg,
    #[serde(skip)]
    pub root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            upstream: UpstreamCfg::default(),
            auth: AuthCfg::default(),
            cont: ContinueCfg::default(),
            stream: StreamCfg::default(),
            log: LogCfg::default(),
            root: PathBuf::from("."),
        }
    }
}

pub fn config_path_for_exe(exe: impl AsRef<Path>) -> PathBuf {
    exe.as_ref()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml")
}

pub fn default_config_path() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map(config_path_for_exe)
        .map_err(|e| format!("failed to locate current executable: {e}"))
}

pub fn load_config(path: impl Into<PathBuf>) -> Result<Config, String> {
    let path = path.into();
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut cfg = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        toml::from_str::<Config>(&text).map_err(|e| e.to_string())?
    } else {
        Config::default()
    };
    cfg.root = root;
    Ok(cfg)
}

fn deserialize_headers<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = HashMap::<String, toml::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let value = match v {
                toml::Value::String(s) => s,
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                toml::Value::Datetime(d) => d.to_string(),
                toml::Value::Array(_) | toml::Value::Table(_) => v.to_string(),
            };
            (k, value)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{config_path_for_exe, load_config};
    use std::path::PathBuf;

    #[test]
    fn config_path_uses_exe_directory() {
        let exe = PathBuf::from("bin").join("codex-cont.exe");
        assert_eq!(
            config_path_for_exe(&exe),
            PathBuf::from("bin").join("config.toml")
        );
    }

    #[test]
    fn missing_config_root_is_config_directory() {
        let path = PathBuf::from("missing-bin").join("config.toml");
        let cfg = load_config(path).unwrap();
        assert_eq!(cfg.root, PathBuf::from("missing-bin"));
    }
}
