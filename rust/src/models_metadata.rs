use super::*;
use regex::RegexBuilder;
use serde_json::json;

pub fn load_model_rules(path: &Path) -> Result<Value, String> {
    config::read_optional(path)?.map(|d| rules_from_catalog(&d.value)).ok_or_else(|| "未找到模型规则目录。".into())
}
pub(super) fn rules_from_catalog(catalog: &Value) -> Value {
    if let Some(rules) = catalog.pointer("/config/modelConfigRules").filter(|v| v.is_object()) { return rules.clone(); }
    // 兼容旧 models_catalog：精确 provider/model 范围，不把别家同名模型混入。
    let mut entries = Vec::new();
    if let Some(providers) = catalog.get("providers").and_then(Value::as_array) {
        for provider in providers {
            let pid = provider.get("id").or_else(|| provider.get("providerId")).and_then(Value::as_str).unwrap_or("");
            if !config::valid_id(pid, &[]) { continue; }
            if let Some(models) = provider.get("models").and_then(Value::as_array) {
                for model in models {
                    let id = string(&model["id"]);
                    if config::valid_id(id, &[]) { entries.push(json!({"providerId":pid,"modelId":id,"config":{"properties":model}})); }
                }
            }
        }
    }
    json!({"builtinProviderModelRules": entries})
}
pub(super) fn deep_merge(target: &mut Value, incoming: &Value) {
    let Some(src) = incoming.as_object() else { *target = incoming.clone(); return; };
    if !target.is_object() { *target = json!({}); }
    let Some(dst) = target.as_object_mut() else { return; };
    for (key, value) in src {
        if value.is_object() { deep_merge(dst.entry(key).or_insert(Value::Null), value); }
        else { dst.insert(key.clone(), value.clone()); }
    }
}
fn generic(pattern: &str) -> bool {
    matches!(pattern, ".*" | ".+" | "^.*$" | "^.+$" | "^.*" | "^.+" | ".*$" | ".+$" | ".*?" | ".+?" | "^.*?$" | "^.+?$")
}
fn regex_matches(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() || value.is_empty() || pattern.len() > 8192 { return false; }
    RegexBuilder::new(&format!("\\A(?:{pattern})\\z")).case_insensitive(true)
        .size_limit(1024 * 1024).dfa_size_limit(1024 * 1024).build().is_ok_and(|re| re.is_match(value))
}
fn matches(rule: &Value, id: &str, api: &str, base: &str, pid: &str, builtin: bool) -> bool {
    for (key, value) in [("providerId", pid), ("modelId", id)] {
        if builtin || rule.get(key).is_some() {
            let Some(expected) = rule.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) else { return false; };
            if value.is_empty() { return false; }
            if key == "providerId" {
                if config::normalized_provider(expected) != config::normalized_provider(value) { return false; }
            } else if expected.to_lowercase() != value.to_lowercase() { return false; }
        }
    }
    for (key, value) in [("modelMatch", id), ("apiTypeMatch", api), ("baseUrlMatch", base)] {
        if let Some(pattern) = rule.get(key) {
            if !pattern.as_str().is_some_and(|s| regex_matches(s, value)) { return false; }
        }
    }
    if !builtin && generic(rule.get("modelMatch").and_then(Value::as_str).unwrap_or(".*")) {
        let site_scoped = rule.get("baseUrlMatch").and_then(Value::as_str).is_some_and(|s| !s.is_empty() && !generic(s));
        if !site_scoped && rule.get("providerId").is_none() { return false; }
    }
    true
}
fn matching_fields(id: &str, api: &str, base: &str, pid: &str, rules: &Value) -> (Value, Value) {
    let mut props = json!({}); let mut specs = json!({});
    for group in ["modelRules", "modelApiRules", "providerSiteRules", "builtinProviderModelRules"] {
        let Some(list) = rules.get(group).and_then(Value::as_array) else { continue; };
        for rule in list.iter().take(20_000) {
            if matches(rule, id, api, base, pid, group == "builtinProviderModelRules") {
                if rule["config"]["properties"].is_object() { deep_merge(&mut props, &rule["config"]["properties"]); }
                if rule["config"]["optionSpecs"].is_object() { deep_merge(&mut specs, &rule["config"]["optionSpecs"]); }
            }
        }
    }
    (props, specs)
}
fn flag(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| value.get("supported").and_then(Value::as_bool))
        .or_else(|| value.get("enabled").and_then(Value::as_bool))
}
fn modality_flags(value: &Value) -> Vec<(&'static str, bool)> {
    if let Some(list) = value.as_array() {
        if list.iter().any(|v| !v.is_string()) { return Vec::new(); }
        let names: Vec<_> = list.iter().filter_map(Value::as_str).map(str::to_lowercase).collect();
        if !names.is_empty() && !names.iter().any(|s| MODALITIES.contains(&s.as_str())) { return Vec::new(); }
        return MODALITIES.into_iter().map(|name| (name, names.iter().any(|s| s == name))).collect();
    }
    let mut out = Vec::new();
    for (name, key) in [("text", "supportsText"), ("image", "supportsImage"), ("video", "supportsVideo"), ("audio", "supportsAudio"), ("pdf", "supportsPdf")] {
        if let Some(v) = value.get(key).or_else(|| value.get(name)).and_then(Value::as_bool) { out.push((name, v)); }
    }
    out
}
fn set_bool(c: &mut Capabilities, key: &str, value: bool, source: &str) {
    if let Some(name) = key.strip_prefix("input.") { c.input.set(name, value); }
    else if let Some(name) = key.strip_prefix("output.") { c.output.set(name, value); }
    else { match key { "tools" => c.tools = Some(value), "reasoning" => c.reasoning = Some(value),
        "structured_output" => c.structured_output = Some(value), "web_search" => c.web_search = Some(value), _ => return } }
    c.sources.insert(key.into(), source.into());
}
fn apply_metadata(c: &mut Capabilities, m: &Value, source: &str) {
    for direction in ["input", "output"] {
        for v in [&m[format!("{direction}Format")], &m[format!("{direction}_format")],
            &m["architecture"][format!("{direction}_modalities")], &m["modalities"][direction], &m[format!("{direction}_modalities")]] {
            for (name, value) in modality_flags(v) { set_bool(c, &format!("{direction}.{name}"), value, source); }
        }
    }
    for (key, fields) in [
        ("context_window", vec![&m["contextWindow"], &m["limit"]["context"], &m["context_length"], &m["context_window"]]),
        ("output_limit", vec![&m["maxOutputTokens"], &m["limit"]["output"], &m["max_output_tokens"]]),
    ] {
        for v in fields {
            if let Some(n) = positive_int(v) {
                if key == "context_window" { c.context_window = Some(n); } else { c.output_limit = Some(n); }
                c.sources.insert(key.into(), source.into());
            }
        }
    }
    for (key, aliases) in [
        ("tools", vec!["supportsToolCall", "supports_tool_call", "supports_tools", "tool_calling", "tools"]),
        ("reasoning", vec!["supports_reasoning", "reasoning", "thinking"]),
        ("structured_output", vec!["supportsJsonSchemaOutput", "supports_structured_output", "structured_output"]),
        ("web_search", vec!["supportsNativeWebSearch", "supports_web_search", "web_search"]),
    ] {
        for holder in [m, &m["capabilities"]] {
            for alias in &aliases { if let Some(v) = flag(&holder[*alias]) { set_bool(c, key, v, source); } }
        }
    }
    if let Some(v) = flag(&m["capabilities"]["vision"]) { set_bool(c, "input.image", v, source); }
    for direction in ["input", "output"] {
        for name in MODALITIES {
            if let Some(v) = flag(&m["capabilities"][format!("{name}_{direction}")]) { set_bool(c, &format!("{direction}.{name}"), v, source); }
        }
    }
    if let Some(params) = m["supported_parameters"].as_array() {
        for (param, field) in [("tools", "tools"), ("reasoning", "reasoning"), ("reasoning_effort", "reasoning"), ("structured_outputs", "structured_output")] {
            if params.iter().any(|v| v.as_str() == Some(param)) { set_bool(c, field, true, source); }
        }
    }
}
/// 仅合并明确字段；优先级 目录规则 < 用户配置 < API 声明 < 实测（RichProbeOutcome::apply）。
pub fn merge_capabilities(provider_id: &str, model_id: &str, api_type: &str, base: &str,
    configured: &Value, api_metadata: &Value, rules: &Value) -> Capabilities {
    let (mut props, specs) = matching_fields(model_id, api_type, base, provider_id, rules);
    if positive_int(&props["maxOutputTokens"]).is_none() {
        if let Some(max) = positive_int(&specs["maxOutputTokens"]["max"]) { props["maxOutputTokens"] = json!(max); }
    }
    let mut out = Capabilities::default();
    apply_metadata(&mut out, &props, "ZCode rules");
    apply_metadata(&mut out, configured, "ZCode config");
    apply_metadata(&mut out, &configured["properties"], "ZCode config");
    if let Some(max) = positive_int(&configured["optionSpecs"]["maxOutputTokens"]["max"]) {
        out.output_limit = Some(max); out.sources.insert("output_limit".into(), "ZCode config".into());
    }
    apply_metadata(&mut out, api_metadata, "provider API"); out
}
pub(super) fn make_row(record: &config::ProviderRecord, rules: &Value, id: &str, meta: &Value) -> RichModelRow {
    let (base, api) = http::Endpoint::for_record(record).map(|e| (e.base, e.api.as_str().to_owned())).unwrap_or_default();
    RichModelRow { row: ModelRow { provider: record.info.name.clone(), provider_id: record.info.id.clone(), id: id.into() },
        capabilities: merge_capabilities(&record.info.id, id, &api, &base, &record.models[id], meta, rules) }
}
pub(super) fn local_models(record: &config::ProviderRecord, rules: &Value) -> (Vec<RichModelRow>, bool) {
    let secrets = record.secrets();
    let mut ids = record.local_ids.clone();
    if ids.is_empty() {
        if let Some(list) = rules["builtinProviderModelRules"].as_array() {
            for rule in list {
                if config::normalized_provider(string(&rule["providerId"])) == config::normalized_provider(&record.info.id) {
                    ids.push(string(&rule["modelId"]).to_owned());
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| config::valid_id(id, &secrets) && seen.insert(id.clone()));
    let truncated = ids.len() > MAX_MODELS;
    (ids.into_iter().take(MAX_MODELS).map(|id| make_row(record, rules, &id, &Value::Null)).collect(), truncated)
}
pub fn capability_lines(caps: &Capabilities) -> Vec<String> {
    fn source(c: &Capabilities, key: &str) -> String {
        c.sources.get(key).map(|s| format!(" · {}", match s.as_str() {
            "provider API" => "API 声明", "ZCode config" => "ZCode 配置", "ZCode rules" => "ZCode 规则", "实测探针" => "实测探针", _ => "外部声明",
        })).unwrap_or_default()
    }
    fn state(v: Option<bool>) -> &'static str { match v { Some(true) => "支持", Some(false) => "不支持", None => "未知" } }
    let mut lines = Vec::new();
    for (direction, title, flags) in [("input", "输入", &caps.input), ("output", "输出", &caps.output)] {
        for (name, label) in [("text", "文本"), ("image", "图片"), ("video", "视频"), ("audio", "音频"), ("pdf", "PDF")] {
            lines.push(format!("{title}{label}：{}{}", state(flags.get(name)), source(caps, &format!("{direction}.{name}"))));
        }
    }
    for (key, title, value) in [("context_window", "上下文窗口", caps.context_window), ("output_limit", "最大输出", caps.output_limit)] {
        lines.push(format!("{title}：{}{}", value.map(|v| format!("{v} tokens")).unwrap_or_else(|| "未知".into()), source(caps, key)));
    }
    for (key, title, value) in [("tools", "工具调用", caps.tools), ("reasoning", "推理", caps.reasoning),
        ("structured_output", "结构化输出", caps.structured_output), ("web_search", "原生搜索", caps.web_search)] {
        lines.push(format!("{title}：{}{}", state(value), source(caps, key)));
    }
    lines
}

pub(super) fn personal_entry(c: &Capabilities) -> Value {
    let mut props = json!({}); let mut format = json!({});
    for (name, key) in [("text", "supportsText"), ("image", "supportsImage"), ("video", "supportsVideo"), ("audio", "supportsAudio"), ("pdf", "supportsPdf")] {
        if let Some(v) = c.input.get(name) { format[key] = json!(v); }
    }
    if format.as_object().is_some_and(|o| !o.is_empty()) { props["inputFormat"] = format; }
    // 当前客户端 outputFormat strict schema 仅允许 supportsText。
    if let Some(v) = c.output.text { props["outputFormat"] = json!({"supportsText":v}); }
    for (key, value) in [("supportsToolCall", c.tools), ("supportsJsonSchemaOutput", c.structured_output), ("supportsNativeWebSearch", c.web_search)] {
        if let Some(v) = value { props[key] = json!(v); }
    }
    if let Some(v) = c.context_window.filter(|v| *v > 0 && *v < 100_000_000) { props["contextWindow"] = json!(v); }
    let mut entry = json!({"enabled":true});
    if props.as_object().is_some_and(|o| !o.is_empty()) { entry["properties"] = props; }
    if let Some(v) = c.output_limit.filter(|v| *v > 0 && *v < 100_000_000) { entry["optionSpecs"] = json!({"maxOutputTokens":{"max":v}}); }
    entry
}
pub(super) fn account_entry(c: &Capabilities) -> Value {
    let mut out = json!({}); let mut limit = json!({});
    if let Some(v) = c.context_window.filter(|v| *v > 0 && *v < 100_000_000) { limit["context"] = json!(v); }
    if let Some(v) = c.output_limit.filter(|v| *v > 0 && *v < 100_000_000) { limit["output"] = json!(v); }
    if limit.as_object().is_some_and(|o| !o.is_empty()) { out["limit"] = limit; }
    if MODALITIES.iter().all(|name| c.input.get(name).is_some() && c.output.get(name).is_some()) {
        out["modalities"] = json!({"input":MODALITIES.iter().filter(|n| c.input.get(n) == Some(true)).collect::<Vec<_>>(),
            "output":MODALITIES.iter().filter(|n| c.output.get(n) == Some(true)).collect::<Vec<_>>()});
        out["zcode"] = json!({"modalitiesConfigured":true});
    }
    if c.reasoning == Some(true) { out["reasoning"] = json!({"enabled":true}); }
    out
}
