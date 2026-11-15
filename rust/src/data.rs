//! 数据层：只读访问 ~/.zcode/cli/db/db.sqlite，组装条面快照。
//!
//! 口径与 Python 版 zusage.py 完全一致（逐条 SQL 对照翻译），差异只在语言：
//!
//! * Python 的 `sqlite3.Row` → rusqlite 的 `Row::get`（按下标/列名取值）。
//! * Python 的 `coalesce(sum(x),0)` 原样保留；rusqlite 取出的整数用
//!   `i64` 承接——SQLite 整数就是 64 位，Python 里随手是 int，Rust 里
//!   必须说清楚宽度，这也是这次重写「显式化」的典型例子。
//! * 连接用 URI `mode=ro` + `query_only` 双保险：悬浮条永远不写 ZCode 库。
//!
//! 泵（pump）模型：后台线程持有唯一的 `Connection`（rusqlite 的
//! `Connection` 是 `Send` 但不是 `Sync`——意思是它可以整体搬给另一个
//! 线程，但不能多线程同时用。所以「一个专用线程 + 通道回传快照」正合适）。

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

/// 库路径：~/.zcode/cli/db/db.sqlite（与 Python 版同源）。
pub fn db_path() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    std::path::Path::new(&home).join(".zcode").join("cli").join("db").join("db.sqlite")
}

/// 最近 30 分钟有请求视为活跃会话（口径同 Python SESSION_TIMEOUT_MS）。
const SESSION_TIMEOUT_MS: i64 = 30 * 60 * 1000;
/// 上下文容量标准档：用量超过已知窗口时向上取档（_infer_window）。
const WINDOW_TIERS: [i64; 7] = [128_000, 200_000, 256_000, 400_000, 512_000, 1_000_000, 2_000_000];

/// 打开严格只读连接。失败就报错，绝不回退成可写连接：
/// `mode=ro` 防止库不存在时被创建空库，`query_only` 再加一道写保护。
pub fn connect_readonly() -> rusqlite::Result<Connection> {
    let path = db_path();
    let uri = format!("file:{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let con = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    con.execute_batch("pragma query_only=on")?;
    // 探针：表不在（老版本/空库）立刻报错，让上层显示「无数据」而不是反复炸。
    con.query_row("select 1 from model_usage limit 1", [], |_| Ok(()))?;
    Ok(con)
}

// ── 结构：字段名与 Python serve 的 JSON 载荷一一对应 ─────────────────

/// 会话级 completed 聚合（session_usage 的返回）。
#[derive(Clone, Default, Serialize)]
pub struct SessionStats {
    pub requests: i64,
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub total: i64,
    pub tool_calls: i64,
    pub last_request_input: i64,
    pub turns: i64,
    pub retries: i64,
}

/// 本轮（turn）聚合：优先 turn_usage 表，回退 model_usage 现场聚合。
#[derive(Clone, Default, Serialize)]
pub struct TurnStats {
    pub requests: i64,
    pub retries: i64,
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub total: i64,
    pub duration_ms: i64,
    pub ttft_ms: i64,
}

/// 最近一次请求（tps 计算的原料）。
#[derive(Clone, Default, Serialize)]
pub struct LastRequest {
    pub duration_ms: i64,
    pub ttft_ms: i64,
    pub model: String,
    pub tps: f64,
}

/// 区间聚合（今日用量）。
#[derive(Clone, Default, Serialize)]
pub struct RangeUsage {
    pub requests: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub total: i64,
}

/// 子代理汇总（第一版只给计数与总量，明细列表后续会话补）。
#[derive(Clone, Default, Serialize)]
pub struct SubSummary {
    pub requests: i64,
    pub total: i64,
    pub active: bool,
}

/// 条面快照：UI 每 1.5 秒拿到一份，字段名对齐 Python serve 的 JSON。
#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub ok: bool,
    pub error: String,
    pub sid: String,
    pub title: String,
    pub directory: String,
    pub active: bool,
    pub generating: bool,
    pub turns: i64,
    pub requests: i64,
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub total: i64,
    pub tool_calls: i64,
    pub retries: i64,
    /// 上下文占用 = 最近一次 completed 请求的 input_tokens（峰值会回落，见 v28 修复）。
    pub ctx: i64,
    pub updated: String,
    pub ctx_exc: i64,
    pub context_window: i64,
    pub context_auto: bool,
    pub last_at: i64,
    pub is_sub: bool,
    pub last_turn: TurnStats,
    pub last: LastRequest,
    pub tools_total: i64,
    pub tools_errors: i64,
    pub sub: SubSummary,
    pub today: RangeUsage,
    /// 条面「今日/会话」等格式的缩写（12.3K / 4.5M），与 Python fmt() 同规则。
    pub today_text: String,
    pub total_text: String,
}

/// 数字缩写：≥1B/1M/1K 用 B/M/K，其余原样。照抄 Python fmt()。
pub fn fmt_tokens(n: i64) -> String {
    let n = n.max(0);
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn session_usage(con: &Connection, sid: &str) -> rusqlite::Result<SessionStats> {
    // last_request_input 取最近一次 completed 请求的 input_tokens（子查询）。
    // 不能用 max(input_tokens)：会话压缩后 input 骤降，峰值永不回落（v28）。
    con.query_row(
        "select count(*), coalesce(sum(input_tokens),0), coalesce(sum(output_tokens),0),
                coalesce(sum(reasoning_tokens),0), coalesce(sum(cache_read_input_tokens),0),
                coalesce(sum(cache_creation_input_tokens),0), coalesce(sum(computed_total_tokens),0),
                coalesce(sum(tool_call_count),0),
                (select m2.input_tokens from model_usage m2
                  where m2.session_id = m.session_id and m2.status = 'completed'
                  order by m2.completed_at desc, m2.id desc limit 1),
                count(distinct turn_id), coalesce(sum(retry_count),0)
         from model_usage m where m.session_id=?1 and status='completed'",
        [sid],
        |r| {
            Ok(SessionStats {
                requests: r.get(0)?,
                input: r.get(1)?,
                output: r.get(2)?,
                reasoning: r.get(3)?,
                cache_read: r.get(4)?,
                cache_write: r.get(5)?,
                total: r.get(6)?,
                tool_calls: r.get(7)?,
                last_request_input: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                turns: r.get(9)?,
                retries: r.get(10)?,
            })
        },
    )
}

fn range_usage(con: &Connection, start_ms: i64, end_ms: i64) -> rusqlite::Result<RangeUsage> {
    con.query_row(
        "select count(*), coalesce(sum(input_tokens),0), coalesce(sum(output_tokens),0),
                coalesce(sum(cache_read_input_tokens),0), coalesce(sum(computed_total_tokens),0)
         from model_usage where status='completed' and completed_at between ?1 and ?2",
        [start_ms, end_ms],
        |r| {
            Ok(RangeUsage {
                requests: r.get(0)?,
                input: r.get(1)?,
                output: r.get(2)?,
                cache_read: r.get(3)?,
                total: r.get(4)?,
            })
        },
    )
}

/// 全库 session_id → max(completed_at)（731MB 库 ~55ms，泵 1.5s 一次可承受；
/// Python 版对这步做了 2s 短缓存，Rust 版同样加一层，见 `Pump`）。
fn scan_session_ids(con: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = con.prepare(
        "select session_id, max(completed_at) as m from model_usage group by session_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// 当前会话 = 用户正在打字的会话（input_history 末行）。
/// 用户不会往子代理里打字，所以后台子代理跑得再欢也拽不走条面（v70 口径）。
fn latest_typed_sid(con: &Connection) -> Option<String> {
    // input_history 末行；三张候选表按 Python 版顺序（h → m → 失败回 None）。
    let try_query = |sql: &str| -> Option<String> {
        con.query_row(sql, [], |r| r.get::<_, Option<String>>(0)).ok().flatten()
    };
    try_query(
        "select h.session_id from input_history h
         join session s on s.id = h.session_id
         order by h.time_created desc limit 1",
    )
    .or_else(|| {
        try_query(
            "select m.session_id from message m
             join session s on s.id = m.session_id
             where json_extract(m.data,'$.role')='user'
             order by m.time_created desc limit 1",
        )
    })
}

fn today_range_ms() -> (i64, i64) {
    // 今日零点（本地时区）→ 明日零点。直接用 chrono 太重，手算：
    // 用 chrono 不算重，但第一版保持零额外依赖，用 std 只能绕——
    // 这里其实需要时区，还是引 chrono 最稳（下一版再收）。
    // 简化实现：用 `windows` 无关、纯 chrono 的 Local。
    use chrono::{Duration, Local};
    let now = Local::now();
    let day_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let day_start = day_start.and_local_timezone(Local).single().unwrap_or_else(Local::now);
    let start = day_start.timestamp_millis();
    (start, start + Duration::days(1).num_milliseconds())
}

/// 组装一份条面快照。任何一步出错都返回 `ok=false` 的合法快照（UI 显示「无数据」），
/// 与 Python 版「缺表/空库不炸条面」的行为一致。
pub fn snapshot(con: &Connection, default_window: i64) -> Snapshot {
    match snapshot_inner(con, default_window) {
        Ok(mut s) => {
            s.ok = true;
            s.today_text = fmt_tokens(s.today.total);
            s.total_text = fmt_tokens(s.total);
            s
        }
        Err(e) => Snapshot {
            ok: false,
            error: e.to_string(),
            ..Default::default()
        },
    }
}

fn snapshot_inner(con: &Connection, default_window: i64) -> Result<Snapshot, rusqlite::Error> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let typed = latest_typed_sid(con);
    let rows = scan_session_ids(con)?;

    // 选会话：打字会话优先；否则取最近活跃。
    let (sid, last_at): (String, i64) = match typed {
        Some(sid) => {
            let la: i64 = con
                .query_row(
                    "select coalesce(max(completed_at),0) from model_usage where session_id=?1",
                    [&sid],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (sid, la)
        }
        None => match rows.iter().max_by_key(|(_, m)| *m) {
            Some((sid, m)) => (sid.clone(), *m),
            None => return Ok(Snapshot::default()), // 全库无任何用量
        },
    };

    let u = session_usage(con, &sid)?;
    let (title, directory): (String, String) = con.query_row(
        "select coalesce(title,''), coalesce(directory,'') from session where id=?1",
        [&sid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // 最近一次 completed 请求：tps 与「本轮」的原料。
    let last = con
        .query_row(
            "select coalesce(duration_ms,0), coalesce(time_to_first_token_ms,0), completed_at,
                    turn_id, coalesce(model_id,''), coalesce(output_tokens,0), first_token_at
             from model_usage where session_id=?1 and status='completed'
             order by completed_at desc limit 1",
            [&sid],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .unwrap_or_default();
    let (last_dur, last_ttft, _last_at2, last_turn_id, last_model, last_out, first_token_at) = last;

    // 生成速度：首 token 之后到完成的输出速率；无 first_token_at 退化为总耗时。
    let tps = if last_out > 0 {
        let gen_ms = match (first_token_at, last_at) {
            (Some(f), _) if f > 0 && last_at > f => Some(last_at - f),
            _ if last_dur > 0 => Some(last_dur),
            _ => None,
        };
        gen_ms.map(|ms| (last_out as f64 / (ms as f64 / 1000.0) * 10.0).round() / 10.0).unwrap_or(0.0)
    } else {
        0.0
    };

    // 本轮：turn_usage 已完成的行；进行中（还没落 completed）回退 model_usage 现场聚合。
    let mut last_turn = TurnStats::default();
    if let Some(turn_id) = last_turn_id.as_deref() {
        let got = con
            .query_row(
                "select coalesce(model_request_count,0), coalesce(model_retry_count,0),
                        coalesce(tool_call_count,0), coalesce(tool_error_count,0),
                        coalesce(input_tokens,0), coalesce(output_tokens,0), coalesce(reasoning_tokens,0),
                        coalesce(cache_read_input_tokens,0), coalesce(cache_creation_input_tokens,0),
                        coalesce(computed_total_tokens,0),
                        coalesce(duration_ms,0), coalesce(time_to_first_token_ms,0)
                 from turn_usage where session_id=?1 and turn_id=?2 and status='completed'",
                [&sid, turn_id],
                |r| {
                    Ok(TurnStats {
                        requests: r.get(0)?,
                        retries: r.get(1)?,
                        tool_calls: r.get(2)?,
                        tool_errors: r.get(3)?,
                        input: r.get(4)?,
                        output: r.get(5)?,
                        reasoning: r.get(6)?,
                        cache_read: r.get(7)?,
                        cache_write: r.get(8)?,
                        total: r.get(9)?,
                        duration_ms: r.get(10)?,
                        ttft_ms: r.get(11)?,
                    })
                },
            )
            .ok();
        last_turn = got.unwrap_or_else(|| {
            con.query_row(
                "select count(*), coalesce(sum(input_tokens),0), coalesce(sum(output_tokens),0),
                        coalesce(sum(computed_total_tokens),0), coalesce(sum(duration_ms),0),
                        coalesce(sum(cache_read_input_tokens),0), coalesce(sum(reasoning_tokens),0),
                        coalesce(sum(cache_creation_input_tokens),0)
                 from model_usage where session_id=?1 and turn_id=?2 and status='completed'",
                [&sid, turn_id],
                |r| {
                    Ok(TurnStats {
                        requests: r.get(0)?,
                        input: r.get(1)?,
                        output: r.get(2)?,
                        total: r.get(3)?,
                        duration_ms: r.get(4)?,
                        cache_read: r.get(5)?,
                        reasoning: r.get(6)?,
                        cache_write: r.get(7)?,
                        ..Default::default()
                    })
                },
            )
            .unwrap_or_default()
        });
    }

    // 活跃：30 分钟内有请求；刚发消息还没落库的窗口期，最新 message 是
    // user 就算「生成中」（15 分钟上限兜底孤儿消息，v70 口径）。
    let active = last_at > 0 && now_ms - last_at < SESSION_TIMEOUT_MS;
    let mut generating = false;
    if !active {
        let mr: Option<(i64, String)> = con
            .query_row(
                "select time_created, coalesce(data,'') from message
                 where session_id=?1 order by time_created desc limit 1",
                [&sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((t, data)) = mr {
            if t > 0 && now_ms - t < 15 * 60_000 {
                let role = serde_json::from_str::<serde_json::Value>(&data)
                    .ok()
                    .and_then(|v| v.get("role").and_then(|r| r.as_str()).map(|s| s.to_string()))
                    .unwrap_or_default();
                generating = role == "user";
            }
        }
    }

    // 上下文容量：已知窗口装不下观测用量 → 向上取标准档（反代模型目录查不到，
    // 但用量不会骗人）。第一版没有模型目录，known=配置默认值。
    let used = u.last_request_input;
    let mut window = default_window;
    if used > window {
        window = WINDOW_TIERS
            .iter()
            .copied()
            .find(|t| *t >= used)
            .unwrap_or((used / 1_000_000 + 1) * 1_000_000);
    }

    // 上下文超限告警：最近一次 context_exceeded 的请求晚于最近成功请求 = 仍超限中。
    let ctx_exc: i64 = con
        .query_row(
            "select coalesce(max(coalesce(completed_at, started_at)),0)
             from model_usage where session_id=?1 and context_exceeded=1",
            [&sid],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // 工具计数（本会话）。
    let (tools_total, tools_errors): (i64, i64) = con
        .query_row(
            "select coalesce(count(*),0), coalesce(sum(case when status='error' then 1 else 0 end),0)
             from tool_usage where session_id=?1 and status in ('completed','error')",
            [&sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    // 子代理汇总（明细列表后续补）。
    let sub = con
        .query_row(
            "select coalesce(count(*),0), coalesce(sum(m.computed_total_tokens),0),
                    coalesce(max(m.completed_at),0)
             from model_usage m join session s2 on m.session_id = s2.id
             where m.query_source='subagent' and s2.parent_id=?1 and m.status='completed'",
            [&sid],
            |r| {
                let total: i64 = r.get(1)?;
                let last: i64 = r.get(2)?;
                Ok(SubSummary {
                    requests: r.get(0)?,
                    total,
                    active: last > 0 && now_ms - last < 30_000,
                })
            },
        )
        .unwrap_or_default();

    let (t0, t1) = today_range_ms();
    let today = range_usage(con, t0, t1)?;

    let updated = if last_at > 0 {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_millis_opt(last_at)
            .single()
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };

    let is_sub = sid.starts_with("sess_subagent");
    Ok(Snapshot {
        ok: true,
        error: String::new(),
        sid,
        title,
        directory,
        active,
        generating,
        turns: u.turns,
        requests: u.requests,
        input: u.input,
        output: u.output,
        reasoning: u.reasoning,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
        total: u.total,
        tool_calls: u.tool_calls,
        retries: u.retries,
        ctx: used,
        updated,
        ctx_exc,
        context_window: window,
        context_auto: true,
        last_at,
        is_sub,
        last_turn,
        last: LastRequest {
            duration_ms: last_dur,
            ttft_ms: last_ttft,
            model: last_model,
            tps,
        },
        tools_total,
        tools_errors,
        sub,
        today,
        today_text: String::new(),
        total_text: String::new(),
    })
}
