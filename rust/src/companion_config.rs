//! 陪伴连接文件：只读取显式路径；凭据不参与 Serialize/Debug，也不写普通设置。
use super::{CompanionError, CompanionSettings, Protocol};
use reqwest::Url;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_KEY_BYTES: usize = 8192;
const MAX_CANDIDATES: usize = 200;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelCandidate {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub protocol: Protocol,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionInfo {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub protocol: Protocol,
    pub candidates: Vec<ModelCandidate>,
}

#[derive(Clone)]
pub struct Connection {
    pub(super) endpoint: Url,
    pub(super) key: String,
    pub(super) model: String,
    pub(super) protocol: Protocol,
    pub(super) route_field: &'static str,
    pub(super) route_value: String,
    pub(super) session_id: String,
    pub(super) username: String,
    pub(super) secrets: Vec<String>,
    pub(super) info: ConnectionInfo,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connection { credentials: [redacted] }")
    }
}

impl Connection {
    pub fn info(&self) -> &ConnectionInfo { &self.info }
}

fn text(value: Option<&Value>, required: bool, max: usize) -> Result<String, CompanionError> {
    let raw = match value {
        None if !required => "",
        Some(Value::String(s)) => s,
        _ => return Err(CompanionError::Config),
    };
    let s = raw.trim();
    if (required && s.is_empty()) || raw.chars().count() > max || raw.chars().any(char::is_control) {
        return Err(CompanionError::Config);
    }
    Ok(s.to_owned())
}

fn field<'a>(v: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| v.get(*name))
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, CompanionError> {
    let file = File::open(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound {
        CompanionError::Unconfigured
    } else { CompanionError::Config })?;
    if !file.metadata().map_err(|_| CompanionError::Config)?.is_file() {
        return Err(CompanionError::Config);
    }
    let mut bytes = Vec::new();
    file.take((max + 1) as u64).read_to_end(&mut bytes).map_err(|_| CompanionError::Config)?;
    if bytes.len() > max { return Err(CompanionError::Config); }
    Ok(bytes)
}

fn read_json(path: &Path) -> Result<Value, CompanionError> {
    let bytes = read_bounded(path, MAX_CONFIG_BYTES)?;
    let raw = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let data: Value = serde_json::from_slice(raw).map_err(|_| CompanionError::Config)?;
    if !data.is_object() { return Err(CompanionError::Config); }
    Ok(data)
}

fn read_key(value: &Value, path: &Path) -> Result<String, CompanionError> {
    let names = ["key", "api_key", "apiKey", "key_file", "api_key_file"];
    let present: Vec<_> = names.into_iter().filter(|name| value.get(*name).is_some()).collect();
    if present.len() != 1 { return Err(CompanionError::Config); }
    let name = present[0];
    let key = if name.ends_with("file") {
        let key_path = PathBuf::from(text(value.get(name), true, 4096)?);
        let key_path = if key_path.is_absolute() { key_path } else {
            path.parent().unwrap_or(Path::new(".")).join(key_path)
        };
        let bytes = read_bounded(&key_path, MAX_KEY_BYTES).map_err(|_| CompanionError::Config)?;
        std::str::from_utf8(&bytes).map_err(|_| CompanionError::Config)?
            .trim_start_matches('\u{feff}').trim().to_owned()
    } else {
        value.get(name).and_then(Value::as_str).ok_or(CompanionError::Config)?.to_owned()
    };
    // 不读取环境凭据，也不将未展开的环境引用误发给服务。
    if key.is_empty() || key.len() > MAX_KEY_BYTES || !key.bytes().all(|b| (0x21..=0x7e).contains(&b))
        || key.starts_with("{env:") || key.starts_with("${") {
        return Err(CompanionError::Config);
    }
    Ok(key)
}

pub(super) fn endpoint(raw: &str, protocol: Protocol) -> Result<Url, CompanionError> {
    if !raw.is_ascii() || raw.trim() != raw || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        || raw.contains(['\\', '?', '#']) {
        return Err(CompanionError::Config);
    }
    let (scheme, rest) = raw.split_once("://").ok_or(CompanionError::Config)?;
    if scheme != "https" && scheme != "http" { return Err(CompanionError::Config); }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains(['@', '%']) { return Err(CompanionError::Config); }
    // 在 URL 库规范化前检查原始主机，拒绝 127.1、十六进制和整数 IP 写法。
    let raw_host = if authority.starts_with('[') {
        authority.split_once(']').map(|(host, _)| &host[1..]).ok_or(CompanionError::Config)?
    } else { authority.split(':').next().unwrap_or("") };
    let mut url = Url::parse(raw).map_err(|_| CompanionError::Config)?;
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some()
        || url.port() == Some(0) || url.query().is_some() || url.fragment().is_some() {
        return Err(CompanionError::Config);
    }
    if scheme == "http" {
        if raw_host.eq_ignore_ascii_case("localhost") {
            url.set_host(Some("127.0.0.1")).map_err(|_| CompanionError::Config)?;
        } else if !raw_host.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false) {
            return Err(CompanionError::Config);
        }
    }
    let path = url.path().trim_end_matches('/');
    let suffix = match protocol {
        Protocol::AstrBot => "/api/v1/chat",
        Protocol::OpenAi => "/chat/completions",
        Protocol::Anthropic => "/messages",
        Protocol::Responses => "/responses",
    };
    let normalized = if path.ends_with(suffix) {
        path.to_owned()
    } else if protocol == Protocol::AstrBot {
        if path.ends_with("/api/v1") { format!("{path}/chat") } else { format!("{path}{suffix}") }
    } else {
        let mut base = path;
        for old in ["/chat/completions", "/responses", "/messages", "/models"] {
            if let Some(prefix) = base.strip_suffix(old) { base = prefix.trim_end_matches('/'); break; }
        }
        if base.is_empty() { format!("/v1{suffix}") } else { format!("{base}{suffix}") }
    };
    url.set_path(&normalized);
    Ok(url)
}

fn protocol(value: &Value) -> Result<Protocol, CompanionError> {
    let kind = text(field(value, &["protocol", "api_type", "type", "kind"]), false, 80)?;
    match kind.to_ascii_lowercase().as_str() {
        "" if value.get("config_id").is_some() || value.get("config_name").is_some() => Ok(Protocol::AstrBot),
        "" | "openai" | "openai-chat-completions" | "chat-completions" => Ok(Protocol::OpenAi),
        "anthropic" | "anthropic-messages" => Ok(Protocol::Anthropic),
        "response" | "responses" | "openai-response" | "openai-responses" => Ok(Protocol::Responses),
        "astrbot" | "astr-bot" => Ok(Protocol::AstrBot),
        _ => Err(CompanionError::Config),
    }
}

struct Record { id: String, name: String, value: Value }

fn records(data: &Value) -> Result<Vec<Record>, CompanionError> {
    let mut out = Vec::new();
    if let Some(rules) = data.pointer("/config/providerConfigRules/providerRules").and_then(Value::as_array) {
        for rule in rules.iter().take(MAX_CANDIDATES) {
            let cfg = &rule["config"];
            if cfg["api"]["baseUrl"].as_str().unwrap_or("").is_empty() { continue; }
            out.push(Record {
                id: text(rule.get("providerId"), true, 256)?,
                name: text(field(rule, &["providerName", "providerId"]), true, 256)?,
                value: json!({"base_url": cfg["api"]["baseUrl"], "protocol": cfg["api"]["type"].as_str().unwrap_or("openai"),
                    "api_key": cfg["access"]["apiKey"], "models": cfg["personalModelIds"]}),
            });
        }
    } else if let Some(providers) = data.get("providers") {
        if let Some(items) = providers.as_array() {
            for v in items.iter().take(MAX_CANDIDATES) {
                if v.get("enabled").and_then(Value::as_bool) == Some(false) { continue; }
                let id = text(field(v, &["id", "provider_id"]), true, 256)?;
                let name = text(field(v, &["name", "provider_name"]), false, 256)?;
                out.push(Record { name: if name.is_empty() { id.clone() } else { name }, id, value: v.clone() });
            }
        } else if let Some(items) = providers.as_object() {
            for (id, v) in items.iter().take(MAX_CANDIDATES) {
                if v.get("enabled").and_then(Value::as_bool) == Some(false) { continue; }
                let id = text(Some(&Value::String(id.clone())), true, 256)?;
                let name = text(field(v, &["name", "provider_name"]), false, 256)?;
                out.push(Record { name: if name.is_empty() { id.clone() } else { name }, id, value: v.clone() });
            }
        } else { return Err(CompanionError::Config); }
    } else if let Some(providers) = data.get("provider").and_then(Value::as_object) {
        for (id, v) in providers.iter().take(MAX_CANDIDATES) {
            if v.get("enabled").and_then(Value::as_bool) == Some(false) { continue; }
            let options = &v["options"];
            let Some(base) = field(options, &["baseURL", "baseUrl", "base_url"]) else { continue; };
            out.push(Record { id: text(Some(&Value::String(id.clone())), true, 256)?,
                name: text(field(v, &["name"]), false, 256)?.chars().take(256).collect(),
                value: json!({"base_url": base, "protocol": v["kind"].as_str().unwrap_or("openai"),
                    "api_key": options["apiKey"], "models": v["models"]}),
            });
        }
    } else {
        let id = text(field(data, &["provider_id", "id"]), false, 256)?;
        let id = if id.is_empty() { "companion".into() } else { id };
        let name = text(field(data, &["provider_name", "name", "persona_label"]), false, 256)?;
        out.push(Record { name: if name.is_empty() { id.clone() } else { name }, id, value: data.clone() });
    }
    if out.is_empty() { return Err(CompanionError::Config); }
    let mut ids = std::collections::HashSet::new();
    if out.iter().any(|r| !ids.insert(r.id.clone())) { return Err(CompanionError::Config); }
    Ok(out)
}

fn models(record: &Record) -> Result<Vec<String>, CompanionError> {
    let mut out = Vec::new();
    let selected = text(field(&record.value, &["model", "model_id", "expected_model"]), false, 256)?;
    if !selected.is_empty() { out.push(selected); }
    if let Some(value) = record.value.get("models") {
        if let Some(list) = value.as_array() {
            for item in list.iter().take(MAX_CANDIDATES) {
                let model = text(if item.is_string() { Some(item) } else { field(item, &["id", "model", "model_id"]) }, true, 256)?;
                if !out.contains(&model) { out.push(model); }
            }
        } else if let Some(map) = value.as_object() {
            for id in map.keys().take(MAX_CANDIDATES) {
                let model = text(Some(&Value::String(id.clone())), true, 256)?;
                if !out.contains(&model) { out.push(model); }
            }
        } else if !value.is_null() { return Err(CompanionError::Config); }
    }
    Ok(out)
}

/// 本地校验。只有显式配置文件会被打开；不会请求模型列表或读取工作会话。
pub fn load_connection(settings: &CompanionSettings) -> Result<Connection, CompanionError> {
    if settings.config_path.trim().is_empty() { return Err(CompanionError::Unconfigured); }
    let path = Path::new(&settings.config_path);
    let data = read_json(path)?;
    let records = records(&data)?;
    let default_id = text(field(&data, &["provider_id", "selected_provider", "default_provider"]), false, 256)?;
    let wanted = if settings.provider_id.is_empty() { &default_id } else { &settings.provider_id };
    let record = if wanted.is_empty() && records.len() == 1 { &records[0] } else {
        records.iter().find(|r| &r.id == wanted).ok_or(CompanionError::Selection)?
    };
    let p = protocol(&record.value)?;
    let known_models = models(record)?;
    let chosen = if settings.model_id.is_empty() {
        let direct = text(field(&record.value, &["model", "model_id", "expected_model"]), false, 256)?;
        if !direct.is_empty() { direct } else if known_models.len() == 1 { known_models[0].clone() } else { String::new() }
    } else { text(Some(&Value::String(settings.model_id.clone())), true, 256)? };
    if p != Protocol::AstrBot && chosen.is_empty() { return Err(CompanionError::Selection); }
    if !known_models.is_empty() && !chosen.is_empty() && !known_models.contains(&chosen) {
        return Err(CompanionError::Selection);
    }
    let base = field(&record.value, &["base_url", "baseUrl", "baseURL", "endpoint"])
        .and_then(Value::as_str).ok_or(CompanionError::Config)?;
    let url = endpoint(base, p)?;
    let key = read_key(&record.value, path)?;
    let route_id = text(record.value.get("config_id"), false, 256)?;
    let route_name = text(record.value.get("config_name"), false, 256)?;
    if p == Protocol::AstrBot && route_id.is_empty() && route_name.is_empty() { return Err(CompanionError::Config); }
    let session_id = match record.value.get("session_id") { Some(v) => text(Some(v), true, 256)?, None => "zcode-companion".into() };
    let username = match record.value.get("username") { Some(v) => text(Some(v), true, 256)?, None => "zcode-companion".into() };
    let mut candidates = candidates_from_records(&records);
    // 如服务回显凭据，显示数据与回复都不得含它。
    for item in &mut candidates {
        item.provider_id = item.provider_id.replace(&key, "[已隐藏]");
        item.provider_name = item.provider_name.replace(&key, "[已隐藏]");
        item.model_id = item.model_id.replace(&key, "[已隐藏]");
    }
    let info = ConnectionInfo { provider_id: record.id.replace(&key, "[已隐藏]"),
        provider_name: record.name.replace(&key, "[已隐藏]"), model_id: chosen.replace(&key, "[已隐藏]"), protocol: p, candidates };
    let mut secrets = vec![key.clone(), settings.config_path.clone(), base.to_owned(), url.to_string()];
    if let Some(host) = url.host_str() { secrets.push(host.to_owned()); }
    Ok(Connection { endpoint: url, key, model: chosen, protocol: p,
        route_field: if route_id.is_empty() { "config_name" } else { "config_id" },
        route_value: if route_id.is_empty() { route_name } else { route_id }, session_id, username, secrets, info })
}

fn candidates_from_records(records: &[Record]) -> Vec<ModelCandidate> {
    let mut out = Vec::new();
    for record in records {
        let Ok(p) = protocol(&record.value) else { continue; };
        let Ok(mut ids) = models(record) else { continue; };
        if p == Protocol::AstrBot && ids.is_empty() { ids.push(String::new()); }
        for id in ids {
            out.push(ModelCandidate { provider_id: record.id.clone(), provider_name: record.name.clone(), model_id: id, protocol: p });
            if out.len() >= MAX_CANDIDATES { return out; }
        }
    }
    out
}

/// 无网络的候选列表。不会展开 key_file 或环境变量。
pub fn connection_candidates(path: impl AsRef<Path>) -> Result<Vec<ModelCandidate>, CompanionError> {
    let data = read_json(path.as_ref())?;
    let records = records(&data)?;
    let mut result = candidates_from_records(&records);
    for record in &records {
        for name in ["key", "api_key", "apiKey"] {
            if let Some(key) = record.value.get(name).and_then(Value::as_str).filter(|k| !k.is_empty()) {
                for item in &mut result {
                    item.provider_id = item.provider_id.replace(key, "[已隐藏]");
                    item.provider_name = item.provider_name.replace(key, "[已隐藏]");
                    item.model_id = item.model_id.replace(key, "[已隐藏]");
                }
            }
        }
    }
    Ok(result)
}

/// 从 Python 普通设置迁移白名单字段；不复制原 JSON、凭据或提示词。
pub fn migrate_settings(value: &Value) -> CompanionSettings {
    if let Some(nested) = value.get("companion").filter(|v| v.is_object()) {
        return serde_json::from_value::<CompanionSettings>(nested.clone()).unwrap_or_default().normalized();
    }
    let mut s = CompanionSettings::default();
    s.enabled = value.get("companion_enabled").and_then(Value::as_bool).unwrap_or(false);
    s.auto = value.get("companion_auto").and_then(Value::as_bool).unwrap_or(true);
    s.paused = value.get("companion_paused").and_then(Value::as_bool).unwrap_or(false);
    s.config_path = value.get("companion_config").and_then(Value::as_str).unwrap_or("").to_owned();
    s.cooldown_seconds = value.get("companion_cooldown_s").and_then(Value::as_u64).unwrap_or(300);
    s.hourly_limit = value.get("companion_hourly_limit").and_then(Value::as_u64).unwrap_or(6).min(u32::MAX as u64) as u32;
    s.normalized()
}

/// 文件选择器返回路径后保存引用即可；旧 AstrBot 文件原样读取，无需复制密钥。
pub fn select_connection(settings: &CompanionSettings, path: impl AsRef<Path>) -> Result<CompanionSettings, CompanionError> {
    let mut next = settings.clone();
    next.config_path = path.as_ref().to_str().ok_or(CompanionError::Config)?.to_owned();
    next.provider_id.clear();
    next.model_id.clear();
    // 多 provider 文件可先选择路径，再由候选列表选择模型。
    connection_candidates(&next.config_path)?;
    Ok(next)
}
