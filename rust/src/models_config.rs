use super::*;
use serde_json::{json, Map};
use std::collections::HashSet;
use std::fs;
use std::io::Read;

const MAX_CONFIG: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ConfigPaths {
    pub v2_config: Option<PathBuf>,
    pub personal_config: Option<PathBuf>,
    pub catalog: Option<PathBuf>,
    pub backup_dir: PathBuf,
}
impl ConfigPaths {
    pub fn from_env() -> Result<Self, String> {
        Self::resolve(|k| std::env::var(k).ok(), running_install_dirs())
    }
    /// 显式环境读取器便于完全隔离测试，不修改进程环境。
    pub fn resolve(get: impl Fn(&str) -> Option<String>, installs: Vec<PathBuf>) -> Result<Self, String> {
        let env = |key: &str| get(key).filter(|s| !s.trim().is_empty());
        let user_home = env("USERPROFILE").or_else(|| env("HOME")).map(PathBuf::from);
        let expand = |s: &str| -> PathBuf {
            if s == "~" { return user_home.clone().unwrap_or_else(|| PathBuf::from(s)); }
            if let Some(tail) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
                if let Some(home) = &user_home { return home.join(tail); }
            }
            PathBuf::from(s.trim())
        };
        let mut bases = Vec::new();
        if let Some(v) = env("ZCODE_DATA_BASE_DIR") { bases.push(expand(&v).join(".zcode")); }
        if let Some(v) = env("ZCODE_HOME") { bases.push(expand(&v)); }
        for key in ["ZCODE_CLI_HOME", "CLI_HOME"] {
            if let Some(v) = env(key) {
                let p = expand(&v);
                if p.file_name().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("cli")) {
                    if let Some(parent) = p.parent() { bases.push(parent.to_owned()); }
                }
                bases.push(p);
            }
        }
        if let Some(home) = &user_home { bases.push(home.join(".zcode")); }
        if let Some(v) = env("APPDATA") { bases.push(expand(&v).join(".zcode")); }
        dedup_paths(&mut bases);
        let mut active = Vec::new();
        for base in &bases {
            // setting.json 只用于定位；不会从该文件取任何认证信息。
            if let Ok(Some(doc)) = read_optional(&base.join("v2/setting.json")) {
                if let Some(v) = doc.value.get("dataBaseDir").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    active.push(expand(v).join(".zcode"));
                }
            }
        }
        active.extend(bases); dedup_paths(&mut active);
        let explicit = env("ZCODE_CONFIG_FILE").or_else(|| env("ZCODE_V2_CONFIG_FILE"));
        let v2_config = if let Some(path) = explicit { Some(expand(&path)) }
            else { active.iter().map(|p| p.join("v2/config.json")).find(|p| p.try_exists().unwrap_or(true)) };
        let selected_root = v2_config.as_ref().and_then(|p| p.parent()).and_then(|p| p.parent()).map(Path::to_owned);
        let personal_config = env("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE").map(|v| expand(&v))
            .or_else(|| v2_config.as_ref().and_then(|p| p.parent()).map(|p| p.join("provider_config.json")))
            .or_else(|| active.iter().map(|p| p.join("v2/provider_config.json")).find(|p| p.try_exists().unwrap_or(true)));
        let mut catalogs = Vec::new();
        if let Some(v) = env("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE") { catalogs.push(expand(&v)); }
        if let Some(root) = &selected_root { catalogs.extend(runtime_catalogs(root)); }
        for root in &active { catalogs.extend(runtime_catalogs(root)); }
        let mut install_dirs = installs;
        for key in ["ZCODE_PATH", "ZCODE_HOME"] {
            if let Some(v) = env(key) { install_dirs.push(expand(&v)); }
        }
        dedup_paths(&mut install_dirs);
        for root in &install_dirs { catalogs.push(root.join("resources/config/provider/zcode-builtin.json")); }
        for root in &install_dirs {
            for dir in [root.join("resources/model-providers"), root.join("model-providers")] {
                let mut old: Vec<_> = entries(&dir).into_iter().filter(|p| p.file_name().is_some_and(|s| {
                    let s = s.to_string_lossy(); s.starts_with("models_catalog") && s.ends_with(".json")
                })).collect();
                old.sort(); catalogs.extend(old);
            }
        }
        dedup_paths(&mut catalogs);
        // 显式指定或当前目录存在但损坏时不偷偷读旧目录规则。
        let catalog = if env("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE").is_some() { catalogs.first().cloned() }
            else { catalogs.into_iter().find(|p| p.is_file()) };
        let backup_dir = env("ZCODE_MODEL_BACKUP_DIR").map(|p| expand(&p)).unwrap_or_else(|| {
            personal_config.as_ref().or(v2_config.as_ref()).and_then(|p| p.parent()).map(|p| p.join("model-import-backups"))
                .or_else(|| selected_root.map(|p| p.join("v2/model-import-backups")))
                .unwrap_or_else(|| std::env::temp_dir().join("zcode-model-import-backups"))
        });
        Ok(Self { v2_config, personal_config, catalog, backup_dir })
    }
}
fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(path_key(p)));
}
pub(super) fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { key.to_lowercase() } else { key }
}
fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).ok().into_iter().flatten().take(4096).filter_map(|e| e.ok().map(|e| e.path())).collect()
}
fn version_key(path: &Path) -> Vec<u64> {
    path.ancestors().filter_map(|p| p.file_name()).filter_map(|s| {
        let v = s.to_string_lossy();
        let v = v.split('-').next().unwrap_or("");
        if !v.contains('.') { return None; }
        v.split('.').map(str::parse::<u64>).collect::<Result<Vec<_>, _>>().ok()
    }).next().unwrap_or_default()
}
fn runtime_catalogs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for platform in entries(&root.join("v2/runtime/provider")) {
        for version in entries(&platform) {
            for endpoint in entries(&version) {
                if endpoint.file_name().is_some_and(|s| s.to_string_lossy().starts_with("endpoint-")) {
                    let file = endpoint.join("zcode-builtin.json"); if file.is_file() { out.push(file); }
                }
            }
        }
    }
    out.sort_by(|a, b| version_key(b).cmp(&version_key(a)).then_with(|| a.cmp(b))); out
}
#[cfg(not(windows))]
fn running_install_dirs() -> Vec<PathBuf> { Vec::new() }
#[cfg(windows)]
fn running_install_dirs() -> Vec<PathBuf> {
    // 只枚举可执行文件路径，不读取任何进程内存/环境/凭据。
    #[link(name = "kernel32")]
    extern "system" {
        fn K32EnumProcesses(ids: *mut u32, size: u32, needed: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn QueryFullProcessImageNameW(h: *mut std::ffi::c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
        fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    }
    let mut ids = [0u32; 4096]; let mut needed = 0; let mut dirs = Vec::new();
    unsafe {
        if K32EnumProcesses(ids.as_mut_ptr(), std::mem::size_of_val(&ids) as u32, &mut needed) == 0 { return dirs; }
        for pid in ids.iter().take((needed as usize / 4).min(ids.len())) {
            let h = OpenProcess(0x1000, 0, *pid); if h.is_null() { continue; }
            let mut buf = [0u16; 32768]; let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len); CloseHandle(h);
            if ok == 0 { continue; }
            let p = PathBuf::from(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]));
            if p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("zcode.exe")) {
                if let Some(parent) = p.parent() { dirs.push(parent.to_owned()); if let Some(p) = parent.parent() { dirs.push(p.to_owned()); } }
            }
        }
    }
    dedup_paths(&mut dirs); dirs
}

// 私有文档/record 不实现 Debug/Serialize，避免错误日志泄漏认证。
pub(super) struct Document { pub path: PathBuf, pub raw: Vec<u8>, pub value: Value }
struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("JSON with unique object keys") }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<UniqueJson, E> { Ok(UniqueJson(json!(v))) }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<UniqueJson, E> { Ok(UniqueJson(json!(v))) }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<UniqueJson, E> { Ok(UniqueJson(json!(v))) }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<UniqueJson, E> {
                serde_json::Number::from_f64(v).map(|n| UniqueJson(Value::Number(n))).ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<UniqueJson, E> { Ok(UniqueJson(json!(v))) }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<UniqueJson, E> { Ok(UniqueJson(Value::String(v))) }
            fn visit_unit<E: serde::de::Error>(self) -> Result<UniqueJson, E> { Ok(UniqueJson(Value::Null)) }
            fn visit_none<E: serde::de::Error>(self) -> Result<UniqueJson, E> { Ok(UniqueJson(Value::Null)) }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<UniqueJson, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<UniqueJson>()? { values.push(v.0); }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<UniqueJson, A::Error> {
                let mut values = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) { return Err(serde::de::Error::custom("duplicate JSON key")); }
                    values.insert(key, map.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
pub(super) fn read_optional(path: &Path) -> Result<Option<Document>, String> {
    let file = match fs::File::open(path) {
        Ok(f) => f, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法读取当前 ZCode 配置，未尝试旧配置或旧凭据。".into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG + 1).read_to_end(&mut bytes).map_err(|_| "无法读取当前 ZCode 配置。".to_string())?;
    if bytes.len() as u64 > MAX_CONFIG { return Err("配置文件超过 16 MiB 限制。".into()); }
    let raw = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let value = serde_json::from_slice::<UniqueJson>(raw).map_err(|_| "当前 ZCode 配置不是有效 JSON 或包含重复字段，未尝试旧配置或旧凭据。".to_string())?.0;
    if !value.is_object() { return Err("当前 ZCode 配置根节点必须是对象。".into()); }
    Ok(Some(Document { path: path.to_owned(), raw: bytes, value }))
}
pub(super) fn optional_doc(path: Option<&PathBuf>) -> Result<Option<Document>, String> {
    path.map(|p| read_optional(p)).transpose().map(Option::flatten)
}
pub(super) fn require_object<'a>(v: &'a Value, message: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object().ok_or_else(|| message.to_owned())
}
pub(super) fn personal_rules(value: &Value) -> Result<Vec<Value>, String> {
    if let Some(version) = value.get("schemaVersion") {
        if version.as_u64() != Some(1) { return Err("个人 provider 配置 schemaVersion 不受支持。".into()); }
    }
    let cfg = value.get("config").ok_or("个人 provider 配置缺少 config 对象。")?;
    require_object(cfg, "个人 provider 的 config 必须是对象。")?;
    let Some(pcr) = cfg.get("providerConfigRules") else { return Ok(Vec::new()); };
    require_object(pcr, "providerConfigRules 必须是对象。")?;
    let Some(rules) = pcr.get("providerRules") else { return Ok(Vec::new()); };
    let rules = rules.as_array().ok_or("providerRules 必须是数组。")?;
    let mut seen = HashSet::new();
    for rule in rules {
        require_object(rule, "providerRules 条目必须是对象。")?;
        let pid = string(&rule["providerId"]);
        if !valid_id(pid, &[]) || !seen.insert(pid) { return Err("providerRules 包含无效或重复 provider ID。".into()); }
        require_object(&rule["config"], "provider 规则的 config 必须是对象。")?;
    }
    Ok(rules.clone())
}

pub(super) struct ProviderRecord {
    pub info: ProviderInfo, pub options: Value, pub models: Value, pub local_ids: Vec<String>,
    pub identity: Value, pub account_only: bool,
}
impl ProviderRecord {
    pub fn secrets(&self) -> Vec<String> { collect_secrets(&self.options) }
}
pub(super) fn normalized_provider(value: &str) -> String {
    let v = value.to_lowercase(); v.strip_prefix("builtin:").or_else(|| v.strip_prefix("account:")).unwrap_or(&v).to_owned()
}
pub(super) fn valid_id(id: &str, secrets: &[String]) -> bool {
    !id.trim().is_empty() && id == id.trim() && id.chars().count() <= 256 &&
        !id.chars().any(|c| c.is_control() || matches!(c, '\u{2028}'|'\u{2029}'|'\u{200b}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')) &&
        !secrets.iter().any(|s| !s.is_empty() && id.contains(s))
}
pub(super) fn safe_identifier(id: &str) -> String {
    if valid_id(id, &[]) { id.to_owned() } else { "[无效 ID]".into() }
}
fn safe_text(value: &str, secrets: &[String]) -> String {
    let mut text = value.to_owned();
    for secret in secrets { if !secret.is_empty() { text = text.replace(secret, "[已隐藏]"); } }
    text.chars().filter(|c| !c.is_control()).take(256).collect::<String>().trim().to_owned()
}
fn merged_headers(containers: &[&Value]) -> Value {
    let mut out = Map::new();
    for v in containers {
        if let Some(h) = v.as_object() {
            for (k, v) in h { out.insert(k.to_ascii_lowercase(), v.clone()); }
        }
    }
    Value::Object(out)
}
fn options_for(cfg: &Value, fallback: &Value) -> Value {
    let mut opts = object(&fallback["options"]);
    if let Some(o) = cfg.get("options").and_then(Value::as_object) { opts.extend(o.clone()); }
    let api = &cfg["api"]; let access = &cfg["access"];
    if let Some(base) = api.get("baseUrl").or_else(|| api.get("baseURL")) { opts.insert("baseURL".into(), base.clone()); }
    // 新运行时 access.credential 是权威值；旧 apiKey 只在 credential 未声明时使用。
    let key = access.get("credential").or_else(|| access.get("apiKey"))
        .or_else(|| cfg.get("apiKey")).or_else(|| opts.get("apiKey")).or_else(|| fallback.get("apiKey"));
    let key = key.cloned().unwrap_or(Value::Null);
    let env_key = access.get("apiKeyEnv").or_else(|| access.get("env"))
        .or_else(|| opts.get("apiKeyEnv"));
    let key = if key.is_null() { env_key.map(|v| json!({"env": v})).unwrap_or(key) } else { key };
    opts.insert("apiKey".into(), key);
    if let Some(required) = access.get("apiKeyRequired").or_else(|| api.get("apiKeyRequired")) {
        opts.insert("apiKeyRequired".into(), required.clone());
    }
    let old_headers = opts.get("headers").cloned().unwrap_or(Value::Null);
    opts.insert("headers".into(), merged_headers(&[&fallback["headers"], &old_headers, &access["headers"], &api["headers"]]));
    Value::Object(opts)
}
fn record(pid: &str, cfg: &Value, name: &str, kind: &str, fallback: &Value) -> Result<ProviderRecord, String> {
    let mut options = options_for(cfg, fallback);
    let secrets = collect_secrets(&options);
    // 一次任务冻结环境引用，避免多个模型在不同凭据下测试；写入前 identity 会重新解析比较。
    if let Ok(key) = credential(&options["apiKey"]) { options["apiKey"] = json!(key); }
    if let Some(headers) = options.get_mut("headers").and_then(Value::as_object_mut) {
        for (name, value) in headers {
            if matches!(name.as_str(), "authorization" | "x-api-key" | "openai-organization" | "openai-project" | "anthropic-version") {
                if let Ok(resolved) = credential(value) { *value = json!(resolved); }
            }
        }
    }
    let identity = json!({"config":cfg, "fallback":fallback, "resolvedOptions":options});
    if !valid_id(pid, &secrets) { return Err("provider ID 无效或含认证内容，已拒绝显示。".into()); }
    let mut models = object(&fallback["models"]);
    if let Some(m) = cfg.get("models").and_then(Value::as_object) { models.extend(m.clone()); }
    let mut ids = Vec::new();
    for key in ["personalModelIds", "builtinModelIds"] {
        if let Some(list) = cfg.get(key).and_then(Value::as_array) {
            ids.extend(list.iter().filter_map(Value::as_str).filter(|s| valid_id(s, &secrets)).map(str::to_owned));
        }
    }
    ids.extend(models.keys().filter(|s| valid_id(s, &secrets)).cloned());
    let mut seen = HashSet::new(); ids.retain(|s| seen.insert(s.clone()));
    models.retain(|s, _| valid_id(s, &secrets));
    let name = safe_text(name, &secrets); let name = if name.is_empty() { pid.into() } else { name };
    let kind = safe_text(kind, &secrets);
    let account_only = string(&options["baseURL"]).is_empty() && string(&options["baseUrl"]).is_empty() ||
        string(&cfg["access"]["type"]) == "zhipu-account";
    Ok(ProviderRecord { info: ProviderInfo { id: pid.into(), name, kind }, options,
        models: Value::Object(models), local_ids: ids,
        identity, account_only })
}
fn catalog_document(paths: &ConfigPaths) -> Option<Value> {
    // catalog 不是认证来源；损坏时能力保持未知，不混旧目录。
    paths.catalog.as_ref().and_then(|p| read_optional(p).ok().flatten()).map(|d| d.value)
}
pub(super) fn load_records(paths: &ConfigPaths) -> Result<Vec<ProviderRecord>, String> {
    let v2 = optional_doc(paths.v2_config.as_ref())?;
    let personal = optional_doc(paths.personal_config.as_ref())?;
    let catalog = catalog_document(paths).unwrap_or(Value::Null);
    let mut existing = Map::new();
    if let Some(doc) = &v2 {
        if let Some(providers) = doc.value.get("provider") {
            existing = require_object(providers, "当前配置的 provider 必须是对象，未读取旧凭据。")?.clone();
        }
    }
    let catalog_rules = catalog.pointer("/config/providerConfigRules/providerRules").and_then(Value::as_array).cloned().unwrap_or_default();
    let lookup_catalog = |pid: &str| -> Value {
        catalog_rules.iter().find(|r| normalized_provider(string(&r["providerId"])) == normalized_provider(pid))
            .map(|r| r["config"].clone()).unwrap_or_else(|| json!({}))
    };
    let mut out = Vec::new(); let mut seen = HashSet::new();
    if let Some(doc) = &personal {
        for r in personal_rules(&doc.value)? {
            let pid = string(&r["providerId"]); let cfg = &r["config"];
            seen.insert(pid.to_owned());
            if cfg.get("enabled") == Some(&Value::Bool(false)) || cfg.get("visibility") == Some(&json!("hidden")) { continue; }
            let fallback = existing.get(pid).cloned().unwrap_or_else(|| json!({}));
            if fallback.get("enabled") == Some(&Value::Bool(false)) { continue; }
            let mut resolved = lookup_catalog(pid);
            metadata::deep_merge(&mut resolved, cfg);
            let kind = cfg.pointer("/api/type").and_then(Value::as_str)
                .or_else(|| fallback.get("kind").and_then(Value::as_str)).unwrap_or("openai-chat-completions");
            let name = r.get("providerName").and_then(Value::as_str)
                .or_else(|| fallback.get("name").and_then(Value::as_str)).unwrap_or(pid);
            out.push(record(pid, &resolved, name, kind, &fallback)?);
        }
    }
    for (pid, p) in &existing {
        if seen.contains(pid) { continue; }
        require_object(p, "provider 条目必须是对象。")?;
        if p.get("enabled") == Some(&Value::Bool(false)) { continue; }
        if let Some(models) = p.get("models") { require_object(models, "provider.models 必须是对象。")?; }
        let catalog_cfg = lookup_catalog(pid);
        let cfg = if p.get("api").is_some() || p.get("access").is_some() { p.clone() } else { json!({}) };
        // 账号地址来自 catalog 时不据此猜账号认证。保留已有 options 独立认证可发现。
        let mut rec = record(pid, &cfg, p.get("name").and_then(Value::as_str).unwrap_or(pid),
            p.get("kind").and_then(Value::as_str).unwrap_or("unknown"), p)?;
        if rec.local_ids.is_empty() {
            if let Some(list) = catalog_cfg.get("builtinModelIds").and_then(Value::as_array) {
                let secrets = rec.secrets();
                rec.local_ids.extend(list.iter().filter_map(Value::as_str).filter(|s| valid_id(s, &secrets)).map(str::to_owned));
            }
        }
        out.push(rec);
    }
    Ok(out)
}
pub(super) fn find_record(paths: &ConfigPaths, pid: &str) -> Result<ProviderRecord, String> {
    load_records(paths)?.into_iter().find(|p| p.info.id == pid).ok_or_else(|| "所选 provider 已移除或停用，请刷新 provider 列表。".into())
}
pub(super) fn rules_for(paths: &ConfigPaths) -> Result<Value, String> {
    let mut rules = catalog_document(paths).map(|v| metadata::rules_from_catalog(&v)).unwrap_or_else(|| json!({}));
    if let Some(doc) = optional_doc(paths.personal_config.as_ref())? {
        if let Some(mcr) = doc.value.pointer("/config/modelConfigRules") {
            require_object(mcr, "modelConfigRules 必须是对象。")?;
            if let Some(arr) = mcr.get("providerModelRules") {
                let arr = arr.as_array().ok_or("providerModelRules 必须是数组。")?;
                let obj = rules.as_object_mut().ok_or("模型规则必须是对象。")?;
                let merged = obj.entry("builtinProviderModelRules").or_insert_with(|| json!([]));
                let merged = merged.as_array_mut().ok_or("内置模型规则必须是数组。")?;
                merged.extend(arr.iter().cloned());
            }
        }
    }
    Ok(rules)
}

/// 只接受明确的 key/env 表示，不拿账号 OAuth/access token 冒充 API key。
pub(super) fn credential(value: &Value) -> Result<String, String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(v) => {
            if v.chars().any(char::is_control) || v.len() > 16 * 1024 { return Err("认证配置包含无效字符或长度超限。".into()); }
            let v = v.trim();
            let env_name = v.strip_prefix("{env:").and_then(|s| s.strip_suffix('}'))
                .or_else(|| v.strip_prefix("${").and_then(|s| s.strip_suffix('}')));
            if let Some(name) = env_name {
                if !valid_env_name(name) { return Err("认证环境变量引用格式无效。".into()); }
                return checked_secret(std::env::var(name).ok().filter(|s| !s.trim().is_empty()).ok_or("认证所引用的环境变量未设置。")?);
            }
            if v.starts_with("{env:") || v.starts_with("${") { return Err("认证环境变量引用格式无效。".into()); }
            checked_secret(v.into())
        }
        Value::Object(o) => {
            let kind = o.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "oauth" | "oauth2" | "account" | "zhipu-account" | "keychain") {
                return Err("账号认证需由 ZCode 管理；没有可独立使用的 API Key。".into());
            }
            if let Some(env) = o.get("env").or_else(|| o.get("envVar"))
                .or_else(|| if matches!(kind, "env" | "environment") { o.get("name").or_else(|| o.get("variable")) } else { None }) {
                let name = env.as_str().filter(|s| valid_env_name(s)).ok_or("认证环境变量引用格式无效。")?;
                return credential(&json!(format!("{{env:{name}}}")));
            }
            if let Some(v) = o.get("apiKey").or_else(|| o.get("value")).or_else(|| o.get("key")) {
                if v.is_object() { return Err("认证配置格式不支持。".into()); }
                return credential(v);
            }
            Err("认证配置格式不支持；请在 ZCode 中配置 API Key。".into())
        }
        _ => Err("认证配置格式不支持；请在 ZCode 中配置 API Key。".into()),
    }
}
fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && name.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
}
fn checked_secret(value: String) -> Result<String, String> {
    if value.chars().any(char::is_control) || value.len() > 16 * 1024 { Err("认证配置包含无效字符或长度超限。".into()) } else { Ok(value) }
}
fn collect_secrets(options: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut values = vec![&options["apiKey"]];
    if let Some(h) = options["headers"].as_object() {
        values.extend(h.iter().filter(|(k, _)| matches!(k.to_ascii_lowercase().as_str(), "authorization" | "x-api-key")).map(|(_, v)| v));
    }
    for v in values {
        if let Ok(secret) = credential(v) {
            if !secret.is_empty() {
                if let Some((scheme, token)) = secret.split_once(' ') {
                    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() { out.push(token.to_owned()); }
                }
                out.push(secret);
            }
        }
        // 格式错误时也不允许把原始串作为名称/模型 ID 回显。
        if let Some(raw) = v.as_str().filter(|s| !s.is_empty()) { out.push(raw.to_owned()); }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.len())); out.dedup(); out
}
