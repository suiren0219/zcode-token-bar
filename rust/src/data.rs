//! 只读用量数据层。成功请求的 input 已包含 cache_read，total 不再叠加缓存。
//! 慢聚合按连接/数据库版本缓存；实时请求和轮状态只走选中会话的索引。

use chrono::{Local, NaiveDate, TimeZone};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[path = "data_context.rs"]
mod context;
#[path = "data_sql.rs"]
mod sql;
#[path = "data_remote.rs"]
mod remote;
#[path = "data_cli.rs"]
mod cli;
pub use cli::handle_cli;
pub use remote::RemoteConfig;

const SESSION_TIMEOUT_MS: i64 = 30 * 60 * 1000;
const PENDING_TIMEOUT_MS: i64 = 15 * 60 * 1000;

pub fn db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ZCODE_TOKEN_BAR_DB") {
        return p.into();
    }
    for base in context::data_homes() {
        let p = base.join("cli/db/db.sqlite");
        if p.is_file() { return p; }
    }
    context::home().join(".zcode/cli/db/db.sqlite")
}

/// READ_ONLY 不会创建库；不以「有数据行」作为连接成功的条件。
pub fn connect_readonly() -> rusqlite::Result<Connection> {
    connect_readonly_at(db_path())
}

pub fn connect_readonly_at(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let con = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    con.busy_timeout(Duration::from_secs(3))?;
    con.execute_batch("PRAGMA query_only=ON")?;
    con.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
    clear_cache();
    Ok(con)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStats {
    pub requests: i64, pub input: i64, pub output: i64, pub reasoning: i64,
    pub cache_read: i64, pub cache_write: i64, pub total: i64, pub tool_calls: i64,
    pub last_request_input: i64, pub turns: i64, pub retries: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnStats {
    pub requests: i64, pub retries: i64, pub tool_calls: i64, pub tool_errors: i64,
    pub input: i64, pub output: i64, pub reasoning: i64, pub cache_read: i64,
    pub cache_write: i64, pub total: i64, pub duration_ms: i64, pub ttft_ms: i64,
}

/// 只有 turn_usage 能证明整轮终态；模型请求 completed 不能证明整轮结束。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnState {
    pub available: bool, pub session_id: String, pub turn_id: String, pub status: String,
    pub started_at: i64, pub completed_at: i64, pub duration_ms: i64,
    pub tool_errors: i64, pub cancelled_by_user: bool, pub is_sub: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LastRequest {
    pub duration_ms: i64, pub ttft_ms: i64, pub model: String, pub tps: f64,
    pub provider: String, pub completed_at: i64, pub first_token_at: i64,
    pub input: i64, pub output: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RangeUsage {
    pub requests: i64, pub input: i64, pub output: i64, pub cache_read: i64, pub total: i64,
    pub reasoning: i64, pub cache_write: i64, pub retries: i64, pub duration_ms: i64,
}

impl RangeUsage {
    fn add(&mut self, v: &Self) {
        self.requests += v.requests; self.input += v.input; self.output += v.output;
        self.cache_read += v.cache_read; self.total += v.total; self.reasoning += v.reasoning;
        self.cache_write += v.cache_write; self.retries += v.retries; self.duration_ms += v.duration_ms;
    }
    pub fn cache_rate(&self) -> f64 { cache_rate(self.input, self.cache_read) }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolInfo { pub name: String, pub count: i64, pub duration_ms: i64, pub errors: i64 }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSummary { pub total: i64, pub errors: i64, pub list: Vec<ToolInfo> }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeSummary {
    pub add: Option<i64>,
    #[serde(rename = "del")]
    pub deleted: Option<i64>,
    pub files: Option<i64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SubInfo {
    pub sid: String, pub agent: String, pub title: String, pub task: String,
    pub requests: i64, pub total: i64, pub input: i64, pub output: i64,
    pub cache_read: i64, pub reasoning: i64, pub cache_write: i64,
    pub last: i64, pub active: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SubSummary {
    pub requests: i64, pub total: i64, pub active: bool,
    pub input: i64, pub output: i64, pub cache_read: i64, pub reasoning: i64,
    pub cache_write: i64, pub list: Vec<SubInfo>,
}

/// 原 Snapshot 字段全部保留。context_known=false 时不能把窗口当作模型官方容量。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub ok: bool, pub error: String, pub sid: String, pub title: String, pub directory: String,
    pub active: bool, pub generating: bool, pub turns: i64, pub requests: i64,
    pub input: i64, pub output: i64, pub reasoning: i64, pub cache_read: i64,
    pub cache_write: i64, pub total: i64, pub tool_calls: i64, pub retries: i64,
    pub ctx: i64, pub updated: String, pub ctx_exc: i64, pub context_window: i64,
    pub context_auto: bool, pub last_at: i64, pub is_sub: bool,
    pub last_turn: TurnStats, pub last: LastRequest,
    pub tools_total: i64, pub tools_errors: i64, pub sub: SubSummary, pub today: RangeUsage,
    pub today_text: String, pub total_text: String,
    pub pinned: String, pub pin_lost: bool, pub parent: Option<String>,
    pub context_known: bool, pub context_inferred: bool, pub context_source: String,
    pub context_exceeded: bool,
    /// 百分数，0..100；分母为 input，不是 input + cache_read。
    pub cache_rate: f64,
    pub turn_state: TurnState, pub tools: ToolSummary, pub code: CodeSummary,
    /// 近 7 / 30 个本地自然日（含今天），与 Python days 7/30 一致。
    pub week: RangeUsage, pub month: RangeUsage,
    pub detail_text: String,
    pub remote: bool, pub remote_host: String, pub remote_enabled: bool,
    pub remote_error: String, pub not_local: Vec<String>, pub known_sids: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    pub sid: String, pub title: String, pub directory: String, pub parent: Option<String>,
    pub is_sub: bool, pub active: bool, pub last_at: i64,
    pub requests: i64, pub turns: i64, pub input: i64, pub output: i64,
    pub total: i64, pub cache_read: i64, pub cache_write: i64, pub reasoning: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyUsage { pub date: String, pub usage: RangeUsage }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelUsage { pub provider: String, pub model: String, pub usage: RangeUsage }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageReports {
    pub days: usize, pub today: RangeUsage, pub week: RangeUsage, pub month: RangeUsage,
    pub daily: Vec<DailyUsage>, pub models: Vec<ModelUsage>, pub sessions: Vec<SessionInfo>,
    pub detail_text: String,
}

pub fn fmt_tokens(n: i64) -> String {
    let n = n.max(0);
    if n >= 1_000_000_000 { format!("{:.2}B", n as f64 / 1e9) }
    else if n >= 1_000_000 { format!("{:.2}M", n as f64 / 1e6) }
    else if n >= 1_000 { format!("{:.1}K", n as f64 / 1e3) }
    else { n.to_string() }
}

pub fn cache_rate(input: i64, read: i64) -> f64 {
    if input <= 0 { 0.0 } else { (read.max(0) as f64 / input as f64 * 100.0).clamp(0.0, 100.0) }
}
fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }
fn recent(t: i64, now: i64, age: i64) -> bool { t > 0 && now >= t && now - t < age }
fn time_text(t: i64) -> String {
    Local.timestamp_millis_opt(t).single().filter(|_| t > 0)
        .map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_default()
}
fn day_start(date: NaiveDate) -> i64 {
    // 极少数地区午夜跳时：找当天第一个合法时刻；不用固定 24h 推算明天。
    for hour in 0..24 {
        if let Some(t) = date.and_hms_opt(hour, 0, 0).and_then(|d| Local.from_local_datetime(&d).earliest()) {
            return t.timestamp_millis();
        }
    }
    date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}
fn day_bounds(days: usize) -> (i64, i64) {
    let date = Local::now().date_naive();
    (day_start(date - chrono::Duration::days(days.saturating_sub(1) as i64)),
     day_start(date + chrono::Duration::days(1)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Revision { source: String, version: i64, changes: u64, schema: i64 }
struct Cached { at: Instant, revision: Revision, value: serde_json::Value }
thread_local! { static CACHE: RefCell<HashMap<String, Cached>> = RefCell::new(HashMap::new()); }

/// 缓存只在当前数据线程使用；重连时清除，不能把另一数据库的同名 sid 命中进来。
pub fn clear_cache() { CACHE.with(|c| c.borrow_mut().clear()); }
fn revision(con: &Connection) -> rusqlite::Result<Option<Revision>> {
    let path: String = con.query_row("PRAGMA database_list", [], |r| r.get(2))?;
    if path.is_empty() { return Ok(None); } // 内存 fixture 无持久身份，不共享缓存。
    Ok(Some(Revision {
        source: format!("{:p}:{path}", con),
        version: con.query_row("PRAGMA data_version", [], |r| r.get(0))?,
        schema: con.query_row("PRAGMA schema_version", [], |r| r.get(0))?,
        changes: con.total_changes(),
    }))
}
fn cached<T: Serialize + for<'de> Deserialize<'de>>(
    con: &Connection, key: &str, min_age: Duration, build: impl FnOnce() -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    let Some(rev) = revision(con)? else { return build(); };
    let hit = CACHE.with(|c| {
        let cache = c.borrow(); let e = cache.get(key)?;
        if e.revision.source == rev.source && e.revision.schema == rev.schema
            && (e.revision == rev || e.at.elapsed() < min_age) {
            serde_json::from_value(e.value.clone()).ok()
        } else { None }
    });
    if let Some(value) = hit { return Ok(value); }
    let value = build()?;
    if let Ok(v) = serde_json::to_value(&value) {
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= 64 { c.clear(); }
            c.insert(key.to_string(), Cached { at: Instant::now(), revision: rev, value: v });
        });
    }
    Ok(value)
}

pub fn snapshot(con: &Connection, default_window: i64) -> Snapshot { snapshot_for(con, default_window, "") }

/// sid 为空自动跟随；指定会话在 session 或 usage 中存在就可钉住，包括零请求新会话。
pub fn snapshot_for(con: &Connection, default_window: i64, sid: &str) -> Snapshot {
    match snapshot_inner(con, default_window, sid.trim()) {
        Ok(mut s) => { finish(&mut s); s }
        Err(e) => Snapshot { error: e.to_string(), ..Default::default() },
    }
}

fn snapshot_inner(con: &Connection, default_window: i64, pin: &str) -> rusqlite::Result<Snapshot> {
    let schema = sql::Schema::load(con)?;
    let now = now_ms();
    let mut s = Snapshot { ok: true, ..Default::default() };
    let (today, week, month) = periods(con, &schema)?;
    s.today = today; s.week = week; s.month = month;
    let known = valid_sid(pin) && sql::session_exists(con, &schema, pin)?;
    let selected = if known { Some(pin.to_string()) } else { sql::current_sid(con, &schema)? };
    if valid_sid(pin) {
        s.pinned = pin.to_string(); s.pin_lost = !known;
        if known { s.known_sids.push(pin.to_string()); } else { s.not_local.push(pin.to_string()); }
    }
    let Some(sid) = selected else { return Ok(s); };
    let meta = sql::session_meta(con, &schema, &sid)?;
    let u = sql::session_usage(con, &schema, &sid)?;
    let last = sql::last_request(con, &schema, &sid, true)?;
    let latest = sql::last_request(con, &schema, &sid, false)?;
    let messages = sql::message_state(con, &schema, &sid)?;
    let (mut turn, mut state) = sql::turn(con, &schema, &sid, &latest, meta.reverted || meta.deleted)?;
    s.sid = sid.clone(); s.title = meta.title; s.directory = meta.directory; s.parent = meta.parent;
    s.is_sub = s.parent.as_ref().is_some_and(|p| !p.is_empty()) || sid.starts_with("sess_subagent");
    state.is_sub = s.is_sub;
    let valid_turn = state.available;
    let awaiting = recent(messages.user_at, now, PENDING_TIMEOUT_MS)
        && ((state.available && messages.user_at > state.completed_at.max(state.started_at))
            || (!state.available && messages.latest_role == "user" && messages.user_at > latest.completed_at));
    // 新用户轮不能展示上一轮的 completed/error/cancelled 为本轮终态。
    if awaiting && state.available && state.status != "running" {
        state.available = false; state.status.clear(); turn = TurnStats::default();
    }
    let request_after_turn = !valid_turn || latest.started_at > state.completed_at.max(state.started_at);
    s.generating = awaiting || (valid_turn && state.status == "running")
        || (request_after_turn && latest.status == "running" && recent(latest.started_at, now, PENDING_TIMEOUT_MS));
    // 陪伴/主轮通知只消费父会话；子代理自己的生成状态和指标仍可显示。
    if s.is_sub { state.available = false; }
    s.last_at = latest.completed_at.max(latest.started_at).max(last.completed_at).max(messages.user_at);
    s.active = s.generating || recent(s.last_at, now, SESSION_TIMEOUT_MS);
    s.requests = u.requests; s.turns = u.turns; s.input = u.input; s.output = u.output;
    s.reasoning = u.reasoning; s.cache_read = u.cache_read; s.cache_write = u.cache_write;
    s.total = u.total; s.tool_calls = u.tool_calls; s.retries = u.retries; s.ctx = last.input;
    s.last = LastRequest {
        duration_ms: last.duration_ms, ttft_ms: last.ttft_ms, model: last.model.clone(),
        provider: last.provider.clone(), completed_at: last.completed_at,
        first_token_at: last.first_token_at, input: last.input, output: last.output,
        tps: request_tps(&last),
    };
    s.last_turn = turn; s.turn_state = state;
    let model = if last.model.is_empty() { &latest.model } else { &last.model };
    let provider = if last.provider.is_empty() { &latest.provider } else { &last.provider };
    let window = context::resolve(provider, model, s.ctx, default_window);
    s.context_window = window.value; s.context_known = window.known;
    s.context_inferred = window.inferred; s.context_source = window.source;
    s.context_auto = window.known || window.inferred;
    s.ctx_exc = sql::context_error(con, &schema, &sid)?;
    s.context_exceeded = s.ctx_exc > last.completed_at && s.ctx_exc > 0;
    s.tools = sql::tools(con, &schema, &sid)?;
    s.tools_total = s.tools.total; s.tools_errors = s.tools.errors;
    s.sub = sql::subagents(con, &schema, &sid, now)?;
    s.code = meta.code;
    Ok(s)
}

fn request_tps(last: &sql::Request) -> f64 {
    let ms = if last.first_token_at > 0 && last.completed_at > last.first_token_at {
        last.completed_at - last.first_token_at
    } else { last.duration_ms };
    if ms > 0 && last.output > 0 { (last.output as f64 * 10000.0 / ms as f64).round() / 10.0 } else { 0.0 }
}
fn finish(s: &mut Snapshot) {
    s.today_text = fmt_tokens(s.today.total); s.total_text = fmt_tokens(s.total);
    s.cache_rate = cache_rate(s.input, s.cache_read); s.updated = time_text(s.last_at);
    s.detail_text = detail_text(s);
}
fn periods(con: &Connection, schema: &sql::Schema) -> rusqlite::Result<(RangeUsage, RangeUsage, RangeUsage)> {
    let date = Local::now().date_naive();
    cached(con, &format!("periods:{date}"), Duration::from_secs(8), || {
        let rows = sql::daily(con, schema, 30)?;
        let n = rows.len(); let mut week = RangeUsage::default(); let mut month = RangeUsage::default();
        for (i, r) in rows.iter().enumerate() { month.add(&r.usage); if i + 7 >= n { week.add(&r.usage); } }
        Ok((rows.last().map(|r| r.usage.clone()).unwrap_or_default(), week, month))
    })
}

/// 一次批量聚合列出会话，包含只有消息的新会话；不会对列表每行重算完整快照。
pub fn session_list(con: &Connection, limit: usize) -> rusqlite::Result<Vec<SessionInfo>> {
    let schema = sql::Schema::load(con)?;
    let mut rows: Vec<SessionInfo> = cached(con, &format!("sessions:{}", limit.min(500)), Duration::from_secs(5), || {
        sql::sessions(con, &schema, limit.min(500))
    })?;
    let now = now_ms();
    for row in &mut rows { row.active = recent(row.last_at, now, SESSION_TIMEOUT_MS); }
    Ok(rows)
}

pub fn reports(con: &Connection, days: usize) -> rusqlite::Result<UsageReports> {
    let days = days.clamp(1, 366);
    let schema = sql::Schema::load(con)?;
    let mut out: UsageReports = cached(con, &format!("reports:{days}:{}", Local::now().date_naive()), Duration::from_secs(8), || {
        let (today, week, month) = periods(con, &schema)?;
        Ok(UsageReports { days, today, week, month, daily: sql::daily(con, &schema, days)?,
            models: sql::models(con, &schema, days)?, sessions: session_list(con, 50)?, detail_text: String::new() })
    })?;
    out.detail_text = report_text(&out);
    Ok(out)
}

fn duration_text(ms: i64) -> String {
    if ms >= 60_000 { format!("{}分{:.0}秒", ms / 60_000, (ms % 60_000) as f64 / 1000.0) }
    else { format!("{:.1}秒", ms.max(0) as f64 / 1000.0) }
}
fn plain(s: &str, len: usize) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(len).collect() }

pub fn detail_text(s: &Snapshot) -> String {
    if !s.ok { return format!("读取数据失败：{}", s.error); }
    let mut rows = vec![format!("会话  {}", if s.title.is_empty() { plain(&s.sid, 64) } else { plain(&s.title, 64) })];
    let state = if s.generating { "生成中" } else { match s.turn_state.status.as_str() {
        "error" => "本轮失败", "cancelled" => "本轮已取消", "completed" => "本轮已完成", _ if s.active => "最近活跃", _ => "空闲",
    }};
    rows.push(format!("状态  {state}{}", if s.pinned.is_empty() { "" } else { " · 已钉住" }));
    if s.context_window > 0 {
        let mark = if s.context_inferred { "推断" } else if s.context_known { "已知" } else { "设置参考值，模型窗口未知" };
        rows.push(format!("上下文  {} / {} · {:.1}% · {mark}", fmt_tokens(s.ctx), fmt_tokens(s.context_window), s.ctx as f64 / s.context_window as f64 * 100.0));
        if !s.context_source.is_empty() { rows.push(format!("窗口来源  {}", s.context_source)); }
    } else { rows.push(format!("上下文  {} · 窗口未知", fmt_tokens(s.ctx))); }
    if s.context_exceeded { rows.push("上下文超限  最近请求被拒，尚无后续成功请求".into()); }
    rows.push(format!("缓存率  {:.1}% · 读 {} / 输入 {}（输入已含缓存）", cache_rate(s.input, s.cache_read), fmt_tokens(s.cache_read), fmt_tokens(s.input)));
    let t = &s.last_turn;
    rows.push(format!("本轮  {} · {}次请求 · {} · 首 token {}", fmt_tokens(t.total), t.requests, duration_text(t.duration_ms), duration_text(t.ttft_ms)));
    rows.push(format!("本轮拆分  输入 {} · 输出 {} · 思考 {} · 缓存读 {} / 写 {}", fmt_tokens(t.input), fmt_tokens(t.output), fmt_tokens(t.reasoning), fmt_tokens(t.cache_read), fmt_tokens(t.cache_write)));
    rows.push(format!("会话累计  {} · {}轮 / {}次 · 重试 {}", fmt_tokens(s.total), s.turns, s.requests, s.retries));
    rows.push(format!("今日  {} · {}次请求", fmt_tokens(s.today.total), s.today.requests));
    rows.push(format!("近7天 / 30天  {} / {}", fmt_tokens(s.week.total), fmt_tokens(s.month.total)));
    if !s.last.model.is_empty() { rows.push(format!("最近请求  {} · {:.1} t/s · 耗时 {} · TTFT {}", s.last.model, s.last.tps, duration_text(s.last.duration_ms), duration_text(s.last.ttft_ms))); }
    if s.tools.total > 0 {
        rows.push(format!("工具  {}次 · {}次错误", s.tools.total, s.tools.errors));
        for t in s.tools.list.iter().take(5) { rows.push(format!("  {}  {}次 / 错误{} / {}", plain(&t.name, 48), t.count, t.errors, duration_text(t.duration_ms))); }
    }
    if !s.sub.list.is_empty() {
        rows.push(format!("子代理  {}个 · {}次请求 · {}", s.sub.list.len(), s.sub.requests, fmt_tokens(s.sub.total)));
        for t in s.sub.list.iter().take(5) {
            rows.push(format!("  {}  {}{}", plain(if t.task.is_empty() { &t.title } else { &t.task }, 48), fmt_tokens(t.total), if t.active { " · 活跃" } else { "" }));
        }
    }
    if s.code.add.is_some() || s.code.deleted.is_some() || s.code.files.is_some() {
        rows.push(format!("代码改动  +{} / -{} · {}文件", s.code.add.unwrap_or(0), s.code.deleted.unwrap_or(0), s.code.files.unwrap_or(0)));
    }
    if s.remote { rows.push(format!("数据源  远端 · {}", s.remote_host)); }
    if !s.remote_error.is_empty() { rows.push(format!("远端  {}", s.remote_error)); }
    if s.pin_lost { rows.push("原钉住会话已不存在，已回退当前会话".into()); }
    rows.join("\n")
}
fn report_text(r: &UsageReports) -> String {
    let mut rows = vec![format!("今日 {} · 近7天 {} · 近30天 {}", fmt_tokens(r.today.total), fmt_tokens(r.week.total), fmt_tokens(r.month.total)), format!("近{}天每日用量", r.days)];
    for d in &r.daily { rows.push(format!("{}  {}次 · 输入 {} / 输出 {} / 合计 {}", d.date, d.usage.requests, fmt_tokens(d.usage.input), fmt_tokens(d.usage.output), fmt_tokens(d.usage.total))); }
    rows.push("按模型".into());
    for m in &r.models { rows.push(format!("{} / {}  {}次 · {}", m.provider, m.model, m.usage.requests, fmt_tokens(m.usage.total))); }
    rows.push("最近会话".into());
    for s in &r.sessions { rows.push(format!("{}  {}次 · {} · {}", plain(&s.title, 48), s.requests, fmt_tokens(s.total), s.sid)); }
    rows.join("\n")
}

/// 供后台泵持有。读失败丢弃句柄，下次刷新重新打开；闲置及文件替换也会重连。
pub struct DataReader {
    path: Option<PathBuf>, con: Option<Connection>, opened_path: PathBuf,
    file_created: Option<SystemTime>, last_use: Instant, opened: Instant,
    remote: remote::RemoteReader,
}
impl Default for DataReader { fn default() -> Self { Self::new() } }
impl DataReader {
    pub fn new() -> Self { Self { path: None, con: None, opened_path: PathBuf::new(), file_created: None,
        last_use: Instant::now(), opened: Instant::now(), remote: remote::RemoteReader::default() } }
    pub fn with_path(path: impl Into<PathBuf>) -> Self { Self { path: Some(path.into()), ..Self::new() } }
    fn connection(&mut self) -> rusqlite::Result<&Connection> {
        let path = self.path.clone().unwrap_or_else(db_path);
        let created = std::fs::metadata(&path).ok().and_then(|m| m.created().ok());
        if self.opened_path != path || self.file_created != created || self.last_use.elapsed() > Duration::from_secs(60)
            || self.opened.elapsed() > Duration::from_secs(300) { self.con = None; }
        if self.con.is_none() {
            self.con = Some(connect_readonly_at(&path)?); self.opened_path = path;
            self.file_created = created; self.opened = Instant::now();
        }
        self.last_use = Instant::now();
        Ok(self.con.as_ref().unwrap())
    }
    pub fn snapshot(&mut self, default_window: i64, sid: &str) -> Snapshot {
        let out = match self.connection() {
            Ok(con) => snapshot_for(con, default_window, sid),
            Err(e) => Snapshot { error: e.to_string(), ..Default::default() },
        };
        if !out.ok { self.con = None; clear_cache(); }
        out
    }
    pub fn snapshot_remote(&mut self, default_window: i64, sid: &str, config: &RemoteConfig) -> Snapshot {
        let local = self.snapshot(default_window, sid);
        self.remote.merge(local, sid, config)
    }
    pub fn session_list(&mut self, limit: usize) -> rusqlite::Result<Vec<SessionInfo>> {
        let out = self.connection().and_then(|con| session_list(con, limit));
        if out.is_err() { self.con = None; clear_cache(); }
        out
    }
    pub fn reports(&mut self, days: usize) -> rusqlite::Result<UsageReports> {
        let out = self.connection().and_then(|con| reports(con, days));
        if out.is_err() { self.con = None; clear_cache(); }
        out
    }
}

#[cfg(test)]
#[path = "data_tests.rs"]
mod tests;
