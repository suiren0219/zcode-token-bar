//! 小亚陪伴后端。Slint 主线程仅传最小轮状态、发送用户动作并轮询 `drain()`。
//! 构造/配置/候选列表不会发网络请求；默认关闭。无 Python、子进程或工作正文读取。
//! 依赖 serde/serde_json/chrono/reqwest 与 tokio(rt,time,sync,macros)。

#[path = "companion_config.rs"]
mod config;
#[path = "companion_http.rs"]
mod http;
#[path = "companion_watch.rs"]
mod watch;

pub use config::{connection_candidates, load_connection, migrate_settings, select_connection, Connection, ConnectionInfo, ModelCandidate};
pub use http::parse_reply;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

const MAX_RESULTS: usize = 64;
const MAX_HISTORY: usize = 100;
const HISTORY_FORMAT: &str = "zcode-companion-replies-v1";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol { OpenAi, Anthropic, Responses, AstrBot }

/// 普通 settings 只保存开关、标识和文件路径，不含凭据、人格提示词或工作正文。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CompanionSettings {
    pub enabled: bool,
    pub auto: bool,
    pub paused: bool,
    #[serde(alias = "config", alias = "companion_config")]
    pub config_path: String,
    pub provider_id: String,
    pub model_id: String,
    #[serde(alias = "cooldown_s")]
    pub cooldown_seconds: u64,
    pub hourly_limit: u32,
    pub timeout_seconds: u64,
    pub stream: bool,
    pub max_pending: usize,
    pub max_output_tokens: u32,
    /// 默认在用户启用后的成功轮结束触发；false 可恢复仅长轮/错误阈值模式。
    pub end_of_turn: bool,
    pub long_turn_seconds: u64,
    pub error_threshold: u32,
    /// 旧 Python 模式可在 running 状态跨过错误阈值时触发；默认只在轮结束。
    pub trigger_running_errors: bool,
    pub event_ttl_seconds: u64,
    pub persist_history: bool,
    /// 只有显式配置路径并开启 persist_history 才读写；仅存小亚回复。
    pub history_path: String,
}

impl Default for CompanionSettings {
    fn default() -> Self {
        Self { enabled: false, auto: true, paused: false, config_path: String::new(),
            provider_id: String::new(), model_id: String::new(), cooldown_seconds: 300,
            hourly_limit: 6, timeout_seconds: 30, stream: false, max_pending: 4,
            max_output_tokens: 512, end_of_turn: true, long_turn_seconds: 120,
            error_threshold: 3, trigger_running_errors: false, event_ttl_seconds: 120,
            persist_history: false, history_path: String::new() }
    }
}

impl CompanionSettings {
    pub fn normalized(mut self) -> Self {
        self.cooldown_seconds = self.cooldown_seconds.min(86_400);
        self.hourly_limit = self.hourly_limit.min(120);
        self.timeout_seconds = self.timeout_seconds.clamp(1, 180);
        self.max_pending = self.max_pending.clamp(1, 16);
        self.max_output_tokens = self.max_output_tokens.clamp(64, 4096);
        self.long_turn_seconds = self.long_turn_seconds.min(86_400);
        self.error_threshold = self.error_threshold.clamp(1, 1000);
        self.event_ttl_seconds = self.event_ttl_seconds.clamp(1, 300);
        self
    }
}

/// 只带固定安全类别。Display/Debug 不会携带 URL、凭据、提示词或服务响应。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompanionError {
    Unconfigured, Config, Selection, Disabled, Paused, Closed, Auth, RateLimit, Server,
    Request, Redirect, Timeout, Network, Tls, Format, Empty, Size, Api, Incomplete,
    Message, Event, Expired, Busy, Cooldown, HourlyLimit, Backoff, Cancelled, History, Internal,
}

impl CompanionError {
    pub fn category(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured", Self::Config => "config", Self::Selection => "selection",
            Self::Disabled => "disabled", Self::Paused => "paused", Self::Closed => "closed",
            Self::Auth => "auth", Self::RateLimit => "rate_limit", Self::Server => "server",
            Self::Request => "request", Self::Redirect => "redirect", Self::Timeout => "timeout",
            Self::Network => "network", Self::Tls => "tls", Self::Format => "format", Self::Empty => "empty",
            Self::Size => "size", Self::Api => "api", Self::Incomplete => "incomplete", Self::Message => "message",
            Self::Event => "event", Self::Expired => "expired", Self::Busy => "busy", Self::Cooldown => "cooldown",
            Self::HourlyLimit => "hourly_limit", Self::Backoff => "backoff", Self::Cancelled => "cancelled",
            Self::History => "history", Self::Internal => "internal",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::Unconfigured => "请选择本地陪伴连接文件（unconfigured）",
            Self::Config => "陪伴连接文件不完整或无效（config）",
            Self::Selection => "请选择有效的陪伴 provider 和模型（selection）",
            Self::Disabled => "陪伴已关闭（disabled）", Self::Paused => "陪伴已暂停（paused）",
            Self::Closed => "陪伴服务已停止（closed）", Self::Auth => "陪伴服务认证失败（auth）",
            Self::RateLimit => "陪伴请求过于频繁，请稍后再试（rate_limit）",
            Self::Server => "陪伴服务暂时不可用（server）", Self::Request => "陪伴服务拒绝请求（request）",
            Self::Redirect => "已阻止陪伴服务重定向（redirect）", Self::Timeout => "陪伴请求超时（timeout）",
            Self::Network => "无法连接陪伴服务（network）", Self::Tls => "陪伴安全连接验证失败（tls）",
            Self::Format => "陪伴回复格式无效（format）", Self::Empty => "服务未返回有效陪伴回复（empty）",
            Self::Size => "陪伴回复超过大小限制（size）", Self::Api => "陪伴服务返回错误（api）",
            Self::Incomplete => "陪伴回复不完整，请增加输出上限后重试（incomplete）",
            Self::Message => "请输入不超过 2000 字的消息（message）", Self::Event => "自动事件格式无效（event）",
            Self::Expired => "自动陪伴事件已过期（expired）", Self::Busy => "待发送消息已满，请稍后再试（busy）",
            Self::Cooldown => "自动陪伴仍在冷却中（cooldown）", Self::HourlyLimit => "已达到自动陪伴每小时上限（hourly_limit）",
            Self::Backoff => "陪伴服务暂缓自动请求，稍后有新轮次时再尝试（backoff）",
            Self::Cancelled => "陪伴请求已取消（cancelled）", Self::History => "陪伴历史文件无法安全读写（history）",
            Self::Internal => "陪伴请求未完成（internal）",
        }
    }
}
impl std::fmt::Display for CompanionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.message()) }
}
impl std::error::Error for CompanionError {}

/// 与 data::TurnState 同字段，主程序逐字段映射；两个模块无需互相依赖。
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct TurnState {
    pub available: bool,
    pub is_sub: bool,
    pub session_id: String,
    pub turn_id: String,
    pub status: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub duration_ms: i64,
    pub tool_errors: i64,
    pub cancelled_by_user: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoKind { TurnFinished, LongTurn, ToolFailures }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoSignal {
    pub kind: AutoKind,
    /// epoch 秒，其他轮次时间戳在 TurnState 中为毫秒。
    pub occurred_at: f64,
    pub duration_ms: u64,
    pub tool_errors: u64,
}
impl AutoSignal {
    fn validate(&self) -> Result<(), CompanionError> {
        if !self.occurred_at.is_finite() || !(0.0..=1e15).contains(&self.occurred_at)
            || self.duration_ms > 1_000_000_000_000 || self.tool_errors > 1_000_000_000_000 {
            Err(CompanionError::Event)
        } else { Ok(()) }
    }
}

#[derive(Clone)]
pub enum Signal { Greeting, SaySomething, Message(String), Auto(AutoSignal) }
impl std::fmt::Debug for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Self::Greeting => "Greeting", Self::SaySomething => "SaySomething", Self::Message(_) => "Message([redacted])", Self::Auto(_) => "Auto(metadata)" })
    }
}
impl Signal {
    fn source(&self) -> Source { if matches!(self, Self::Auto(_)) { Source::Auto } else { Source::Manual } }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source { Manual, Auto, System }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind { Status, Partial, Reply, Error }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health { Disabled, Loading, Ready, Requesting, Paused, Stopped, Error }

#[derive(Clone, Debug, Serialize)]
pub struct CompanionStatus {
    pub state: Health,
    pub message: String,
    /// 仅代表本地文件已验证，不代表服务端已验收或已产生付费调用。
    pub configured: bool,
    pub busy: bool,
    pub queued: usize,
    pub provider_id: String,
    pub model_id: String,
    pub last_success_at: Option<f64>,
    pub error: Option<CompanionError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompanionEvent {
    pub kind: EventKind,
    /// Partial 为本次完整安全文本快照，UI 应替换，不应追加。
    pub message: String,
    pub source: Source,
    pub request_id: u64,
    pub created_at: f64,
    pub status: CompanionStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub message: String,
    pub source: Source,
    pub created_at: f64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryFile { format: String, replies: Vec<HistoryEntry> }

struct Pending { id: u64, signal: Signal, ready_at: Instant, expires_at: Option<Instant> }
#[derive(Clone)]
struct Active { id: u64, source: Source, generation: u64, auto_generation: u64 }

struct Inner {
    settings: CompanionSettings,
    connection: Option<Connection>,
    reload: bool,
    closed: bool,
    generation: u64,
    auto_generation: u64,
    next_id: u64,
    active: Option<Active>,
    manual: VecDeque<Pending>,
    auto: Option<Pending>,
    events: VecDeque<CompanionEvent>,
    history: VecDeque<HistoryEntry>,
    history_loaded: String,
    history_dirty: bool,
    history_revision: u64,
    state: Health,
    message: String,
    error: Option<CompanionError>,
    last_success_at: Option<f64>,
    failures: u32,
    backoff_until: Option<Instant>,
    auto_sent: VecDeque<Instant>,
    last_auto: Option<Instant>,
}
struct Shared { inner: Mutex<Inner>, wake: Notify }
impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> { self.inner.lock().unwrap_or_else(|e| e.into_inner()) }
}

impl Inner {
    fn status(&self) -> CompanionStatus {
        CompanionStatus { state: self.state, message: self.message.clone(), configured: self.connection.is_some() && !self.closed,
            busy: self.active.is_some(), queued: self.manual.len() + usize::from(self.auto.is_some()),
            provider_id: self.connection.as_ref().map(|c| c.info.provider_id.clone()).unwrap_or_default(),
            model_id: self.connection.as_ref().map(|c| c.info.model_id.clone()).unwrap_or_default(),
            last_success_at: self.last_success_at, error: self.error }
    }
    fn emit(&mut self, kind: EventKind, message: String, source: Source, request_id: u64) {
        if kind == EventKind::Partial {
            self.events.retain(|e| !(e.kind == EventKind::Partial && e.request_id == request_id));
        }
        if self.events.len() == MAX_RESULTS { self.events.pop_front(); }
        self.events.push_back(CompanionEvent { kind, message, source, request_id, created_at: epoch_seconds(), status: self.status() });
    }
    fn state(&mut self, health: Health, message: &str, error: Option<CompanionError>, source: Source) {
        self.state = health;
        self.message = message.to_owned();
        self.error = error;
        self.emit(if error.is_some() { EventKind::Error } else { EventKind::Status }, message.to_owned(), source, 0);
    }
    fn settle(&mut self) {
        let (health, message) = if self.closed { (Health::Stopped, "陪伴服务已停止") }
            else if !self.settings.enabled { (Health::Disabled, "陪伴已关闭") }
            else if self.settings.paused { (Health::Paused, "陪伴已暂停") }
            else if self.reload { (Health::Loading, "正在加载陪伴连接文件") }
            else if self.active.is_some() { (Health::Requesting, "正在等待小亚回复") }
            else if self.connection.is_some() { (Health::Ready, "已就绪（本地配置有效）") }
            else { (Health::Error, "请选择本地陪伴连接文件") };
        self.state(health, message, None, Source::System);
    }
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.manual.clear();
        self.auto = None;
        self.active = None;
        self.events.clear();
    }
    fn discard_auto(&mut self) {
        self.auto_generation = self.auto_generation.wrapping_add(1);
        self.auto = None;
        if self.active.as_ref().is_some_and(|a| a.source == Source::Auto) { self.active = None; }
        self.events.retain(|event| event.source != Source::Auto);
    }
    fn available(&self) -> Result<(), CompanionError> {
        if self.closed { Err(CompanionError::Closed) }
        else if !self.settings.enabled { Err(CompanionError::Disabled) }
        else if self.settings.paused { Err(CompanionError::Paused) }
        else if self.connection.is_none() { Err(self.error.unwrap_or(CompanionError::Unconfigured)) }
        else { Ok(()) }
    }
    fn valid(&self, a: &Active) -> bool {
        !self.closed && self.settings.enabled && !self.settings.paused && self.generation == a.generation
            && (a.source != Source::Auto || (self.settings.auto && self.auto_generation == a.auto_generation))
            && self.active.as_ref().is_some_and(|current| current.id == a.id)
    }
    fn auto_limit(&mut self, now: Instant) -> Result<(), CompanionError> {
        while self.auto_sent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600)) { self.auto_sent.pop_front(); }
        if self.auto_sent.len() >= self.settings.hourly_limit as usize { return Err(CompanionError::HourlyLimit); }
        if self.last_auto.is_some_and(|t| now.duration_since(t) < Duration::from_secs(self.settings.cooldown_seconds)) {
            return Err(CompanionError::Cooldown);
        }
        Ok(())
    }
}

/// 一个后台工作线程串行发请求；手动优先，自动事件合并，结果队列有界。
/// Drop/close/cancel/暂停只发取消信号，不等待网络，因此可直接从 Slint 回调调用。
pub struct Runtime {
    shared: Arc<Shared>,
    watch: watch::TurnWatch,
}

impl Default for Runtime { fn default() -> Self { Self::new(CompanionSettings::default()) } }

impl Runtime {
    pub fn new(settings: CompanionSettings) -> Self {
        let settings = settings.normalized();
        let enabled = settings.enabled;
        let shared = Arc::new(Shared { inner: Mutex::new(Inner { settings, connection: None, reload: enabled,
            closed: false, generation: 0, auto_generation: 0, next_id: 0, active: None, manual: VecDeque::new(),
            auto: None, events: VecDeque::new(), history: VecDeque::new(), history_loaded: String::new(),
            history_dirty: false, history_revision: 0, state: Health::Disabled, message: String::new(), error: None,
            last_success_at: None, failures: 0, backoff_until: None, auto_sent: VecDeque::new(), last_auto: None }), wake: Notify::new() });
        shared.lock().settle();
        let worker = shared.clone();
        let spawned = std::thread::Builder::new().name("companion-service".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(worker_loop(worker)),
                Err(_) => {
                    let mut s = worker.lock();
                    s.state(Health::Error, CompanionError::Internal.message(), Some(CompanionError::Internal), Source::System);
                }
            }
        });
        if spawned.is_err() {
            shared.lock().state(Health::Error, CompanionError::Internal.message(), Some(CompanionError::Internal), Source::System);
        }
        Self { shared, watch: watch::TurnWatch::default() }
    }

    pub fn settings(&self) -> CompanionSettings { self.shared.lock().settings.clone() }
    pub fn status(&self) -> CompanionStatus { self.shared.lock().status() }
    pub fn history(&self) -> Vec<HistoryEntry> { self.shared.lock().history.iter().cloned().collect() }
    pub fn connection_info(&self) -> Option<ConnectionInfo> { self.shared.lock().connection.as_ref().map(|c| c.info.clone()) }
    pub fn drain(&self) -> Vec<CompanionEvent> { self.shared.lock().events.drain(..).collect() }

    pub fn configure(&mut self, settings: CompanionSettings) {
        let settings = settings.normalized();
        let mut s = self.shared.lock();
        if s.closed || s.settings == settings { return; }
        let old = &s.settings;
        let connection_changed = old.config_path != settings.config_path || old.provider_id != settings.provider_id
            || old.model_id != settings.model_id;
        let reconnect = connection_changed || old.enabled != settings.enabled || old.timeout_seconds != settings.timeout_seconds
            || old.stream != settings.stream || old.max_output_tokens != settings.max_output_tokens;
        let paused_changed = old.paused != settings.paused;
        let auto_changed = old.auto != settings.auto;
        let history_changed = old.history_path != settings.history_path || old.persist_history != settings.persist_history;
        if reconnect || paused_changed { s.invalidate(); }
        else if auto_changed { s.discard_auto(); }
        if connection_changed {
            s.history.clear();
            s.history_loaded.clear();
            s.history_dirty = false;
            s.history_revision = s.history_revision.wrapping_add(1);
            s.last_success_at = None;
        }
        if history_changed {
            s.history_loaded.clear();
            s.history_revision = s.history_revision.wrapping_add(1);
        }
        s.settings = settings;
        if reconnect {
            s.connection = None;
            s.reload = s.settings.enabled;
            s.failures = 0;
            s.backoff_until = None;
        } else if paused_changed && s.connection.is_none() && s.settings.enabled {
            // 暂停使在途本地加载代次失效；恢复时必须重新装载。
            s.reload = true;
        }
        if reconnect || paused_changed || auto_changed { self.watch.rebaseline(); }
        s.settle();
        drop(s);
        self.shared.wake.notify_one();
    }

    /// 显式重载文件，只进行本地验证，不发健康探测或问候请求。
    pub fn reload(&mut self) {
        let mut s = self.shared.lock();
        if s.closed { return; }
        s.invalidate();
        s.connection = None;
        s.reload = s.settings.enabled;
        s.failures = 0;
        s.backoff_until = None;
        self.watch.rebaseline();
        s.settle();
        drop(s);
        self.shared.wake.notify_one();
    }

    /// 每次最新快照调用；首次观察、恢复数据、切会话只建基线。
    /// 返回的 Auto 不含 session/turn ID。调用方随后 request(signal) 即可。
    pub fn eligible(&mut self, state: &TurnState, now: f64) -> Option<Signal> {
        if self.watch.selection_changed(state) { self.discard_auto(); }
        let settings = self.settings();
        if self.shared.lock().available().is_err() || !settings.auto {
            // 禁用/暂停/未配置期间不消耗额度，恢复后只为当前轮建立新基线。
            self.watch.rebaseline();
            return None;
        }
        self.watch.observe(state, now, &settings).map(Signal::Auto)
    }

    /// 便于主定时器接入的组合方法；不在当前线程执行网络。
    pub fn observe(&mut self, state: &TurnState, now: f64) -> bool {
        self.eligible(state, now).is_some_and(|signal| self.request(signal).is_ok())
    }

    pub fn request(&self, signal: Signal) -> Result<(), CompanionError> {
        let source = signal.source();
        let mut s = self.shared.lock();
        let result = (|| {
            s.available()?;
            // 验证文本只在内存，既不记日志也不写历史。
            http::prompt(&signal)?;
            let now = Instant::now();
            let expires_at = if let Signal::Auto(event) = &signal {
                if !s.settings.auto { return Err(CompanionError::Disabled); }
                let age = epoch_seconds() - event.occurred_at;
                if age < -1.0 || age >= s.settings.event_ttl_seconds as f64 { return Err(CompanionError::Expired); }
                if s.manual.len() >= s.settings.max_pending { return Err(CompanionError::Busy); }
                s.auto_limit(now)?;
                Some(now + Duration::from_secs_f64((s.settings.event_ttl_seconds as f64 - age.max(0.0)).max(0.001)))
            } else {
                if s.manual.len() >= s.settings.max_pending { return Err(CompanionError::Busy); }
                if s.manual.len() + usize::from(s.auto.is_some()) >= s.settings.max_pending { s.auto = None; }
                None
            };
            s.next_id = s.next_id.wrapping_add(1);
            let pending = Pending { id: s.next_id, signal, ready_at: now + if source == Source::Auto { Duration::from_millis(200) } else { Duration::ZERO }, expires_at };
            if source == Source::Auto { s.auto = Some(pending); } else { s.manual.push_back(pending); }
            Ok(())
        })();
        if let Err(error) = result {
            // 不破坏在途任务或暂停/关闭状态，也不将拒绝的文本回显给 UI。
            if s.active.is_none() && !matches!(error, CompanionError::Closed | CompanionError::Disabled | CompanionError::Paused) {
                s.state = Health::Error;
                s.message = error.to_string();
                s.error = Some(error);
            }
            s.emit(EventKind::Error, error.to_string(), source, 0);
        }
        drop(s);
        if result.is_ok() { self.shared.wake.notify_one(); }
        result
    }

    pub fn set_paused(&mut self, paused: bool) {
        let mut settings = self.settings();
        settings.paused = paused;
        self.configure(settings);
    }
    pub fn discard_auto(&self) {
        let mut s = self.shared.lock();
        if s.closed { return; }
        s.discard_auto();
        if s.connection.is_some() && s.settings.enabled && !s.settings.paused { s.settle(); }
        drop(s);
        self.shared.wake.notify_one();
    }
    /// 清空触发基线和频率记录，只取消自动任务；手动聊天保留。
    pub fn reset(&mut self) {
        self.watch.reset();
        let mut s = self.shared.lock();
        s.discard_auto();
        s.auto_sent.clear();
        s.last_auto = None;
        drop(s);
        self.shared.wake.notify_one();
    }
    /// 停止本次及排队请求；陪伴仍保持开启，可继续手动发送。
    pub fn cancel(&self) {
        let mut s = self.shared.lock();
        if s.closed { return; }
        let loading = s.state == Health::Loading;
        s.invalidate();
        if loading && s.settings.enabled && s.connection.is_none() { s.reload = true; }
        s.settle();
        s.emit(EventKind::Status, CompanionError::Cancelled.to_string(), Source::System, 0);
        drop(s);
        self.shared.wake.notify_one();
    }
    pub fn clear_history(&self) {
        let mut s = self.shared.lock();
        s.history.clear();
        s.history_revision = s.history_revision.wrapping_add(1);
        s.history_loaded = s.settings.history_path.clone();
        s.history_dirty = true;
        drop(s);
        self.shared.wake.notify_one();
    }
    pub fn close(&self) {
        let mut s = self.shared.lock();
        if s.closed { return; }
        s.closed = true;
        s.invalidate();
        s.connection = None;
        s.reload = false;
        s.settle();
        drop(s);
        self.shared.wake.notify_one();
    }
    pub fn stop(&self) { self.close(); }
}
impl Drop for Runtime { fn drop(&mut self) { self.close(); } }

pub fn epoch_seconds() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() }

struct Job { pending: Pending, active: Active, connection: Connection, settings: CompanionSettings }

fn next_job(s: &mut Inner) -> (Option<Job>, Option<Duration>) {
    if s.available().is_err() { return (None, None); }
    let now = Instant::now();
    let pending = if let Some(manual) = s.manual.pop_front() { Some(manual) } else {
        if s.auto.as_ref().and_then(|p| p.expires_at).is_some_and(|t| t <= now) {
            s.auto = None;
            s.emit(EventKind::Status, CompanionError::Expired.to_string(), Source::Auto, 0);
        }
        let Some(pending) = s.auto.as_ref() else { return (None, None); };
        let ready = pending.ready_at.max(s.backoff_until.unwrap_or(now));
        if ready > now {
            let wait = ready.min(pending.expires_at.unwrap_or(ready)).duration_since(now);
            return (None, Some(wait));
        }
        if let Err(error) = s.auto_limit(now) {
            s.auto = None;
            s.emit(EventKind::Status, error.to_string(), Source::Auto, 0);
            return (None, None);
        }
        s.auto.take()
    };
    let Some(pending) = pending else { return (None, None); };
    let active = Active { id: pending.id, source: pending.signal.source(), generation: s.generation, auto_generation: s.auto_generation };
    if active.source == Source::Auto {
        s.auto_sent.push_back(now);
        s.last_auto = Some(now);
    }
    s.active = Some(active.clone());
    s.state(Health::Requesting, "正在等待小亚回复", None, active.source);
    (Some(Job { pending, active, connection: s.connection.as_ref().unwrap().clone(), settings: s.settings.clone() }), None)
}

async fn worker_loop(shared: Arc<Shared>) {
    loop {
        let reload = {
            let mut s = shared.lock();
            if s.closed { return; }
            if s.reload {
                s.reload = false;
                Some((s.generation, s.settings.clone()))
            } else { None }
        };
        if let Some((generation, settings)) = reload {
            let result = tokio::task::spawn_blocking(move || load_connection(&settings)).await.unwrap_or(Err(CompanionError::Internal));
            let mut s = shared.lock();
            if s.closed { return; }
            if s.generation == generation && s.settings.enabled {
                match result {
                    Ok(connection) => { s.connection = Some(connection); s.settle(); }
                    Err(error) => { s.connection = None; s.state(Health::Error, error.message(), Some(error), Source::System); }
                }
            }
            continue;
        }
        service_history(&shared).await;
        let (job, wait) = { let mut s = shared.lock(); if s.closed { return; } next_job(&mut s) };
        let Some(job) = job else {
            if let Some(wait) = wait {
                let _ = tokio::time::timeout(wait, shared.wake.notified()).await;
            } else { shared.wake.notified().await; }
            continue;
        };
        let active = job.active.clone();
        if !shared.lock().valid(&active) { continue; }
        let outcome = match http::client(job.settings.timeout_seconds) {
            Err(error) => Some(Err(error)),
            Ok(client) => {
                let progress_shared = shared.clone();
                let progress_active = active.clone();
                let request = http::request(&client, &job.connection, &job.pending.signal, &job.settings, move |message| {
                    let mut s = progress_shared.lock();
                    if s.valid(&progress_active) && s.settings.stream {
                        s.emit(EventKind::Partial, message, progress_active.source, progress_active.id);
                    }
                });
                let timed = tokio::time::timeout(Duration::from_secs(job.settings.timeout_seconds), request);
                let mut timed = std::pin::pin!(timed);
                loop {
                    let notified = shared.wake.notified();
                    let mut notified = std::pin::pin!(notified);
                    let result = std::future::poll_fn(|cx| {
                        use std::future::Future;
                        use std::task::Poll;
                        if !shared.lock().valid(&active) || notified.as_mut().poll(cx).is_ready() { return Poll::Ready(None); }
                        timed.as_mut().poll(cx).map(Some)
                    }).await;
                    match result {
                        None if !shared.lock().valid(&active) => break None,
                        None => {},
                        Some(result) => break Some(result.unwrap_or(Err(CompanionError::Timeout))),
                    }
                }
                // future 离开作用域即丢弃底层响应/连接，不阻塞等待远端。
            }
        };
        let mut s = shared.lock();
        if !s.valid(&active) { continue; }
        s.active = None;
        let Some(outcome) = outcome else { continue; };
        s.events.retain(|event| !(event.kind == EventKind::Partial && event.request_id == active.id));
        match outcome {
            Ok(message) => {
                s.failures = 0;
                s.backoff_until = None;
                s.last_success_at = Some(epoch_seconds());
                if s.history.len() >= MAX_HISTORY { s.history.pop_front(); }
                s.history.push_back(HistoryEntry { message: message.clone(), source: active.source, created_at: epoch_seconds() });
                s.history_revision = s.history_revision.wrapping_add(1);
                s.history_dirty = true;
                s.emit(EventKind::Reply, message, active.source, active.id);
                s.state(Health::Ready, "小亚已回复", None, active.source);
            }
            Err(error) => {
                s.failures = (s.failures + 1).min(16);
                let backoff = (15u64.saturating_mul(1u64 << (s.failures - 1))).min(300);
                s.backoff_until = Some(Instant::now() + Duration::from_secs(backoff));
                s.state(Health::Error, error.message(), Some(error), active.source);
            }
        }
    }
}

fn read_history(path: &Path) -> Result<Vec<HistoryEntry>, CompanionError> {
    if !path.exists() { return Ok(Vec::new()); }
    let metadata = fs::symlink_metadata(path).map_err(|_| CompanionError::History)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 512 * 1024 { return Err(CompanionError::History); }
    let mut bytes = Vec::new();
    File::open(path).map_err(|_| CompanionError::History)?.take(512 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| CompanionError::History)?;
    if bytes.len() > 512 * 1024 { return Err(CompanionError::History); }
    let data: HistoryFile = serde_json::from_slice(&bytes).map_err(|_| CompanionError::History)?;
    if data.format != HISTORY_FORMAT || data.replies.len() > MAX_HISTORY
        || data.replies.iter().any(|e| e.message.chars().count() > 600 || !e.created_at.is_finite() || e.created_at < 0.0 || e.source == Source::System) {
        return Err(CompanionError::History);
    }
    Ok(data.replies)
}

fn history_path(settings: &CompanionSettings) -> Result<Option<PathBuf>, CompanionError> {
    if !settings.persist_history || settings.history_path.is_empty() { return Ok(None); }
    let path = PathBuf::from(&settings.history_path);
    // 防止把连接文件/普通设置当历史覆盖；也不允许自动猜一个隐式位置。
    if !path.is_absolute() || path.extension().and_then(|s| s.to_str()) != Some("json") { return Err(CompanionError::History); }
    let source = Path::new(&settings.config_path);
    if path == source || (path.exists() && source.exists() && fs::canonicalize(&path).ok() == fs::canonicalize(source).ok()) {
        return Err(CompanionError::History);
    }
    if path.exists() { read_history(&path)?; }
    Ok(Some(path))
}

async fn service_history(shared: &Arc<Shared>) {
    enum Action { Load(CompanionSettings, u64, u64), Save(CompanionSettings, Vec<HistoryEntry>, u64), None }
    let action = {
        let mut s = shared.lock();
        if s.closed || !s.settings.enabled || !s.settings.persist_history || s.settings.history_path.is_empty() { Action::None }
        else if s.history_loaded != s.settings.history_path {
            s.history_loaded = s.settings.history_path.clone();
            Action::Load(s.settings.clone(), s.generation, s.history_revision)
        } else if s.history_dirty {
            s.history_dirty = false;
            Action::Save(s.settings.clone(), s.history.iter().cloned().collect(), s.history_revision)
        } else { Action::None }
    };
    match action {
        Action::None => {}
        Action::Load(settings, generation, revision) => {
            let path_name = settings.history_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                match history_path(&settings)? { Some(path) => read_history(&path), None => Ok(Vec::new()) }
            }).await.unwrap_or(Err(CompanionError::History));
            let mut s = shared.lock();
            if s.closed || !s.settings.persist_history || s.generation != generation
                || s.settings.history_path != path_name || s.history_revision != revision { return; }
            match result {
                Ok(entries) => {
                    // 已有内存回复时不倒灌旧记录；配置切换后的历史由显式历史路径决定。
                    if s.history.is_empty() {
                        s.history = entries.into_iter().filter_map(|mut entry| {
                            if let Some(connection) = &s.connection { entry.message = http::sanitize_history(&entry.message, connection); }
                            if entry.message.is_empty() { None } else { Some(entry) }
                        }).collect();
                    }
                }
                Err(error) => s.emit(EventKind::Error, error.to_string(), Source::System, 0),
            }
            if s.history_dirty { shared.wake.notify_one(); }
        }
        Action::Save(settings, entries, revision) => {
            let worker = shared.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<(), CompanionError> {
                let Some(path) = history_path(&settings)? else { return Ok(()); };
                if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|_| CompanionError::History)?; }
                let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
                let temp = path.with_extension(format!("json.{}.{}.{}.tmp", std::process::id(), revision, nonce));
                let mut owned_temp = false;
                let result = (|| {
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
                    let mut file = options.open(&temp).map_err(|_| CompanionError::History)?;
                    owned_temp = true;
                    let data = HistoryFile { format: HISTORY_FORMAT.into(), replies: entries };
                    let raw = serde_json::to_vec_pretty(&data).map_err(|_| CompanionError::History)?;
                    file.write_all(&raw).and_then(|_| file.sync_all()).map_err(|_| CompanionError::History)?;
                    drop(file);
                    let s = worker.lock();
                    if s.closed || !s.settings.persist_history || s.settings.history_path != settings.history_path || s.history_revision != revision {
                        return Ok(());
                    }
                    fs::rename(&temp, &path).map_err(|_| CompanionError::History)
                })();
                if owned_temp { let _ = fs::remove_file(temp); }
                result
            }).await.unwrap_or(Err(CompanionError::History));
            if let Err(error) = result {
                let mut s = shared.lock();
                if !s.closed { s.emit(EventKind::Error, error.to_string(), Source::System, 0); }
            }
        }
    }
}

#[cfg(test)]
#[path = "companion_tests.rs"]
mod tests;
