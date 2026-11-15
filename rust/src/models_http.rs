use super::*;
use reqwest::{header::{HeaderMap, HeaderName, HeaderValue}, Client, Url};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
pub(super) struct ActiveGuard(String);
impl ActiveGuard {
    pub fn acquire(paths: &ConfigPaths, pid: &str) -> Result<Self, String> {
        let key = format!("{}|{}|{}", paths.personal_config.as_ref().map(|p| config::path_key(p)).unwrap_or_default(),
            paths.v2_config.as_ref().map(|p| config::path_key(p)).unwrap_or_default(), pid);
        let mut active = ACTIVE.get_or_init(Default::default).lock().map_err(|_| "模型任务锁不可用。".to_string())?;
        if active.len() >= 4 { return Err("已有四个模型任务运行，请稍后重试。".into()); }
        if !active.insert(key.clone()) { return Err("该 provider 上次请求仍在结束，请稍后重试。".into()); }
        Ok(Self(key))
    }
}
impl Drop for ActiveGuard { fn drop(&mut self) { if let Some(lock) = ACTIVE.get() { if let Ok(mut active) = lock.lock() { active.remove(&self.0); } } } }

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Api { Chat, Responses, Anthropic }
impl Api {
    pub fn as_str(self) -> &'static str { match self { Self::Chat => "openai-chat-completions", Self::Responses => "openai-responses", Self::Anthropic => "anthropic-messages" } }
}
pub(super) struct Endpoint { pub base: String, pub list: Url, pub probe: Url, pub api: Api }
impl Endpoint {
    pub fn for_record(record: &config::ProviderRecord) -> Result<Self, String> {
        let options = &record.options;
        let raw = options.get("baseURL").or_else(|| options.get("baseUrl")).and_then(Value::as_str).ok_or("未配置 API 地址。")?;
        if raw.trim().is_empty() { return Err("未配置 API 地址。".into()); }
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) || raw.contains('\\') { return Err("API 地址包含无效字符。".into()); }
        let mut base = Url::parse(raw).map_err(|_| "API 地址格式无效。".to_string())?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() || base.port() == Some(0) { return Err("API 地址必须是有效的 HTTP(S) 地址。".into()); }
        if !base.username().is_empty() || base.password().is_some() || base.query().is_some() || base.fragment().is_some() {
            return Err("请使用不含认证信息、查询参数或片段的 API 基础地址。".into());
        }
        let mut path = base.path().trim_end_matches('/').to_owned();
        for tail in ["/chat/completions", "/completions", "/responses", "/messages", "/models"] {
            if path.to_ascii_lowercase().ends_with(tail) { path.truncate(path.len() - tail.len()); break; }
        }
        base.set_path(path.trim_end_matches('/'));
        let base_string = base.as_str().trim_end_matches('/').to_owned();
        let api = match record.info.kind.to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai-chat-completions" => Api::Chat,
            "openai-responses" | "openai-compatible-responses" => Api::Responses,
            "anthropic" | "anthropic-messages" => Api::Anthropic,
            _ => return Err("该 provider 协议不支持发现或探针（支持 OpenAI Chat、Responses 与 Anthropic）。".into()),
        };
        let root = if api == Api::Anthropic && !path.ends_with("/v1") || api != Api::Anthropic && path.is_empty() {
            format!("{base_string}/v1")
        } else { base_string.clone() };
        let list = Url::parse(&format!("{root}/models")).map_err(|_| "模型接口地址无效。".to_string())?;
        let suffix = match api { Api::Chat => "chat/completions", Api::Responses => "responses", Api::Anthropic => "messages" };
        let probe = Url::parse(&format!("{root}/{suffix}")).map_err(|_| "探针接口地址无效。".to_string())?;
        Ok(Self { base: base_string, list, probe, api })
    }
}
fn headers(options: &Value, api: Api) -> Result<HeaderMap, String> {
    let mut hs = HeaderMap::new();
    hs.insert("accept", HeaderValue::from_static("application/json"));
    hs.insert("user-agent", HeaderValue::from_static("zcode-token-bar-rs/1"));
    if api == Api::Anthropic { hs.insert("anthropic-version", HeaderValue::from_static("2023-06-01")); }
    let primary = if api == Api::Anthropic { "x-api-key" } else { "authorization" };
    let custom = options.get("headers");
    if custom.is_some_and(|v| !v.is_null() && !v.is_object()) { return Err("自定义 headers 必须是对象。".into()); }
    let custom = custom.and_then(Value::as_object);
    let override_auth = custom.is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case(primary)));
    if !override_auth {
        let key = config::credential(&options["apiKey"])?;
        if !key.is_empty() {
            let value = if api == Api::Anthropic { key } else { format!("Bearer {key}") };
            let mut value = HeaderValue::from_str(&value).map_err(|_| "API Key 不是有效 HTTP 认证值。".to_string())?;
            value.set_sensitive(true); hs.insert(HeaderName::from_static(primary), value);
        }
    }
    if let Some(custom) = custom {
        let mut seen = HashSet::new();
        for (name, value) in custom {
            let name = name.to_ascii_lowercase();
            if !matches!(name.as_str(), "authorization" | "x-api-key" | "openai-organization" | "openai-project" | "anthropic-version") { continue; }
            if !seen.insert(name.clone()) { return Err("自定义认证头重复（大小写不敏感）。".into()); }
            let value = config::credential(value)?;
            let mut value = HeaderValue::from_str(&value).map_err(|_| "自定义认证头格式无效。".to_string())?;
            value.set_sensitive(matches!(name.as_str(), "authorization" | "x-api-key"));
            hs.insert(HeaderName::from_bytes(name.as_bytes()).map_err(|_| "认证头名称无效。".to_string())?, value);
        }
    }
    let authenticated = ["authorization", "x-api-key"].iter().any(|k| hs.get(*k).is_some_and(|v| !v.as_bytes().is_empty()));
    if !authenticated && options.get("apiKeyRequired") != Some(&Value::Bool(false)) { return Err("未配置可用 API Key；请在 ZCode 中设置认证。".into()); }
    Ok(hs)
}
struct Session { runtime: tokio::runtime::Runtime, client: Client, headers: HeaderMap }
struct Reply { status: u16, value: Value }
impl Session {
    fn new(options: &Value, api: Api) -> Result<Self, String> {
        if tokio::runtime::Handle::try_current().is_ok() { return Err("同步模型 API 必须在后台线程调用。".into()); }
        let hs = headers(options, api)?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| "无法创建模型网络运行时。".to_string())?;
        let builder = Client::builder().redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(8)).timeout(Duration::from_secs(15));
        // fixture 的 loopback 请求必须完全隔离用户真实系统代理。
        #[cfg(test)] let builder = builder.no_proxy();
        let client = builder.build().map_err(|_| "无法创建 HTTP 客户端。".to_string())?;
        Ok(Self { runtime, client, headers: hs })
    }
    fn request(&self, url: &Url, body: Option<Value>, op: &Operation) -> Result<Reply, String> {
        op.check()?;
        let limit = if body.is_some() { Duration::from_secs(15) } else { Duration::from_secs(8) }.min(op.remaining());
        let deadline = Instant::now() + limit;
        self.runtime.block_on(async {
            let work = async {
                let request = match body { Some(body) => self.client.post(url.clone()).json(&body), None => self.client.get(url.clone()) };
                let mut response = request.headers(self.headers.clone()).timeout(limit).send().await
                    .map_err(|e| if e.is_timeout() { "请求超时。".to_string() } else { "网络请求失败，请检查连接或代理。".to_string() })?;
                let status = response.status().as_u16();
                if (300..400).contains(&status) { return Err("模型接口发生重定向，已阻止转发认证。请检查 API 基础地址。".into()); }
                if response.content_length().is_some_and(|len| len > MAX_BODY as u64) { return Err("模型响应超过 2 MiB 限制。".into()); }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| "读取模型响应失败。".to_string())? {
                    if bytes.len() + chunk.len() > MAX_BODY { return Err("模型响应超过 2 MiB 限制。".into()); }
                    bytes.extend_from_slice(&chunk);
                }
                let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                Ok(Reply { status, value })
            };
            let mut work = Box::pin(work);
            loop {
                op.check()?;
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() { return Err("请求超时。".into()); }
                match tokio::time::timeout(Duration::from_millis(25).min(left), &mut work).await {
                    Ok(reply) => { op.check()?; return reply; },
                    Err(_) => continue,
                }
            }
            // 取消/超时时 future 被丢弃，连接关闭，不残留继续发下一请求的后台任务。
        })
    }
}
fn http_error(status: u16) -> String {
    format!("HTTP {status}：{}", match status {
        401 => "认证失败，请检查 API Key。", 403 => "账号无权访问此接口。", 404 => "该 API 路径或模型不存在。",
        405 => "供应商不支持此接口。", 429 => "请求限流或额度不足，请稍后重试。", 500..=599 => "供应商服务暂时不可用。",
        _ => "供应商拒绝请求；未显示响应正文。",
    })
}
fn explicit_no_list(payload: &Value) -> bool {
    let code = payload.pointer("/error/code").or_else(|| payload.get("code")).and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    matches!(code.as_str(), "not_implemented" | "method_not_allowed" | "models_not_supported" | "unsupported_endpoint" | "endpoint_not_found")
}
pub(super) fn discover(record: &config::ProviderRecord, rules: &Value, op: &Operation) -> Result<RichDiscovery, String> {
    let fallback = |note: &str| -> Result<RichDiscovery, String> {
        op.check()?;
        let (rows, truncated) = metadata::local_models(record, rules);
        if rows.is_empty() { return Err("该端点不提供模型列表，且当前账号配置与 runtime catalog 没有模型清单。".into()); }
        Ok(RichDiscovery { provider: record.info.name.clone(), provider_id: record.info.id.clone(), rows,
            truncated, source_note: note.into() })
    };
    if record.account_only { return fallback("账号 provider 没有可独立调用的 API 认证；清单来自当前配置和 runtime catalog，尚未实测。"); }
    let endpoint = Endpoint::for_record(record)?;
    let session = Session::new(&record.options, endpoint.api)?;
    let secrets = record.secrets(); let mut rows = Vec::new(); let mut seen = HashSet::new(); let mut cursors = HashSet::new();
    let mut url = endpoint.list.clone(); let mut truncated = false;
    for page in 0..MAX_PAGES {
        op.check()?; op.emit(&record.info.id, "", "discovery", page, MAX_PAGES);
        let reply = session.request(&url, None, op)?;
        if matches!(reply.status, 404 | 405) || explicit_no_list(&reply.value) && !matches!(reply.status, 401 | 403 | 429 | 500..=599) {
            if page != 0 { return Err("分页请求失败，未返回不完整清单。".into()); }
            return fallback("该端点不提供模型列表接口；清单与能力来自 ZCode 配置和本地 runtime catalog。");
        }
        if !(200..300).contains(&reply.status) { return Err(http_error(reply.status)); }
        let payload = &reply.value;
        if !payload.is_object() { return Err("供应商返回的模型列表不是有效 JSON 对象。".into()); }
        if payload.get("error").is_some_and(|e| !e.is_null()) { return Err("供应商在成功状态中返回错误，未回退或使用响应正文。".into()); }
        let entries = payload.get("data").or_else(|| payload.get("models"));
        let Some(entries) = entries else {
            if page != 0 { return Err("供应商分页响应缺少模型列表。".into()); }
            return fallback("供应商响应中没有模型列表；清单与能力来自 ZCode 配置和本地 runtime catalog。");
        };
        let entries = entries.as_array().ok_or("供应商响应中的 data/models 必须是数组。")?;
        for item in entries {
            let raw = item.as_str().or_else(|| item.get("id").or_else(|| item.get("name")).and_then(Value::as_str)).unwrap_or("");
            let id = raw.trim();
            if raw.chars().any(char::is_control) || !config::valid_id(id, &secrets) || !seen.insert(id.to_owned()) { continue; }
            if rows.len() >= MAX_MODELS { truncated = true; break; }
            rows.push(metadata::make_row(record, rules, id, item));
        }
        let more = payload.get("has_more") == Some(&Value::Bool(true));
        if truncated || more && (rows.len() >= MAX_MODELS || page + 1 == MAX_PAGES) { truncated = true; break; }
        if !more { break; }
        let cursor = payload.get("last_id").or_else(|| payload.get("next_cursor")).and_then(Value::as_str).unwrap_or("");
        if cursor.is_empty() || cursor.len() > 1024 || cursor.chars().any(char::is_control) || secrets.iter().any(|s| cursor.contains(s)) || !cursors.insert(cursor.to_owned()) {
            truncated = true; break;
        }
        url = endpoint.list.clone();
        url.query_pairs_mut().append_pair(if endpoint.api == Api::Anthropic { "after_id" } else { "after" }, cursor).append_pair("limit", "100");
    }
    op.check()?;
    if rows.is_empty() { return fallback("模型列表中没有有效 ID；使用当前配置和本地 runtime catalog，尚未实测。"); }
    op.emit(&record.info.id, "", "discovery", MAX_PAGES, MAX_PAGES);
    Ok(RichDiscovery { provider: record.info.name.clone(), provider_id: record.info.id.clone(), rows, truncated,
        source_note: if truncated { "清单达到分页或数量上限；仅显示已取得的模型。".into() } else { "清单来自 provider API；能力仅按明确声明合并。".into() } })
}

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
const TOOL: &str = "zb_probe";
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (chunk.get(1).copied().unwrap_or(0) as u32) << 8 | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char); out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}
fn pdf_base64() -> String {
    // 一个合法单页空白 PDF，含 xref/startxref/EOF；不会把损坏 PDF 当作能力拒绝。
    let mut pdf = b"%PDF-1.4\n".to_vec(); let mut offsets = Vec::new();
    for (i, object) in ["<< /Type /Catalog /Pages 2 0 R >>", "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 36 72] /Resources << >> /Contents 4 0 R >>", "<< /Length 0 >>\nstream\n\nendstream"].iter().enumerate() {
        offsets.push(pdf.len()); pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, object).as_bytes());
    }
    let xref = pdf.len(); pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets { pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes()); }
    pdf.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes()); base64(&pdf)
}
pub(super) fn probe_body(api: Api, model: &str, kind: &str) -> Value {
    let mut body = match api {
        Api::Chat => json!({"model":model,"max_tokens":8,"stream":false,"messages":[{"role":"user","content":"Reply OK."}]}),
        Api::Anthropic => json!({"model":model,"max_tokens":8,"stream":false,"messages":[{"role":"user","content":"Reply OK."}]}),
        Api::Responses => json!({"model":model,"max_output_tokens":16,"stream":false,"store":false,"input":[{"role":"user","content":"Reply OK."}]}),
    };
    if kind == "tools" {
        match api {
            Api::Anthropic => { body["max_tokens"] = json!(16); body["tools"] = json!([{"name":TOOL,"description":"Return OK.","input_schema":{"type":"object","properties":{},"additionalProperties":false}}]); body["tool_choice"] = json!({"type":"tool","name":TOOL}); }
            Api::Chat => { body["max_tokens"] = json!(16); body["tools"] = json!([{"type":"function","function":{"name":TOOL,"description":"Return OK.","parameters":{"type":"object","properties":{},"additionalProperties":false}}}]); body["tool_choice"] = json!({"type":"function","function":{"name":TOOL}}); }
            Api::Responses => { body["tools"] = json!([{"type":"function","name":TOOL,"description":"Return OK.","parameters":{"type":"object","properties":{},"additionalProperties":false},"strict":true}]); body["tool_choice"] = json!({"type":"function","name":TOOL}); }
        }
    } else if matches!(kind, "image" | "pdf") {
        let pdf = pdf_base64();
        let part = match (api, kind) {
            (Api::Anthropic, "image") => json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}),
            (Api::Anthropic, _) => json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":pdf}}),
            (Api::Responses, "image") => json!({"type":"input_image","image_url":format!("data:image/png;base64,{PNG}"),"detail":"low"}),
            (Api::Responses, _) => json!({"type":"input_file","filename":"probe.pdf","file_data":format!("data:application/pdf;base64,{pdf}")}),
            (_, "image") => json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}"),"detail":"low"}}),
            _ => json!({"type":"file","file":{"filename":"probe.pdf","file_data":format!("data:application/pdf;base64,{pdf}")}}),
        };
        let text_type = if api == Api::Responses { "input_text" } else { "text" };
        let content = json!([{"type":text_type,"text":"Reply OK."},part]);
        body[if api == Api::Responses { "input" } else { "messages" }] = json!([{"role":"user","content":content}]);
    }
    body
}
fn accepted(api: Api, payload: &Value) -> bool {
    if !payload.is_object() || payload.get("error").is_some_and(|e| !e.is_null()) { return false; }
    match api {
        Api::Chat => payload.get("choices").and_then(Value::as_array).is_some_and(|choices| choices.iter().any(|c| c.get("message").is_some_and(|m| m.is_object()))),
        Api::Anthropic => payload.get("content").and_then(Value::as_array).is_some_and(|c| c.iter().any(|p| p.get("type").and_then(Value::as_str).is_some_and(|t| matches!(t, "text" | "tool_use" | "thinking")))),
        Api::Responses => payload.get("output").and_then(Value::as_array).is_some_and(|out| out.iter().any(|v| matches!(string(&v["type"]), "message" | "function_call" | "reasoning"))) && string(&payload["status"]) != "failed",
    }
}
fn final_text(api: Api, payload: &Value) -> bool {
    let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    if !accepted(api, payload) { return false; }
    match api {
        Api::Chat => payload.get("choices").and_then(Value::as_array).is_some_and(|choices| choices.iter().any(|choice| {
            let message = &choice["message"];
            let calls = message.get("tool_calls").and_then(Value::as_array).is_some_and(|c| !c.is_empty());
            !calls && (nonempty(&message["content"]) || message["content"].as_array().is_some_and(|parts| parts.iter().any(|p| p["type"] == "text" && nonempty(&p["text"]))))
        })),
        Api::Anthropic => payload["content"].as_array().is_some_and(|content| !content.iter().any(|p| p["type"] == "tool_use") && content.iter().any(|p| p["type"] == "text" && nonempty(&p["text"]))),
        Api::Responses => payload["output"].as_array().is_some_and(|output| !output.iter().any(|p| p["type"] == "function_call") && output.iter().any(|p| p["type"] == "message" && p["content"].as_array().is_some_and(|parts| parts.iter().any(|p| p["type"] == "output_text" && nonempty(&p["text"]))))),
    }
}
fn tool_roundtrip(api: Api, model: &str, payload: &Value) -> Option<Value> {
    let mut body = probe_body(api, model, "text");
    let valid_call = |name: &Value, id: &Value| name.as_str() == Some(TOOL) && id.as_str().is_some_and(|s| config::valid_id(s, &[]));
    match api {
        Api::Chat => {
            let msg = payload.pointer("/choices/0/message")?;
            let calls = msg.get("tool_calls")?.as_array()?;
            let call = calls.iter().find(|c| valid_call(&c["function"]["name"], &c["id"]) && c["function"]["arguments"].as_str().is_some_and(|s| serde_json::from_str::<Value>(s).is_ok_and(|v| v.is_object())))?;
            let safe_call = json!({"id":call["id"],"type":"function","function":{"name":TOOL,"arguments":"{}"}});
            body["messages"] = json!([{"role":"user","content":"Call zb_probe, then reply OK."},{"role":"assistant","content":null,"tool_calls":[safe_call]},{"role":"tool","tool_call_id":call["id"],"content":"OK"}]);
        }
        Api::Anthropic => {
            let call = payload.get("content")?.as_array()?.iter().find(|c| c["type"] == "tool_use" && valid_call(&c["name"], &c["id"]) && c["input"].is_object())?;
            body["messages"] = json!([{"role":"user","content":"Call zb_probe, then reply OK."},{"role":"assistant","content":[{"type":"tool_use","id":call["id"],"name":TOOL,"input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":call["id"],"content":"OK"}]}]);
        }
        Api::Responses => {
            let call = payload.get("output")?.as_array()?.iter().find(|c| c["type"] == "function_call" && valid_call(&c["name"], &c["call_id"]) && c["arguments"].as_str().is_some_and(|s| serde_json::from_str::<Value>(s).is_ok_and(|v| v.is_object())))?;
            body["input"] = json!([{"role":"user","content":"Call zb_probe, then reply OK."},{"type":"function_call","call_id":call["call_id"],"name":TOOL,"arguments":"{}"},{"type":"function_call_output","call_id":call["call_id"],"output":"OK"}]);
        }
    }
    // 继续回合保留工具 schema，移除强制调用，防止又被要求调用。
    let tool_body = probe_body(api, model, "tools"); body["tools"] = tool_body["tools"].clone();
    Some(body)
}
fn media_evidence(payload: &Value, kind: &str) -> Option<bool> {
    let usage = &payload["usage"];
    let aliases: &[&str] = if kind == "image" { &["image_tokens", "image_input_tokens", "cache_read_image_tokens", "vision_tokens"] } else { &["pdf_tokens", "document_tokens", "file_tokens"] };
    for holder in [usage, &usage["prompt_tokens_details"], &usage["input_tokens_details"]] {
        if aliases.iter().any(|a| holder[*a].as_u64().is_some_and(|n| n > 0)) { return Some(true); }
    }
    // 零 token 可能是缓存/供应商省略统计，不能据此推断模型不支持。
    None
}
fn explicit_unsupported(payload: &Value, field: &str) -> bool {
    let error = &payload["error"];
    let text = format!("{} {} {}", string(&error["code"]), string(&error["param"]), string(&error["message"])).to_ascii_lowercase();
    let neg = ["not support", "unsupported", "not available", "does not exist", "not found", "no such", "不支持", "不存在"].iter().any(|s| text.contains(s));
    let names: &[&str] = match field { "text" => &["model", "模型"], "tools" => &["tool", "function", "工具"], "image" => &["image", "vision", "图片"], _ => &["pdf", "document", "file", "文件"] };
    neg && names.iter().any(|s| text.contains(s))
}
pub(super) fn probe(record: &config::ProviderRecord, model_id: &str, op: &Operation) -> Result<RichProbeOutcome, String> {
    op.check()?;
    if !config::valid_id(model_id, &record.secrets()) { return Err("模型 ID 无效或含认证信息。".into()); }
    let mut out = RichProbeOutcome { provider_id: record.info.id.clone(), model_id: model_id.into(), ..Default::default() };
    if record.account_only { out.error = "账号 provider 没有可独立使用的 API 认证，探针保持未知；不会自动导入。".into(); return Ok(out); }
    let endpoint = Endpoint::for_record(record)?;
    let session = Session::new(&record.options, endpoint.api)?;
    for (i, field) in ["text", "tools", "image", "pdf"].iter().enumerate() {
        if let Err(error) = op.check() { out.error = error; break; }
        op.emit(&record.info.id, model_id, field, i, 4);
        out.requests += 1;
        let reply = match session.request(&endpoint.probe, Some(probe_body(endpoint.api, model_id, field)), op) {
            Ok(r) => r, Err(error) => {
                out.errors.insert((*field).into(), error.clone());
                if *field == "text" || op.check().is_err() { out.error = error; break; }
                continue;
            }
        };
        if !(200..300).contains(&reply.status) {
            let error = http_error(reply.status); out.errors.insert((*field).into(), error.clone());
            let unsupported = matches!(reply.status, 400 | 404 | 415 | 422) && explicit_unsupported(&reply.value, field);
            match *field { "text" => { if unsupported { out.usable = Some(false); } out.error = error; break; },
                "tools" if unsupported => out.tools = Some(false), "image" if unsupported => out.image = Some(false), "pdf" if unsupported => out.pdf = Some(false), _ => {} }
            if matches!(reply.status, 401 | 403 | 429) { out.error = error; break; }
            continue;
        }
        if !accepted(endpoint.api, &reply.value) {
            let error = "响应未提供有效协议结果，能力保持未知。".to_string(); out.errors.insert((*field).into(), error.clone());
            if *field == "text" { out.error = error; break; } continue;
        }
        match *field {
            "text" => {
                if final_text(endpoint.api, &reply.value) { out.usable = Some(true); }
                else { out.error = "文本探针未取得实际文本输出，可用性保持未知。".into(); break; }
            },
            "tools" => {
                if let Some(body) = tool_roundtrip(endpoint.api, model_id, &reply.value) {
                    op.emit(&record.info.id, model_id, "tool_result", i, 4);
                    out.requests += 1;
                    match session.request(&endpoint.probe, Some(body), op) {
                        Ok(r) if (200..300).contains(&r.status) && final_text(endpoint.api, &r.value) => out.tools = Some(true),
                        Ok(r) if (200..300).contains(&r.status) => { out.errors.insert("tools".into(), "工具结果回传后没有最终文本，工具能力保持未知。".into()); },
                        Ok(r) => { out.errors.insert("tools".into(), http_error(r.status)); },
                        Err(e) => { out.errors.insert("tools".into(), e); },
                    }
                }
                // 被网关静默忽略或截断不是明确“不支持”。
            }
            "image" => out.image = media_evidence(&reply.value, field),
            "pdf" => out.pdf = media_evidence(&reply.value, field), _ => {},
        }
    }
    out.cancelled = op.cancel.is_cancelled(); out.timed_out = op.is_timed_out();
    if out.cancelled || out.timed_out { out.error = op.check().err().unwrap_or_default(); }
    op.emit(&record.info.id, model_id, "probe", 4, 4);
    Ok(out)
}
