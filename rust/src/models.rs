//! 模型发现 / 导入 / 校验：口径与 Python model_discovery.py 对齐。
//!
//! 两条配置来源（与 Python 版同构）：
//! * personal：`~/.zcode/v2/provider_config.json` 的 providerRules
//!   （自定义 API provider：有 baseUrl/apiKey，走 HTTP 发现）
//! * builtin/account：`~/.zcode/v2/config.json` 的 provider 字典
//!   （账号类 provider：没有独立地址，发现走 provider.models 本地清单）
//!
//! 导入的「热写入」契约（2026-11-15 与 Python 版同步定稿）：客户端每秒
//! 轮询 provider_config.json，兼容写入立即生效；写完必须
//! `verify_import_applied` 读回校验，撞上客户端写回覆盖才降级。
//!
//! Rust 与 Python 的差异集中在错误处理：Python 抛 DiscoveryError，
//! 这里统一返回 `Result<_, String>`（消息文本与 Python 版一致，直接上 UI）。

use serde_json::{json, Value};
use std::path::PathBuf;

/// 账号类 provider 的发现回退上限（Python MAX_MODELS = 200）。
const MAX_MODELS: usize = 200;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
}

fn v2_config_path() -> PathBuf {
    home().join(".zcode").join("v2").join("config.json")
}

fn personal_path() -> PathBuf {
    // 与 Python 版一致：环境变量可覆盖（测试/便携用）。
    std::env::var("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".zcode").join("v2").join("provider_config.json"))
}

fn obj<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

fn text(v: &Value, default: &str) -> String {
    v.as_str().map(|s| s.to_string()).unwrap_or_else(|| default.to_string())
}

// ── provider 枚举 ───────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// 全部可发现的 provider：personal（自定义 API）排前，账号类随后。
pub fn configured_providers() -> Vec<ProviderInfo> {
    let mut out: Vec<ProviderInfo> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Ok(data) = std::fs::read_to_string(personal_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            let rules = obj(obj(obj(&v, "config"), "providerConfigRules"), "providerRules")
                .as_array().cloned().unwrap_or_default();
            for rule in rules {
                let pid = text(obj(&rule, "providerId"), "");
                let cfg = obj(&rule, "config").clone();
                let api = obj(&cfg, "api").clone();
                if pid.is_empty() || text(obj(&api, "baseUrl"), "").is_empty() {
                    continue;
                }
                if seen.insert(pid.clone()) {
                    out.push(ProviderInfo {
                        name: {
                            let n = text(obj(&rule, "providerName"), &pid);
                            if n.is_empty() { pid.clone() } else { n }
                        },
                        kind: text(obj(&api, "type"), "openai-chat-completions"),
                        id: pid,
                    });
                }
            }
        }
    }

    if let Ok(data) = std::fs::read_to_string(v2_config_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            if let Some(providers) = obj(&v, "provider").as_object() {
                for (pid, p) in providers {
                    if p.get("enabled") == Some(&json!(false)) || !seen.insert(pid.clone()) {
                        continue;
                    }
                    out.push(ProviderInfo {
                        id: pid.clone(),
                        name: text(obj(p, "name"), pid),
                        kind: text(obj(p, "kind"), "unknown"),
                    });
                }
            }
        }
    }
    out
}

/// 发现用完整 provider 记录（含 options/models，认证信息只在内存）。
fn provider_record(pid: &str) -> Option<Value> {
    // personal 优先。
    if let Ok(data) = std::fs::read_to_string(personal_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            let rules = obj(obj(obj(&v, "config"), "providerConfigRules"), "providerRules")
                .as_array().cloned().unwrap_or_default();
            for rule in rules {
                if text(obj(&rule, "providerId"), "") == pid {
                    let cfg = obj(&rule, "config").clone();
                    let api = obj(&cfg, "api").clone();
                    let base = text(obj(&api, "baseUrl"), "");
                    if base.is_empty() {
                        continue;
                    }
                    let ids: Vec<String> = obj(&cfg, "personalModelIds")
                        .as_array().map(|a| {
                            a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                        }).unwrap_or_default();
                    let models: Value = ids.into_iter()
                        .map(|i| (i, json!({}))).collect::<serde_json::Map<String, Value>>().into();
                    return Some(json!({
                        "name": text(obj(&rule, "providerName"), pid),
                        "kind": text(obj(&api, "type"), "openai-chat-completions"),
                        "options": {
                            "baseURL": base,
                            "apiKey": obj(obj(&cfg, "access"), "apiKey"),
                        },
                        "models": models,
                    }));
                }
            }
        }
    }
    // 账号类。
    if let Ok(data) = std::fs::read_to_string(v2_config_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            if let Some(p) = obj(&v, "provider").get(pid) {
                if p.get("enabled") != Some(&json!(false)) {
                    return Some(p.clone());
                }
            }
        }
    }
    None
}

// ── 发现 ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelRow {
    pub provider: String,
    pub provider_id: String,
    pub id: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Discovery {
    pub provider: String,
    pub provider_id: String,
    pub rows: Vec<ModelRow>,
    pub source_note: String,
}

/// base URL 清洗：去尾斜杠、剥掉误配的 /models 等端点尾巴（Python _base_url 同规则）。
fn base_url(options: &Value) -> Result<String, String> {
    let raw = text(obj(options, "baseURL"), "").trim().to_string();
    let raw = if raw.is_empty() { text(obj(options, "baseUrl"), "") } else { raw };
    let raw = raw.trim().trim_end_matches('/').to_string();
    if raw.is_empty() {
        return Err("未配置 API 地址。".into());
    }
    if !raw.starts_with("https://") && !raw.starts_with("http://") {
        return Err("API 地址必须是有效的 HTTP(S) 地址。".into());
    }
    let stripped = raw
        .trim_end_matches(|c: char| c == '/')
        .to_string();
    // 简化版：剥 chat/completions|models 尾巴（Python 正则同义）。
    for tail in ["/chat/completions", "/completions", "/responses", "/messages", "/models"] {
        if stripped.to_ascii_lowercase().ends_with(tail) {
            return Ok(stripped[..stripped.len() - tail.len()].trim_end_matches('/').to_string());
        }
    }
    Ok(stripped)
}

/// 认证解析（Python _credential 同款）：支持 {env:NAME} / ${NAME} 引用环境变量。
fn credential(raw: &str) -> String {
    let t = raw.trim();
    if let Some(inner) = t.strip_prefix("{env:").and_then(|x| x.strip_suffix('}')) {
        return std::env::var(inner).unwrap_or_default();
    }
    if let Some(inner) = t.strip_prefix("${").and_then(|x| x.strip_suffix('}')) {
        return std::env::var(inner).unwrap_or_default();
    }
    t.to_string()
}

/// GET {base}/models；404/405 返回 Ok(None) 让调用方走本地回退。
fn http_list_models(url: &str, headers: &[(String, String)]) -> Result<Option<Vec<String>>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
    let mut req = client.get(url).header("Accept", "application/json").header("User-Agent", "zcode-token-bar-rs/1");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().map_err(|e| {
        // reqwest 的连接错误统一成 Python 版口吻
        let msg = e.to_string();
        if msg.contains("timed out") { "请求超时（20s）。".into() } else { format!("网络错误：{msg}") }
    })?;
    match resp.status().as_u16() {
        404 | 405 => return Ok(None),
        401 => return Err("HTTP 401：认证失败。检查 API Key（支持 {env:变量名} 引用），或 Key 是否配在自定义 headers 里。".into()),
        s if !(200..300).contains(&s) => return Err(format!("HTTP {s}。")),
        _ => {}
    }
    let v: Value = resp.json().map_err(|e| format!("响应不是 JSON：{e}"))?;
    let mut ids = Vec::new();
    if let Some(arr) = obj(&v, "data").as_array() {
        for item in arr {
            if let Some(id) = obj(item, "id").as_str() {
                if !id.trim().is_empty() {
                    ids.push(id.trim().to_string());
                }
            }
        }
    }
    ids.truncate(MAX_MODELS);
    Ok(Some(ids))
}

/// 单个 provider 的发现。认证头构造照 Python _headers（openai→Bearer，
/// anthropic→x-api-key + anthropic-version）。
pub fn discover_provider(pid: &str) -> Result<Discovery, String> {
    let record = provider_record(pid).ok_or_else(|| "provider 已移除或停用。".to_string())?;
    let name = text(obj(&record, "name"), pid);
    let kind = text(obj(&record, "kind"), "").to_ascii_lowercase();
    let options = obj(&record, "options").clone();

    let base = base_url(&options)?;
    let (url, api_type) = if kind.contains("anthropic") {
        let suffix = if base.ends_with("/v1") { "/models" } else { "/v1/models" };
        (format!("{base}{suffix}"), "anthropic")
    } else if kind.is_empty() || kind.contains("openai") {
        let suffix = if base[8..].contains('/') { "/models" } else { "/v1/models" };
        (format!("{base}{suffix}"), "openai")
    } else {
        return Err("该 provider 协议暂不支持模型发现（支持 OpenAI 与 Anthropic 兼容接口）。".into());
    };

    let key_raw = {
        let a = text(obj(&options, "apiKey"), "");
        if a.is_empty() { text(obj(&record, "apiKey"), "") } else { a }
    };
    let key = credential(&key_raw);
    if key_raw.contains("{env:") && key.is_empty() {
        return Err("认证所引用的环境变量未设置。".into());
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    if api_type == "anthropic" {
        headers.push(("anthropic-version".into(), "2023-06-01".into()));
        if !key.is_empty() {
            headers.push(("x-api-key".into(), key.clone()));
        }
    } else if !key.is_empty() {
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    // 转交用户自定义头（白名单同 Python 版：只认认证与组织选择，不允许改 Host 等）。
    let allowed = ["authorization", "x-api-key", "openai-organization", "openai-project", "anthropic-version"];
    if let Some(hs) = obj(&options, "headers").as_object() {
        for (k, v) in hs {
            let kl = k.to_ascii_lowercase();
            if allowed.contains(&kl.as_str()) {
                if let Some(val) = v.as_str() {
                    // 白名单头不覆盖上面已设置的默认（用户显式声明优先，但同名跳过重复 push）
                    if !headers.iter().any(|(hk, _)| hk.eq_ignore_ascii_case(&kl)) {
                        headers.push((k.clone(), val.to_string()));
                    }
                }
            }
        }
    }

    let ids = match http_list_models(&url, &headers)? {
        Some(ids) => ids,
        None => {
            // 本地回退：provider.models 键名清单（账号类 provider 唯一路径）。
            let local: Vec<String> = obj(&record, "models")
                .as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            if local.is_empty() {
                return Err("端点无模型列表接口且本地没有模型清单。".into());
            }
            return Ok(Discovery {
                provider: name.clone(),
                provider_id: pid.to_string(),
                rows: local.into_iter().take(MAX_MODELS).map(|id| ModelRow {
                    provider: name.clone(),
                    provider_id: pid.to_string(),
                    id,
                }).collect(),
                source_note: "该端点不提供模型列表接口；清单来自 ZCode 配置。".into(),
            });
        }
    };

    Ok(Discovery {
        provider: name.clone(),
        provider_id: pid.to_string(),
        rows: ids.into_iter().map(|id| ModelRow {
            provider: name.clone(),
            provider_id: pid.to_string(),
            id,
        }).collect(),
        source_note: String::new(),
    })
}

// ── 探针：逐模型小请求实测可用性 ────────────────────────────────

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProbeOutcome {
    pub model_id: String,
    pub usable: Option<bool>, // Some(true)=可用 Some(false)=明确不可用 None=未知（超时/5xx）
    pub error: String,
}

/// 单模型探针：1-token 对话请求，2xx 即可用。Python probe_model 的简化版
/// （tools/image/pdf 细项探测后续补，先用可用性支撑「只导入能用到的」）。
pub fn probe_model(pid: &str, model_id: &str) -> ProbeOutcome {
    let mut out = ProbeOutcome { model_id: model_id.to_string(), usable: None, error: String::new() };
    let Some(record) = provider_record(pid) else {
        out.error = "provider 已移除。".into();
        return out;
    };
    let options = obj(&record, "options").clone();
    let Ok(base) = base_url(&options) else {
        // 账号类 provider 没有独立端点：无法实测，视作未知（照常导入）
        return out;
    };
    let kind = text(obj(&record, "kind"), "").to_ascii_lowercase();
    let key_raw = {
        let a = text(obj(&options, "apiKey"), "");
        if a.is_empty() { text(obj(&record, "apiKey"), "") } else { a }
    };
    let key = credential(&key_raw);

    let (url, body, headers) = if kind.contains("anthropic") {
        let suffix = if base.ends_with("/v1") { "/messages" } else { "/v1/messages" };
        let mut hs = vec![("anthropic-version".to_string(), "2023-06-01".to_string())];
        if !key.is_empty() {
            hs.push(("x-api-key".into(), key.clone()));
        }
        (format!("{base}{suffix}"), json!({"model": model_id, "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}), hs)
    } else {
        let mut hs = Vec::new();
        if !key.is_empty() {
            hs.push(("Authorization".into(), format!("Bearer {key}")));
        }
        (format!("{base}/chat/completions"), json!({"model": model_id, "max_tokens": 1, "stream": false, "messages": [{"role": "user", "content": "hi"}]}), hs)
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => { out.error = e.to_string(); return out; }
    };
    let mut req = client.post(&url).json(&body);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.send() {
        Ok(resp) => {
            let st = resp.status().as_u16();
            if (200..300).contains(&st) {
                out.usable = Some(true);
            } else if (400..500).contains(&st) {
                out.usable = Some(false);
                out.error = format!("HTTP {st}");
            } else {
                out.error = format!("HTTP {st}");
            }
        }
        Err(e) => {
            out.error = if e.to_string().contains("timed out") { "超时".into() } else { e.to_string() };
        }
    }
    out
}

/// 批量探针：逐个顺序请求（每模型最多 15s，避免并发打爆端点）。
pub fn probe_models(pid: &str, ids: &[String], progress: impl Fn(usize, usize)) -> Vec<ProbeOutcome> {
    let mut out = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        out.push(probe_model(pid, id));
        progress(i + 1, ids.len());
    }
    out
}

// ── 导入（热写入 + 校验） ───────────────────────────────────────────

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ImportResult {
    pub added: usize,
    pub skipped: Vec<String>,
    pub missing: Vec<String>,
    pub added_ids: Vec<(String, String)>,
    pub backup: String,
    pub error: String,
}

/// 把发现结果分组导入。personal provider 写 provider_config.json
/// （personalModelIds/modelOrder + providerModelRules），账号类写
/// v2/config.json 的 provider.models。只新增，不覆盖。
pub fn import_groups(groups: &[(String, Vec<ModelRow>)]) -> ImportResult {
    let mut res = ImportResult::default();
    if groups.is_empty() {
        res.error = "没有可写入的模型。".into();
        return res;
    }

    let personal_ids: std::collections::HashSet<String> = {
        let mut s = std::collections::HashSet::new();
        if let Ok(data) = std::fs::read_to_string(personal_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                for rule in obj(obj(obj(&v, "config"), "providerConfigRules"), "providerRules")
                    .as_array().map(|a| a.clone()).unwrap_or_default()
                {
                    if let Some(pid) = obj(&rule, "providerId").as_str() {
                        s.insert(pid.to_string());
                    }
                }
            }
        }
        s
    };

    let mut personal: Vec<(String, &Vec<ModelRow>)> = Vec::new();
    let mut account: Vec<(String, &Vec<ModelRow>)> = Vec::new();
    for (pid, rows) in groups {
        if personal_ids.contains(pid) {
            personal.push((pid.clone(), rows));
        } else {
            account.push((pid.clone(), rows));
        }
    }

    // —— personal → provider_config.json ——
    if !personal.is_empty() {
        let path = personal_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            res.error = "找不到 provider_config.json。".into();
            return res;
        };
        let mut data: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                res.error = format!("provider_config.json 解析失败：{e}");
                return res;
            }
        };
        res.backup = backup_file(&path);
        let rules = obj(obj(obj(&data, "config"), "providerConfigRules"), "providerRules")
            .as_array().cloned().unwrap_or_default();
        let mcr = {
            // 确保 config.modelConfigRules.providerModelRules 存在
            let cfg = data["config"].as_object_mut().unwrap();
            let mcr = cfg.entry("modelConfigRules".to_string()).or_insert_with(|| json!({}));
            mcr["providerModelRules"] = mcr["providerModelRules"].clone();
            let arr = if mcr["providerModelRules"].is_array() {
                mcr["providerModelRules"].as_array().unwrap().clone()
            } else {
                mcr["providerModelRules"] = json!([]);
                Vec::new()
            };
            arr
        };
        let mut pmr = mcr;
        for (pid, rows) in personal.iter() {
            let rule = rules.iter().find(|r| obj(r, "providerId").as_str() == Some(pid));
            let Some(rule) = rule else {
                res.missing.push(pid.clone());
                continue;
            };
            let cfg_mut = &data["config"]["providerConfigRules"]["providerRules"];
            let _ = cfg_mut; // borrow 帮手（实际改动在下面的循环外做）
            let idx = rules.iter().position(|r| obj(r, "providerId").as_str() == Some(pid)).unwrap();
            // personalModelIds / modelOrder 追加
            let ptr = format!("/config/providerConfigRules/providerRules/{idx}/config");
            let rule_cfg = data.pointer_mut(&ptr).unwrap();
            let ids = rule_cfg["personalModelIds"].as_array().cloned().unwrap_or_default();
            let order = rule_cfg["modelOrder"].as_array().cloned().unwrap_or_default();
            let mut ids = ids;
            let mut order = order;
            for row in rows.iter() {
                if ids.iter().any(|x| x.as_str() == Some(row.id.as_str())) {
                    res.skipped.push(format!("{pid} / {}", row.id));
                    continue;
                }
                ids.push(json!(row.id));
                if !order.iter().any(|x| x.as_str() == Some(row.id.as_str())) {
                    order.push(json!(row.id));
                }
                pmr.push(json!({
                    "providerId": pid, "modelId": row.id, "config": {"enabled": true}
                }));
                res.added += 1;
                res.added_ids.push((pid.clone(), row.id.clone()));
            }
            rule_cfg["personalModelIds"] = json!(ids);
            rule_cfg["modelOrder"] = json!(order);
        }
        data["config"]["modelConfigRules"]["providerModelRules"] = json!(pmr);
        if !write_json_atomic(&path, &data) {
            res.error = "写入 provider_config.json 失败，原文件未修改。".into();
            return res;
        }
    }

    // —— account → v2/config.json ——
    if !account.is_empty() {
        let path = v2_config_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            res.error = "找不到 v2/config.json。".into();
            return res;
        };
        let mut data: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                res.error = format!("v2/config.json 解析失败：{e}");
                return res;
            }
        };
        res.backup = backup_file(&path);
        for (pid, rows) in account.iter() {
            if data["provider"].get(pid).is_none() {
                res.missing.push(pid.clone());
                continue;
            }
            let models = data["provider"][pid]["models"].as_object().cloned().unwrap_or_default();
            let mut models = models;
            for row in rows.iter() {
                if models.contains_key(&row.id) {
                    res.skipped.push(format!("{pid} / {}", row.id));
                    continue;
                }
                models.insert(row.id.clone(), json!({"enabled": true}));
                res.added += 1;
                res.added_ids.push((pid.clone(), row.id.clone()));
            }
            data["provider"][pid]["models"] = models.into_iter().collect::<serde_json::Map<String, Value>>().into();
        }
        if !write_json_atomic(&path, &data) {
            res.error = "写入 v2/config.json 失败，原文件未修改。".into();
            return res;
        }
    }
    res
}

/// 写盘后校验：新增条目是否仍在配置里（被客户端写回覆盖返回 false）。
pub fn verify_import_applied(added_ids: &[(String, String)]) -> bool {
    if added_ids.is_empty() {
        return true;
    }
    // personal 判定
    let personal_ids: std::collections::HashSet<String> = std::fs::read_to_string(personal_path())
        .ok()
        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
        .map(|v| {
            obj(obj(obj(&v, "config"), "providerConfigRules"), "providerRules")
                .as_array().map(|a| {
                    a.iter().filter_map(|r| obj(r, "providerId").as_str().map(|s| s.to_string())).collect()
                }).unwrap_or_default()
        })
        .unwrap_or_default();

    let personal: Vec<_> = added_ids.iter().filter(|(p, _)| personal_ids.contains(p)).collect();
    if !personal.is_empty() {
        let Ok(data) = std::fs::read_to_string(personal_path()) else { return false };
        let Ok(v) = serde_json::from_str::<Value>(&data) else { return false };
        let pmr = obj(obj(obj(&v, "config"), "modelConfigRules"), "providerModelRules")
            .as_array().cloned().unwrap_or_default();
        let mut have: std::collections::HashSet<(String, String)> = pmr.into_iter().filter_map(|r| {
            Some((text(obj(&r, "providerId"), ""), text(obj(&r, "modelId"), "")))
        }).collect();
        for rule in obj(obj(obj(&v, "config"), "providerConfigRules"), "providerRules")
            .as_array().map(|a| a.clone()).unwrap_or_default()
        {
            let pid = text(obj(&rule, "providerId"), "");
            for id in obj(&rule, "config").get("personalModelIds")
                .and_then(|x| x.as_array()).map(|a| a.clone()).unwrap_or_default()
            {
                if let Some(s) = id.as_str() {
                    have.insert((pid.clone(), s.to_string()));
                }
            }
        }
        for (p, m) in &personal {
            if !have.contains(&(p.clone(), m.clone())) {
                return false;
            }
        }
    }
    let account: Vec<_> = added_ids.iter().filter(|(p, _)| !personal_ids.contains(p)).collect();
    if !account.is_empty() {
        let Ok(data) = std::fs::read_to_string(v2_config_path()) else { return false };
        let Ok(v) = serde_json::from_str::<Value>(&data) else { return false };
        for (p, m) in &account {
            match v["provider"].get(p).and_then(|prov| prov.get("models")) {
                Some(models) if models.get(m).is_some() => {}
                _ => return false,
            }
        }
    }
    true
}

fn backup_file(path: &PathBuf) -> String {
    let dir = crate::data_dir().join("registry-backups");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = format!("{}.bak-{}", path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), stamp);
    let target = dir.join(name);
    match std::fs::copy(path, &target) {
        Ok(_) => target.to_string_lossy().to_string(),
        Err(_) => String::new(),
    }
}

fn write_json_atomic(path: &PathBuf, v: &Value) -> bool {
    let body = serde_json::to_string_pretty(v).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_ok()
}
