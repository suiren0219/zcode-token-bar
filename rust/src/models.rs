//! 原生模型发现、声明合并、能力探针和保守导入。
//!
//! 同步入口须在后台线程调用。`ModelsService` 固定配置路径，provider 始终以 ID
//! 选择；界面应保存同一批 `providers()` 结果，不能在点击时重新按索引枚举。
//! 所有公开结果只含清洗后的名称、ID、能力、固定错误与文件路径，不含认证信息。
//! 配置读回只能证明条目仍在磁盘，不能证明运行中的客户端已接受它。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::time::{Duration, Instant};

#[path = "models_config.rs"] mod config;
#[path = "models_metadata.rs"] mod metadata;
#[path = "models_http.rs"] mod http;
#[path = "models_import.rs"] mod importing;
#[cfg(test)] #[path = "models_tests.rs"] mod tests;

pub use config::ConfigPaths;
pub use metadata::{capability_lines, load_model_rules, merge_capabilities};
pub use importing::{FileImport, ImportReport, Verification};

pub const MAX_MODELS: usize = 200;
pub const MAX_PAGES: usize = 5;
pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub const MAX_PROBE_MODELS: usize = 12;
pub const MODALITIES: [&str; 5] = ["text", "image", "video", "audio", "pdf"];

// 保持旧 struct literal 兼容；丰富字段放进单独的 Rich 类型。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderInfo { pub id: String, pub name: String, pub kind: String }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelRow { pub provider: String, pub provider_id: String, pub id: String }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub provider: String, pub provider_id: String, pub rows: Vec<ModelRow>, pub source_note: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportResult {
    pub added: usize, pub skipped: Vec<String>, pub missing: Vec<String>,
    pub added_ids: Vec<(String, String)>, pub backup: String, pub error: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProbeOutcome { pub model_id: String, pub usable: Option<bool>, pub error: String }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Modalities {
    pub text: Option<bool>, pub image: Option<bool>, pub video: Option<bool>,
    pub audio: Option<bool>, pub pdf: Option<bool>,
}
impl Modalities {
    pub fn get(&self, name: &str) -> Option<bool> {
        match name { "text" => self.text, "image" => self.image, "video" => self.video,
            "audio" => self.audio, "pdf" => self.pdf, _ => None }
    }
    pub(crate) fn set(&mut self, name: &str, value: bool) {
        match name { "text" => self.text = Some(value), "image" => self.image = Some(value),
            "video" => self.video = Some(value), "audio" => self.audio = Some(value),
            "pdf" => self.pdf = Some(value), _ => {} }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Capabilities {
    pub input: Modalities, pub output: Modalities,
    pub context_window: Option<u64>, pub output_limit: Option<u64>,
    pub tools: Option<bool>, pub reasoning: Option<bool>,
    pub structured_output: Option<bool>, pub web_search: Option<bool>,
    /// 字段名如 input.image；值为 provider API / ZCode config / ZCode rules / 实测探针。
    pub sources: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichModelRow {
    #[serde(flatten)] pub row: ModelRow,
    pub capabilities: Capabilities,
}
impl From<ModelRow> for RichModelRow {
    fn from(row: ModelRow) -> Self { Self { row, capabilities: Capabilities::default() } }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichDiscovery {
    pub provider: String, pub provider_id: String, pub rows: Vec<RichModelRow>,
    pub source_note: String, pub truncated: bool,
}
impl RichDiscovery {
    pub fn legacy(&self) -> Discovery {
        Discovery { provider: self.provider.clone(), provider_id: self.provider_id.clone(),
            rows: self.rows.iter().map(|r| r.row.clone()).collect(), source_note: self.source_note.clone() }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichProbeOutcome {
    pub provider_id: String,
    pub model_id: String, pub usable: Option<bool>, pub tools: Option<bool>,
    pub image: Option<bool>, pub pdf: Option<bool>, pub error: String,
    pub errors: BTreeMap<String, String>, pub cancelled: bool, pub timed_out: bool,
    /// 实际发出的请求数；文本失败时不再发送能力探针。
    pub requests: usize,
}
impl RichProbeOutcome {
    pub fn apply(&self, row: &mut RichModelRow) {
        if self.usable != Some(true) { return; }
        let c = &mut row.capabilities;
        c.input.text = Some(true);
        c.sources.insert("input.text".into(), "实测探针".into());
        for (key, value) in [("tools", self.tools), ("input.image", self.image), ("input.pdf", self.pdf)] {
            if let Some(v) = value {
                match key { "tools" => c.tools = Some(v), "input.image" => c.input.image = Some(v),
                    _ => c.input.pdf = Some(v) }
                c.sources.insert(key.into(), "实测探针".into());
            }
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProbeBatch {
    pub outcomes: Vec<RichProbeOutcome>, pub rows: Vec<RichModelRow>,
    pub probed: usize, pub total: usize, pub truncated: bool,
    pub cancelled: bool, pub timed_out: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProbeImportReport { pub probe: ProbeBatch, pub import: ImportReport }
#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoveryBatch {
    pub providers: Vec<RichDiscovery>, pub errors: BTreeMap<String, String>,
    pub cancelled: bool, pub timed_out: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub provider_id: String, pub model_id: String, pub stage: String,
    pub completed: usize, pub total: usize,
}
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn cancel(&self) { self.0.store(true, Ordering::Release); }
    pub fn is_cancelled(&self) -> bool { self.0.load(Ordering::Acquire) }
}
/// 一次任务共享同一个 Operation，发现/批量探针不会逐模型重置总时限。
#[derive(Clone)]
pub struct Operation {
    pub cancel: CancelToken,
    deadline: Instant,
    progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
}
impl Operation {
    pub fn new(timeout: Duration) -> Self {
        Self { cancel: CancelToken::default(), deadline: Instant::now() + timeout.min(Duration::from_secs(3600)), progress: None }
    }
    pub fn discovery() -> Self { Self::new(Duration::from_secs(20)) }
    pub fn probe() -> Self { Self::new(Duration::from_secs(240)) }
    pub fn with_progress(mut self, callback: impl Fn(Progress) + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(callback)); self
    }
    pub fn is_timed_out(&self) -> bool { Instant::now() >= self.deadline }
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() { Err("操作已取消。".into()) }
        else if self.is_timed_out() { Err("操作总时限已到，剩余项未处理。".into()) } else { Ok(()) }
    }
    pub(crate) fn remaining(&self) -> Duration { self.deadline.saturating_duration_since(Instant::now()) }
    pub(crate) fn emit(&self, pid: &str, mid: &str, stage: &str, completed: usize, total: usize) {
        if let Some(cb) = &self.progress {
            // 界面回调不应影响请求/写入的资源清理。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(Progress {
                provider_id: pid.into(), model_id: mid.into(), stage: stage.into(), completed, total,
            })));
        }
    }
    pub(crate) fn wait(&self, duration: Duration) -> Result<(), String> {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.check()?;
            std::thread::sleep(Duration::from_millis(25).min(end.saturating_duration_since(Instant::now())).min(self.remaining()));
        }
        self.check()
    }
}

/// 服务仅保存定位路径，不缓存任何凭据；fixture 可显式传入隔离路径。
#[derive(Clone, Debug)]
pub struct ModelsService { pub paths: ConfigPaths }
impl ModelsService {
    pub fn from_env() -> Result<Self, String> { Ok(Self::new(ConfigPaths::from_env()?)) }
    pub fn new(paths: ConfigPaths) -> Self { Self { paths } }
    pub fn providers(&self) -> Result<Vec<ProviderInfo>, String> {
        Ok(config::load_records(&self.paths)?.into_iter().map(|r| r.info).collect())
    }
    pub fn discover(&self, provider_id: &str, operation: &Operation) -> Result<RichDiscovery, String> {
        operation.check()?;
        let _guard = http::ActiveGuard::acquire(&self.paths, provider_id)?;
        let record = config::find_record(&self.paths, provider_id)?;
        let rules = config::rules_for(&self.paths)?;
        http::discover(&record, &rules, operation)
    }
    /// 顺序、总时限有界；一个 provider 失败不会抹掉其它成功结果。
    pub fn discover_all(&self, ids: &[String], operation: &Operation) -> DiscoveryBatch {
        let mut out = DiscoveryBatch::default();
        let mut seen = std::collections::HashSet::new();
        for (i, id) in ids.iter().take(MAX_MODELS).enumerate() {
            if !seen.insert(id) { continue; }
            if operation.check().is_err() { break; }
            match self.discover(id, operation) {
                Ok(result) => out.providers.push(result),
                Err(error) => { out.errors.insert(config::safe_identifier(id), error); }
            }
            operation.emit(&config::safe_identifier(id), "", "provider", i + 1, ids.len().min(MAX_MODELS));
        }
        out.cancelled = operation.cancel.is_cancelled(); out.timed_out = operation.is_timed_out(); out
    }
    pub fn manual_rows(&self, provider_id: &str, input: &str) -> Result<Vec<RichModelRow>, String> {
        let record = config::find_record(&self.paths, provider_id)?;
        let rules = config::rules_for(&self.paths)?;
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for id in input.split(|c: char| c.is_whitespace() || ",，;；、".contains(c)) {
            if !config::valid_id(id, &record.secrets()) || !seen.insert(id) { continue; }
            rows.push(metadata::make_row(&record, &rules, id, &Value::Null));
            if rows.len() >= MAX_MODELS { break; }
        }
        if rows.is_empty() { return Err("没有可用的模型 ID。".into()); }
        Ok(rows)
    }
    pub fn probe_model(&self, provider_id: &str, model_id: &str, operation: &Operation) -> RichProbeOutcome {
        let run = || -> Result<RichProbeOutcome, String> {
            operation.check()?;
            let _guard = http::ActiveGuard::acquire(&self.paths, provider_id)?;
            let record = config::find_record(&self.paths, provider_id)?;
            http::probe(&record, model_id, operation)
        };
        run().unwrap_or_else(|error| RichProbeOutcome { provider_id: config::safe_identifier(provider_id), model_id: "[未完成探针]".into(), error,
            cancelled: operation.cancel.is_cancelled(), timed_out: operation.is_timed_out(), ..Default::default() })
    }
    pub fn probe_rows(&self, provider_id: &str, rows: &[RichModelRow], operation: &Operation) -> Result<ProbeBatch, String> {
        operation.check()?;
        let _guard = http::ActiveGuard::acquire(&self.paths, provider_id)?;
        let record = config::find_record(&self.paths, provider_id)?;
        let secrets = record.secrets();
        let mut selected = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            if row.row.provider_id != provider_id || !config::valid_id(&row.row.id, &secrets) {
                return Err("模型条目与目标 provider 不匹配，或 ID 无效。".into());
            }
            if seen.insert(&row.row.id) { selected.push(row); }
        }
        let mut out = ProbeBatch { total: selected.len().min(MAX_PROBE_MODELS), truncated: selected.len() > MAX_PROBE_MODELS, ..Default::default() };
        for row in selected.into_iter().take(MAX_PROBE_MODELS) {
            if operation.check().is_err() { break; }
            let result = http::probe(&record, &row.row.id, operation)?;
            let abort = result.cancelled || result.timed_out;
            if result.usable == Some(true) && !abort {
                let mut row = row.clone(); result.apply(&mut row); out.rows.push(row);
            }
            out.probed += 1;
            out.outcomes.push(result);
            operation.emit(provider_id, &row.row.id, "model", out.probed, out.total);
            if abort { break; }
        }
        out.cancelled = operation.cancel.is_cancelled(); out.timed_out = operation.is_timed_out();
        Ok(out)
    }
    /// 仅 usable == Some(true) 的行会导入；取消/总超时后整批不写。
    pub fn probe_and_import(&self, provider_id: &str, rows: &[RichModelRow], operation: &Operation) -> Result<ProbeImportReport, String> {
        self.probe_all_and_import(&[(provider_id.to_owned(), rows.to_vec())], operation)
    }
    /// 跨 provider 共用 12 个模型上限与总时限；所有探针完成后才统一写入。
    pub fn probe_all_and_import(&self, groups: &[(String, Vec<RichModelRow>)], operation: &Operation) -> Result<ProbeImportReport, String> {
        if operation.check().is_err() {
            return Ok(ProbeImportReport { probe: ProbeBatch { cancelled: operation.cancel.is_cancelled(), timed_out: operation.is_timed_out(), ..Default::default() }, ..Default::default() });
        }
        let records = config::load_records(&self.paths)?;
        let mut selected = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut identities = Vec::new();
        for (pid, rows) in groups {
            if rows.is_empty() { continue; }
            let record = records.iter().find(|r| r.info.id == *pid).ok_or("所选 provider 已移除或停用。")?;
            let secrets = record.secrets();
            if !identities.iter().any(|(id, _)| id == pid) { identities.push((pid.clone(), record.identity.clone())); }
            for row in rows {
                if row.row.provider_id != *pid || !config::valid_id(&row.row.id, &secrets) { return Err("模型条目与目标 provider 不匹配，或 ID 无效。".into()); }
                if seen.insert((pid.clone(), row.row.id.clone())) { selected.push((record, row)); }
            }
        }
        let mut probe = ProbeBatch { total: selected.len().min(MAX_PROBE_MODELS), truncated: selected.len() > MAX_PROBE_MODELS, ..Default::default() };
        for (record, row) in selected.into_iter().take(MAX_PROBE_MODELS) {
            if operation.check().is_err() { break; }
            let result = match http::ActiveGuard::acquire(&self.paths, &record.info.id) {
                Ok(_guard) => http::probe(record, &row.row.id, operation),
                Err(e) => Err(e),
            }.unwrap_or_else(|error| RichProbeOutcome { provider_id: record.info.id.clone(), model_id: row.row.id.clone(), error, cancelled: operation.cancel.is_cancelled(), timed_out: operation.is_timed_out(), ..Default::default() });
            let abort = result.cancelled || result.timed_out;
            if result.usable == Some(true) && !abort {
                let mut row = row.clone(); result.apply(&mut row); probe.rows.push(row);
            }
            probe.probed += 1; probe.outcomes.push(result);
            operation.emit(&record.info.id, &row.row.id, "model", probe.probed, probe.total);
            if abort { break; }
        }
        probe.cancelled = operation.cancel.is_cancelled(); probe.timed_out = operation.is_timed_out();
        let import = if probe.cancelled || probe.timed_out || probe.rows.is_empty() { ImportReport::default() }
        else {
            let mut groups: Vec<(String, Vec<RichModelRow>)> = Vec::new();
            for row in &probe.rows {
                match groups.iter_mut().find(|(pid, _)| *pid == row.row.provider_id) {
                    Some((_, rows)) => rows.push(row.clone()),
                    None => groups.push((row.row.provider_id.clone(), vec![row.clone()])),
                }
            }
            // 在文件锁与快照校验范围内再次比较全部 provider 配置，防止旧探针结果写入新地址。
            importing::import_checked(self, &groups, operation, &identities)
        };
        Ok(ProbeImportReport { probe, import })
    }
    /// 不发探针的显式导入；发现/手动输入后由用户选择导入时调用。
    pub fn import_rows(&self, groups: &[(String, Vec<RichModelRow>)], operation: &Operation) -> ImportReport {
        importing::import(self, groups, operation)
    }
    /// 默认等待 1.2 秒再读回；runtime_accepted 永远为 None（未取得客户端确认）。
    pub fn verify(&self, report: &ImportReport, operation: &Operation) -> Verification {
        importing::verify_report(report, Duration::from_millis(1200), operation)
    }
    pub fn verify_after(&self, report: &ImportReport, delay: Duration, operation: &Operation) -> Verification {
        importing::verify_report(report, delay, operation)
    }
}

// 兼容 API：保持原签名，错误仅使用固定文本；默认新任务各有独立总时限。
pub fn configured_providers() -> Vec<ProviderInfo> {
    ModelsService::from_env().and_then(|s| s.providers()).unwrap_or_default()
}
pub fn configured_providers_checked() -> Result<Vec<ProviderInfo>, String> { ModelsService::from_env()?.providers() }
pub fn discover_provider(pid: &str) -> Result<Discovery, String> {
    Ok(ModelsService::from_env()?.discover(pid, &Operation::discovery())?.legacy())
}
pub fn probe_model(pid: &str, model_id: &str) -> ProbeOutcome {
    let rich = match ModelsService::from_env() {
        Ok(s) => s.probe_model(pid, model_id, &Operation::probe()),
        Err(error) => RichProbeOutcome { provider_id: config::safe_identifier(pid), error, model_id: "[未完成探针]".into(), ..Default::default() },
    };
    ProbeOutcome { model_id: rich.model_id, usable: rich.usable, error: rich.error }
}
pub fn probe_models(pid: &str, ids: &[String], progress: impl Fn(usize, usize)) -> Vec<ProbeOutcome> {
    let op = Operation::probe();
    let service = ModelsService::from_env();
    let total = ids.len().min(MAX_PROBE_MODELS);
    let mut out = Vec::new();
    for (i, id) in ids.iter().take(total).enumerate() {
        if op.check().is_err() { break; }
        let r = match &service {
            Ok(s) => s.probe_model(pid, id, &op),
            Err(e) => RichProbeOutcome { provider_id: config::safe_identifier(pid), model_id: "[未完成探针]".into(), error: e.clone(), ..Default::default() },
        };
        out.push(ProbeOutcome { model_id: r.model_id, usable: r.usable, error: r.error }); progress(i + 1, total);
    }
    out
}
pub fn import_groups(groups: &[(String, Vec<ModelRow>)]) -> ImportResult {
    let service = match ModelsService::from_env() { Ok(s) => s, Err(error) => return ImportResult { error, ..Default::default() } };
    let rich: Vec<_> = groups.iter().map(|(pid, rows)| (pid.clone(), rows.iter().cloned().map(RichModelRow::from).collect())).collect();
    service.import_rows(&rich, &Operation::new(Duration::from_secs(30))).result
}
/// 只检查延迟读回后的磁盘条目，不代表客户端热重载成功。
pub fn verify_import_applied(added_ids: &[(String, String)]) -> bool {
    if added_ids.is_empty() { return true; }
    let Ok(s) = ModelsService::from_env() else { return false; };
    importing::verify_ids(&s.paths, added_ids, &Operation::new(Duration::from_secs(10)))
}

pub(crate) fn positive_int(v: &Value) -> Option<u64> { v.as_u64().filter(|n| *n > 0 && *n < 100_000_000) }
pub(crate) fn object(v: &Value) -> serde_json::Map<String, Value> { v.as_object().cloned().unwrap_or_default() }
pub(crate) fn string(v: &Value) -> &str { v.as_str().unwrap_or("") }
