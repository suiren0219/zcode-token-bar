//! 原生 HTTP/SSE。禁止自动代理和重定向，所有错误均归入固定安全类别。
use super::config::Connection;
use super::{CompanionError, CompanionSettings, Protocol, Signal};
use chrono::Timelike;
use reqwest::header::{HeaderValue, ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use serde_json::{json, Value};
use std::time::Duration;

pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const SYSTEM: &str = "你是小亚，温暖、自然、俏皮而有边界的陪伴者，称呼对方为哥哥。只用简体中文回复1～3句简短的话，不输出推理过程、工具调用或长篇建议。你只能使用当前输入中明确提供的信息；工作状态元数据不代表用户情绪、身体状态或动作，不得据此编造用户情绪或动作，也不要假装观察到了用户的动作或情绪。不要索取工作内容、代码或对话原文，不复述状态元数据。";

pub(super) fn client(timeout_seconds: u64) -> Result<reqwest::Client, CompanionError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(timeout_seconds.clamp(1, 15)))
        .timeout(Duration::from_secs(timeout_seconds.clamp(1, 180)))
        .pool_max_idle_per_host(0)
        .user_agent("zcode-token-bar-companion/1")
        .build().map_err(|_| CompanionError::Internal)
}

pub(super) fn prompt(signal: &Signal) -> Result<String, CompanionError> {
    let hour = chrono::Local::now().hour();
    let time_of_day = match hour { 5..=10 => "清晨", 11..=13 => "中午", 14..=17 => "下午", 18..=22 => "晚上", _ => "深夜" };
    match signal {
        Signal::Greeting => Ok(format!("现在是{time_of_day}。哥哥主动点了问候，请自然打个招呼；不要假定他正在做什么。")),
        Signal::SaySomething => Ok(format!("现在是{time_of_day}。哥哥主动点了来一句，请给1～3句轻松的陪伴话语，不猜测他的情绪。")),
        Signal::Message(text) => {
            if text.trim().is_empty() || text.chars().count() > 2000 || text.contains('\0') {
                return Err(CompanionError::Message);
            }
            Ok(text.trim().to_owned())
        }
        Signal::Auto(event) => {
            event.validate()?;
            Ok(format!("以下仅是工作状态元数据，不代表用户的情绪、身体状态或动作。请用1～3句简短回应，无需复述元数据。不得据此编造用户情绪或动作，也不要假装观察到了用户的动作或情绪。不要输出推理过程。\n{}",
                json!({"kind": event.kind, "occurred_at": event.occurred_at, "duration_ms": event.duration_ms,
                    "tool_errors": event.tool_errors, "time_of_day": time_of_day})))
        }
    }
}

pub(super) fn payload(connection: &Connection, prompt: &str, settings: &CompanionSettings) -> Value {
    match connection.protocol {
        Protocol::AstrBot => {
            let mut body = json!({"message": format!("{SYSTEM}\n{prompt}"), "session_id": connection.session_id,
                "username": connection.username, "enable_streaming": settings.stream});
            body[connection.route_field] = json!(connection.route_value);
            body
        }
        Protocol::OpenAi => {
            let mut body = json!({"model": connection.model, "messages": [
                {"role":"system", "content":SYSTEM}, {"role":"user", "content":prompt}], "stream":settings.stream});
            let modern = ["o1", "o3", "o4", "gpt-5", "gpt-6"].iter().any(|s| connection.model.starts_with(s));
            body[if modern { "max_completion_tokens" } else { "max_tokens" }] = json!(settings.max_output_tokens);
            body
        }
        Protocol::Anthropic => json!({"model":connection.model, "system":SYSTEM,
            "messages":[{"role":"user", "content":prompt}], "max_tokens":settings.max_output_tokens, "stream":settings.stream}),
        Protocol::Responses => json!({"model":connection.model, "instructions":SYSTEM,
            "input":[{"role":"user", "content":[{"type":"input_text", "text":prompt}]}],
            "max_output_tokens":settings.max_output_tokens, "store":false, "stream":settings.stream}),
    }
}

fn network_error(error: reqwest::Error) -> CompanionError {
    if error.is_timeout() { return CompanionError::Timeout; }
    if error.is_body() || error.is_decode() { return CompanionError::Format; }
    // 只判别错误链，不将其中可能含 URL/响应内容的文本传给 UI 或日志。
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    for _ in 0..12 {
        let Some(current) = source else { break; };
        let s = current.to_string().to_ascii_lowercase();
        if s.contains("certificate") || s.contains("tls") || s.contains("ssl") { return CompanionError::Tls; }
        source = current.source();
    }
    CompanionError::Network
}

fn status(code: u16) -> Result<(), CompanionError> {
    match code {
        200..=299 => Ok(()),
        300..=399 => Err(CompanionError::Redirect),
        401 | 403 => Err(CompanionError::Auth),
        429 => Err(CompanionError::RateLimit),
        500..=599 => Err(CompanionError::Server),
        _ => Err(CompanionError::Request),
    }
}

pub(super) async fn request<F: FnMut(String)>(client: &reqwest::Client, connection: &Connection,
    signal: &Signal, settings: &CompanionSettings, mut progress: F) -> Result<String, CompanionError> {
    #[cfg(test)]
    assert!(connection.endpoint.host_str().and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback()), "陪伴测试禁止非回环网络访问");
    let user_prompt = prompt(signal)?;
    let body = payload(connection, &user_prompt, settings);
    let mut request = client.post(connection.endpoint.clone())
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .header(ACCEPT, "application/json, text/event-stream")
        .header(ACCEPT_ENCODING, "identity")
        .json(&body);
    let mut auth = HeaderValue::from_str(&if connection.protocol == Protocol::Anthropic {
        connection.key.clone()
    } else { format!("Bearer {}", connection.key) }).map_err(|_| CompanionError::Config)?;
    auth.set_sensitive(true);
    request = if connection.protocol == Protocol::Anthropic {
        request.header("x-api-key", auth).header("anthropic-version", "2023-06-01")
    } else { request.header(AUTHORIZATION, auth) };
    let mut response = request.send().await.map_err(network_error)?;
    status(response.status().as_u16())?;
    if let Some(value) = response.headers().get(CONTENT_ENCODING) {
        if !value.to_str().map(|s| s.eq_ignore_ascii_case("identity")).unwrap_or(false) {
            return Err(CompanionError::Format);
        }
    }
    if let Some(length) = response.headers().get(CONTENT_LENGTH) {
        let length = length.to_str().ok().and_then(|s| s.parse::<u64>().ok()).ok_or(CompanionError::Format)?;
        if length > MAX_RESPONSE_BYTES as u64 { return Err(CompanionError::Size); }
    }
    let content_type = response.headers().get(CONTENT_TYPE).and_then(|s| s.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mut raw = Vec::new();
    let mut sse = Sse::default();
    let mut streaming = false;
    let mut stream_decided = false;
    let mut last_visible = String::new();
    while let Some(bytes) = response.chunk().await.map_err(network_error)? {
        if raw.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES { return Err(CompanionError::Size); }
        raw.extend_from_slice(&bytes);
        if !stream_decided {
            if let Ok(text) = std::str::from_utf8(&raw) {
                let text = text.trim_start_matches('\u{feff}').trim_start();
                if text.starts_with('{') { stream_decided = true; }
                else if text.starts_with("data:") || text.starts_with("event:") || text.starts_with(':') {
                    streaming = true;
                    stream_decided = true;
                } else if !text.is_empty() && content_type.contains("text/event-stream") {
                    streaming = true;
                    stream_decided = true;
                }
            }
        }
        if streaming {
            sse.consume(&raw, false, connection.protocol)?;
            let visible = sanitize(&sse.text, connection, &user_prompt, true);
            if visible != last_visible && !visible.is_empty() {
                last_visible.clone_from(&visible);
                progress(visible);
            }
            if sse.done { break; }
        }
    }
    let answer = if streaming {
        sse.consume(&raw, true, connection.protocol)?;
        sse.text
    } else {
        let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw);
        if raw.iter().all(u8::is_ascii_whitespace) { return Err(CompanionError::Empty); }
        let data: Value = serde_json::from_slice(raw).map_err(|_| CompanionError::Format)?;
        json_text(&data, connection.protocol)?
    };
    let visible = sanitize(&answer, connection, &user_prompt, false);
    if visible.is_empty() { return Err(CompanionError::Empty); }
    Ok(visible)
}

fn api_error(data: &Value) -> Result<(), CompanionError> {
    if !data.is_object() { return Err(CompanionError::Format); }
    if data.get("error").is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        || data.get("success") == Some(&Value::Bool(false))
        || matches!(data.get("status").and_then(Value::as_str), Some("error" | "failed" | "failure" | "cancelled"))
        || data.get("type").and_then(Value::as_str) == Some("error") {
        return Err(CompanionError::Api);
    }
    Ok(())
}

fn content_text(value: &Value) -> String {
    if let Some(s) = value.as_str() { return s.to_owned(); }
    value.as_array().map(|items| items.iter().filter_map(|item| {
        if matches!(item.get("type").and_then(Value::as_str), Some("text" | "output_text")) {
            item.get("text").and_then(Value::as_str)
        } else { None }
    }).collect::<String>()).unwrap_or_default()
}

fn assistant(value: &Value) -> bool {
    matches!(value.get("role").and_then(Value::as_str), None | Some("assistant"))
}

pub(super) fn json_text(data: &Value, protocol: Protocol) -> Result<String, CompanionError> {
    api_error(data)?;
    match protocol {
        Protocol::AstrBot => astrbot_text(data, 0),
        Protocol::OpenAi => {
            let message = data.pointer("/choices/0/message").unwrap_or(&Value::Null);
            if !assistant(message) { return Ok(String::new()); }
            Ok(content_text(&message["content"]))
        }
        Protocol::Anthropic => {
            if !assistant(data) { return Ok(String::new()); }
            Ok(content_text(&data["content"]))
        }
        Protocol::Responses => {
            if data.get("status").and_then(Value::as_str) == Some("incomplete") {
                return Err(CompanionError::Incomplete);
            }
            let mut answer = String::new();
            if let Some(output) = data.get("output").and_then(Value::as_array) {
                for item in output {
                    if item.get("type").and_then(Value::as_str) == Some("message") && assistant(item) {
                        answer.push_str(&content_text(&item["content"]));
                    }
                }
            }
            if answer.is_empty() { answer = data.get("output_text").and_then(Value::as_str).unwrap_or("").to_owned(); }
            Ok(answer)
        }
    }
}

fn astrbot_text(data: &Value, depth: usize) -> Result<String, CompanionError> {
    if depth > 4 || !data.is_object() { return Err(CompanionError::Format); }
    let kind = data.get("type").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    if data.get("type").is_some_and(|v| !v.is_string()) { return Err(CompanionError::Format); }
    if kind == "error" { return Err(CompanionError::Api); }
    if !matches!(kind.as_str(), "" | "plain") || !assistant(data) { return Ok(String::new()); }
    let chain = data.get("chain_type").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    if data.get("chain_type").is_some_and(|v| !v.is_string()) || !matches!(chain.as_str(), "" | "plain" | "text" | "normal" | "answer" | "final" | "assistant") {
        return Ok(String::new());
    }
    api_error(data)?;
    if let Some(code) = data.get("code") {
        if code != &json!(0) && code != &json!(200) && code != &json!("0") && code != &json!("200") { return Err(CompanionError::Api); }
    }
    if kind == "plain" { return Ok(data.get("data").and_then(Value::as_str).unwrap_or("").into()); }
    if let Some(inner) = data.get("data") {
        return if inner.is_object() { astrbot_text(inner, depth + 1) } else { Ok(String::new()) };
    }
    Ok(["reply", "text", "result", "message"].iter().find_map(|key| data.get(*key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())).unwrap_or("").into())
}

#[derive(Default)]
struct Sse {
    offset: usize,
    event: String,
    data: Vec<String>,
    text: String,
    done: bool,
    deltas: bool,
}

impl Sse {
    fn consume(&mut self, raw: &[u8], end: bool, protocol: Protocol) -> Result<(), CompanionError> {
        if self.offset == 0 && raw.starts_with(&[0xef, 0xbb, 0xbf]) { self.offset = 3; }
        while self.offset < raw.len() && !self.done {
            let rest = &raw[self.offset..];
            let delimiter = rest.iter().position(|b| *b == b'\n' || *b == b'\r');
            let (length, advance) = if let Some(n) = delimiter {
                if rest[n] == b'\r' && n + 1 == rest.len() && !end { break; }
                (n, n + if rest[n] == b'\r' && rest.get(n + 1) == Some(&b'\n') { 2 } else { 1 })
            } else if end { (rest.len(), rest.len()) } else { break; };
            let line = std::str::from_utf8(&rest[..length]).map_err(|_| CompanionError::Format)?;
            self.offset += advance;
            if line.is_empty() { self.frame(protocol)?; continue; }
            if line.starts_with(':') { continue; }
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match name { "data" => self.data.push(value.into()), "event" => self.event = value.to_ascii_lowercase(), _ => {} }
        }
        if end && !self.done && !self.data.is_empty() { self.frame(protocol)?; }
        Ok(())
    }

    fn frame(&mut self, protocol: Protocol) -> Result<(), CompanionError> {
        let event = std::mem::take(&mut self.event);
        if self.data.is_empty() { return Ok(()); }
        let raw = std::mem::take(&mut self.data).join("\n");
        if event == "error" { return Err(CompanionError::Api); }
        if raw.trim() == "[DONE]" { self.done = true; return Ok(()); }
        if raw.trim().is_empty() { return Ok(()); }
        if protocol == Protocol::AstrBot && !matches!(event.as_str(), "" | "message" | "plain" | "answer" | "final") { return Ok(()); }
        let data: Value = serde_json::from_str(&raw).map_err(|_| CompanionError::Format)?;
        api_error(&data)?;
        match protocol {
            Protocol::AstrBot => self.text.push_str(&astrbot_text(&data, 0)?),
            Protocol::OpenAi => {
                let choice = data.pointer("/choices/0").unwrap_or(&Value::Null);
                let delta = &choice["delta"];
                if assistant(delta) {
                    let text = content_text(&delta["content"]);
                    if !text.is_empty() { self.deltas = true; self.text.push_str(&text); }
                }
                if !self.deltas && choice.get("message").is_some() { self.text = json_text(&data, protocol)?; }
                if choice.get("finish_reason").and_then(Value::as_str) == Some("content_filter") { return Err(CompanionError::Api); }
            }
            Protocol::Anthropic => {
                let kind = data.get("type").and_then(Value::as_str).unwrap_or(&event);
                match kind {
                    "content_block_start" if data["content_block"]["type"].as_str() == Some("text") => {
                        self.text.push_str(data["content_block"]["text"].as_str().unwrap_or(""));
                    }
                    "content_block_delta" if data["delta"]["type"].as_str() == Some("text_delta") => {
                        self.text.push_str(data["delta"]["text"].as_str().unwrap_or(""));
                    }
                    "message_stop" => self.done = true,
                    _ => {}
                }
            }
            Protocol::Responses => {
                let kind = data.get("type").and_then(Value::as_str).unwrap_or(&event);
                match kind {
                    "response.output_text.delta" => {
                        self.deltas = true;
                        self.text.push_str(data["delta"].as_str().unwrap_or(""));
                    }
                    "response.output_text.done" if !self.deltas => self.text.push_str(data["text"].as_str().unwrap_or("")),
                    "response.completed" => {
                        if !self.deltas && self.text.is_empty() { self.text = json_text(&data["response"], protocol)?; }
                        self.done = true;
                    }
                    "response.failed" => return Err(CompanionError::Api),
                    "response.incomplete" => return Err(CompanionError::Incomplete),
                    _ => {}
                }
            }
        }
        if self.text.len() > MAX_RESPONSE_BYTES { return Err(CompanionError::Size); }
        Ok(())
    }
}

/// 只按明确文本通道取值。支持 JSON 和跨 UTF-8/SSE 分块的完整响应。
pub fn parse_reply(raw: &[u8], content_type: &str, protocol: Protocol) -> Result<String, CompanionError> {
    if raw.len() > MAX_RESPONSE_BYTES { return Err(CompanionError::Size); }
    let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(raw);
    let text = std::str::from_utf8(raw).map_err(|_| CompanionError::Format)?;
    if text.trim().is_empty() { return Err(CompanionError::Empty); }
    let reply = if text.trim_start().starts_with('{') {
        json_text(&serde_json::from_str::<Value>(text).map_err(|_| CompanionError::Format)?, protocol)?
    } else if content_type.contains("text/event-stream") || text.lines().any(|line| line.starts_with("data:")) {
        let mut sse = Sse::default();
        sse.consume(raw, true, protocol)?;
        sse.text
    } else { return Err(CompanionError::Format); };
    let visible = without_thoughts(&reply, false);
    if visible.is_empty() { Err(CompanionError::Empty) } else { Ok(visible) }
}

pub(super) fn without_thoughts(text: &str, partial: bool) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut position = 0usize;
    while let Some(relative) = text[position..].find('<') {
        let start = position + relative;
        let Some(relative_end) = text[start..].find('>') else {
            let fragment = text[start + 1..].trim_start_matches('/').trim().to_ascii_lowercase();
            let thought = ["think", "thinking", "reasoning", "analysis"].iter()
                .any(|name| name.starts_with(&fragment) || fragment.starts_with(name));
            if depth == 0 { out.push_str(&text[position..if partial || thought { start } else { text.len() }]); }
            return out.trim().to_owned();
        };
        let end = start + relative_end + 1;
        let tag = text[start + 1..end - 1].trim();
        let closing = tag.starts_with('/');
        let tag = tag.trim_start_matches('/').trim();
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
        if matches!(name.as_str(), "think" | "thinking" | "reasoning" | "analysis") {
            if depth == 0 { out.push_str(&text[position..start]); }
            if closing {
                if depth == 0 { out.clear(); } else { depth -= 1; }
            } else { depth += 1; }
        } else if depth == 0 { out.push_str(&text[position..end]); }
        position = end;
    }
    if depth == 0 { out.push_str(&text[position..]); }
    out.trim().to_owned()
}

fn short_reply(text: &str) -> String {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).take(600).collect();
    let mut sentences = 0;
    let mut end = chars.len();
    for (i, c) in chars.iter().enumerate() {
        let period = *c == '.' && !(i > 0 && chars[i - 1].is_ascii_alphanumeric() && chars.get(i + 1).is_some_and(char::is_ascii_alphanumeric));
        if matches!(c, '。' | '！' | '？' | '!' | '?') || period {
            if chars.get(i + 1).is_some_and(|c| matches!(c, '。' | '！' | '？' | '!' | '?' | '.')) { continue; }
            sentences += 1;
            if sentences == 3 {
                end = i + 1;
                while chars.get(end).is_some_and(|c| matches!(c, '”' | '’' | '"' | '\'' | '」' | '』' | '）' | ')')) { end += 1; }
                break;
            }
        }
    }
    chars[..end].iter().collect::<String>().trim().to_owned()
}

fn sanitize(text: &str, connection: &Connection, user_prompt: &str, partial: bool) -> String {
    let mut visible = without_thoughts(text, partial);
    if partial {
        // 跨分片的 key/提示词前缀必须先扣住，不能先显示半个 secret 再补救。
        let mut cutoff = visible.len();
        for secret in connection.secrets.iter().map(String::as_str).chain([SYSTEM, user_prompt]) {
            if secret.is_empty() { continue; }
            for (index, _) in visible.char_indices() {
                let suffix = &visible[index..];
                if suffix.len() < secret.len() && secret.starts_with(suffix) { cutoff = cutoff.min(index); }
            }
        }
        visible.truncate(cutoff);
    }
    // 先移除完整提示词，再移除凭据，避免替换 key 后破坏提示词匹配。
    visible = visible.replace(SYSTEM, "").replace(user_prompt, "");
    for secret in &connection.secrets {
        if !secret.is_empty() { visible = visible.replace(secret, "[已隐藏]"); }
    }
    short_reply(&visible)
}

pub(super) fn sanitize_history(text: &str, connection: &Connection) -> String {
    let mut visible = without_thoughts(text, false);
    for secret in &connection.secrets {
        if !secret.is_empty() { visible = visible.replace(secret, "[已隐藏]"); }
    }
    short_reply(&visible)
}
