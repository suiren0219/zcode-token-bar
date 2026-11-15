//! 可选表/列集中适配，SQL 错误只对真正的缺表降级，锁与损坏仍向上传递。
use super::*;
use rusqlite::{params, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Schema { tables: HashMap<String, HashSet<String>> }
impl Schema {
    pub fn load(con: &Connection) -> rusqlite::Result<Self> {
        // schema_version 改变才重新探表；避免每帧 PRAGMA table_info。
        let version: i64 = con.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
        cached(con, &format!("schema:{version}"), Duration::from_secs(3600), || {
            let mut tables = HashMap::new();
            for table in ["session", "message", "input_history", "model_usage", "turn_usage", "tool_usage", "part"] {
                let mut stmt = con.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
                let cols = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<HashSet<_>>>()?;
                if !cols.is_empty() { tables.insert(table.into(), cols); }
            }
            Ok(Self { tables })
        })
    }
    fn has(&self, table: &str, col: &str) -> bool { self.tables.get(table).is_some_and(|s| s.contains(col)) }
    fn has_all(&self, table: &str, cols: &[&str]) -> bool { cols.iter().all(|c| self.has(table, c)) }
    fn col(&self, table: &str, alias: &str, col: &str, fallback: &str) -> String {
        if self.has(table, col) { format!("{alias}.\"{col}\"") } else { fallback.into() }
    }
    fn num(&self, table: &str, alias: &str, col: &str) -> String { format!("coalesce({},0)", self.col(table, alias, col, "0")) }
    fn text(&self, table: &str, alias: &str, col: &str) -> String { format!("coalesce({},'')", self.col(table, alias, col, "''")) }
    fn visible(&self, alias: &str, roots: bool) -> String {
        let mut parts = vec![format!("{} IS NULL", self.col("session", alias, "time_deleted", "NULL"))];
        if roots {
            parts.push(format!("{}=''", self.text("session", alias, "parent_id")));
            parts.push(format!("substr({alias}.id,1,13)<>'sess_subagent'"));
        }
        parts.join(" AND ")
    }
}

fn usage_columns(s: &Schema, table: &str, alias: &str) -> String {
    let mut cols = vec!["count(*)".to_string()];
    for name in ["input_tokens", "output_tokens", "cache_read_input_tokens", "computed_total_tokens",
        "reasoning_tokens", "cache_creation_input_tokens", "retry_count", "duration_ms"] {
        cols.push(format!("coalesce(sum({}),0)", s.num(table, alias, name)));
    }
    cols.join(",")
}
fn range_row(r: &Row<'_>, off: usize) -> rusqlite::Result<RangeUsage> {
    Ok(RangeUsage { requests: r.get(off)?, input: r.get(off+1)?, output: r.get(off+2)?,
        cache_read: r.get(off+3)?, total: r.get(off+4)?, reasoning: r.get(off+5)?,
        cache_write: r.get(off+6)?, retries: r.get(off+7)?, duration_ms: r.get(off+8)? })
}

pub(super) fn session_exists(con: &Connection, s: &Schema, sid: &str) -> rusqlite::Result<bool> {
    if s.has("session", "id") {
        let deleted = s.col("session", "s", "time_deleted", "NULL");
        let row: Option<bool> = con.query_row(&format!("SELECT {deleted} IS NULL FROM session s WHERE id=?1"), [sid], |r| r.get(0)).optional()?;
        // 已软删除 session 的历史 usage 不应使钉住复活。
        if let Some(found) = row { return Ok(found); }
    }
    if !s.has("model_usage", "session_id") { return Ok(false); }
    con.query_row("SELECT EXISTS(SELECT 1 FROM model_usage WHERE session_id=?1)", [sid], |r| r.get(0))
}

pub(super) fn current_sid(con: &Connection, s: &Schema) -> rusqlite::Result<Option<String>> {
    let typed = cached(con, "current_sid", Duration::from_millis(500), || {
        let has_sessions = s.has("session", "id");
        if s.has_all("message", &["session_id", "time_created", "data"]) && has_sessions {
            // 第一层只扫覆盖索引，第二层在候选 session_id 上取用户消息；无逐会话 N+1。
            let id = s.col("message", "m", "id", "''");
            let query = format!("WITH candidates AS (
                SELECT m.session_id, max(m.time_created) t FROM message m
                JOIN session s ON s.id=m.session_id WHERE {}
                GROUP BY m.session_id ORDER BY t DESC,m.session_id DESC LIMIT 6)
                SELECT m.session_id FROM candidates c JOIN message m ON m.session_id=c.session_id
                WHERE json_extract(CASE WHEN json_valid(m.data) THEN m.data ELSE '{{}}' END,'$.role')='user'
                ORDER BY m.time_created DESC,{id} DESC LIMIT 1", s.visible("s", true));
            let hit: Option<String> = con.query_row(&query, [], |r| r.get(0)).optional()?;
            if let Some(sid) = hit.filter(|v| valid_sid(v)) { return Ok(Some(sid)); }
        }
        if s.has_all("input_history", &["session_id", "time_created"]) && has_sessions {
            let id = s.col("input_history", "h", "id", "''");
            let query = format!("SELECT h.session_id FROM input_history h JOIN session s ON s.id=h.session_id
                WHERE {} ORDER BY h.time_created DESC,{id} DESC LIMIT 1", s.visible("s", true));
            let hit: Option<String> = con.query_row(&query, [], |r| r.get(0)).optional()?;
            if let Some(sid) = hit.filter(|v| valid_sid(v)) { return Ok(Some(sid)); }
        }
        Ok(None)
    })?;
    if typed.is_some() { return Ok(typed); }
    let has_sessions = s.has("session", "id");
    cached(con, "fallback_session", Duration::from_secs(6), || {
        if s.has("model_usage", "session_id") {
            let at = format!("coalesce({},{},0)", s.col("model_usage", "m", "completed_at", "NULL"), s.col("model_usage", "m", "started_at", "NULL"));
            let join = if has_sessions { "LEFT JOIN session s ON s.id=m.session_id" } else { "" };
            let visible = if has_sessions { format!("{} AND substr(m.session_id,1,13)<>'sess_subagent'", s.visible("s", true).replace("substr(s.id,1,13)<>'sess_subagent'", "coalesce(substr(s.id,1,13),'')<>'sess_subagent'")) }
                else { "substr(m.session_id,1,13)<>'sess_subagent'".into() };
            let hit: Option<String> = con.query_row(&format!("SELECT m.session_id FROM model_usage m {join}
                WHERE {visible} GROUP BY m.session_id ORDER BY max({at}) DESC,m.session_id DESC LIMIT 1"), [], |r| r.get(0)).optional()?;
            if let Some(sid) = hit.filter(|v| valid_sid(v)) { return Ok(Some(sid)); }
        }
        if has_sessions {
            let time = s.num("session", "s", "time_updated");
            return con.query_row(&format!("SELECT s.id FROM session s WHERE {} ORDER BY {time} DESC,s.id DESC LIMIT 1", s.visible("s", true)), [], |r| r.get::<_, String>(0)).optional()
                .map(|v| v.filter(|id| valid_sid(id)));
        }
        Ok(None)
    })
}

#[derive(Default)]
pub(super) struct Meta {
    pub title: String, pub directory: String, pub parent: Option<String>,
    pub code: CodeSummary, pub reverted: bool, pub deleted: bool,
}
pub(super) fn session_meta(con: &Connection, s: &Schema, sid: &str) -> rusqlite::Result<Meta> {
    if !s.has("session", "id") { return Ok(Meta::default()); }
    let cols = [s.text("session", "s", "title"), s.text("session", "s", "directory"),
        s.col("session", "s", "parent_id", "NULL"), s.col("session", "s", "summary_additions", "NULL"),
        s.col("session", "s", "summary_deletions", "NULL"), s.col("session", "s", "summary_files", "NULL"),
        format!("{} IS NOT NULL", s.col("session", "s", "revert", "NULL")),
        format!("{} IS NOT NULL", s.col("session", "s", "time_deleted", "NULL"))].join(",");
    Ok(con.query_row(&format!("SELECT {cols} FROM session s WHERE id=?1"), [sid], |r| Ok(Meta {
        title: r.get(0)?, directory: r.get(1)?, parent: r.get(2)?,
        code: CodeSummary { add:r.get(3)?, deleted:r.get(4)?, files:r.get(5)? }, reverted:r.get(6)?, deleted:r.get(7)?,
    })).optional()?.unwrap_or_default())
}
pub(super) fn session_usage(con: &Connection, s: &Schema, sid: &str) -> rusqlite::Result<SessionStats> {
    if !s.has_all("model_usage", &["session_id", "status"]) { return Ok(SessionStats::default()); }
    cached(con, &format!("session_usage:{sid}"), Duration::from_millis(500), || {
        let cols = usage_columns(s, "model_usage", "m");
        let turns = s.col("model_usage", "m", "turn_id", "NULL"); let tools = s.num("model_usage", "m", "tool_call_count");
        con.query_row(&format!("SELECT {cols},count(DISTINCT {turns}),coalesce(sum({tools}),0)
            FROM model_usage m WHERE session_id=?1 AND status='completed'"), [sid], |r| {
            let v = range_row(r,0)?;
            Ok(SessionStats { requests:v.requests, input:v.input, output:v.output, cache_read:v.cache_read,
                cache_write:v.cache_write, total:v.total, reasoning:v.reasoning, retries:v.retries,
                turns:r.get(9)?, tool_calls:r.get(10)?, last_request_input:0 })
        })
    })
}

#[derive(Clone, Debug, Default)]
pub(super) struct Request {
    pub turn_id:String, pub model:String, pub provider:String, pub status:String,
    pub duration_ms:i64, pub ttft_ms:i64, pub completed_at:i64, pub started_at:i64,
    pub first_token_at:i64, pub input:i64, pub output:i64,
}
pub(super) fn last_request(con:&Connection, s:&Schema, sid:&str, completed:bool) -> rusqlite::Result<Request> {
    if !s.has_all("model_usage", &["session_id", "status"]) { return Ok(Request::default()); }
    let mut cols:Vec<String> = ["turn_id","model_id","provider_id","status"].iter().map(|c| s.text("model_usage","m",c)).collect();
    cols.extend(["duration_ms","time_to_first_token_ms","completed_at","started_at","first_token_at","input_tokens","output_tokens"].iter().map(|c| s.num("model_usage","m",c)));
    let filter = if completed { "AND status='completed'" } else { "" };
    let order = if completed { s.num("model_usage","m","completed_at") } else {
        format!("coalesce({},{},0)", s.col("model_usage","m","started_at","NULL"),s.col("model_usage","m","completed_at","NULL")) };
    let id = s.col("model_usage","m","id","''");
    Ok(con.query_row(&format!("SELECT {} FROM model_usage m WHERE session_id=?1 {filter} ORDER BY {order} DESC,{id} DESC LIMIT 1",cols.join(",")),[sid],|r| Ok(Request {
        turn_id:r.get(0)?,model:r.get(1)?,provider:r.get(2)?,status:r.get(3)?,duration_ms:r.get(4)?,ttft_ms:r.get(5)?,
        completed_at:r.get(6)?,started_at:r.get(7)?,first_token_at:r.get(8)?,input:r.get(9)?,output:r.get(10)?,
    })).optional()?.unwrap_or_default())
}
#[derive(Default)]
pub(super) struct MessageState { pub user_at:i64, pub latest_role:String }
pub(super) fn message_state(con:&Connection,s:&Schema,sid:&str) -> rusqlite::Result<MessageState> {
    if !s.has_all("message", &["session_id","time_created","data"]) { return Ok(MessageState::default()); }
    let id = s.col("message","m","id","''");
    let role = "json_extract(CASE WHEN json_valid(m.data) THEN m.data ELSE '{}' END,'$.role')";
    let user_at = con.query_row(&format!("SELECT coalesce(max(time_created),0) FROM message m WHERE session_id=?1 AND {role}='user'"),[sid],|r|r.get(0))?;
    let latest_role = con.query_row(&format!("SELECT coalesce({role},'') FROM message m WHERE session_id=?1 ORDER BY time_created DESC,{id} DESC LIMIT 1"),[sid],|r|r.get(0)).optional()?.unwrap_or_default();
    Ok(MessageState { user_at,latest_role })
}

pub(super) fn turn(con:&Connection,s:&Schema,sid:&str,last:&Request,unavailable:bool) -> rusqlite::Result<(TurnStats,TurnState)> {
    let mut state = TurnState { session_id:sid.into(), ..Default::default() };
    let mut turn_id = last.turn_id.clone();
    if s.has_all("turn_usage", &["session_id","turn_id","status","started_at"]) {
        let mut cols = ["turn_id","status"].iter().map(|c| s.text("turn_usage","t",c)).collect::<Vec<_>>();
        for c in ["started_at","completed_at","duration_ms","tool_error_count","cancelled_by_user"] { cols.push(s.col("turn_usage","t",c,"NULL")); }
        for c in ["model_request_count","model_retry_count","tool_call_count","tool_error_count","input_tokens","output_tokens","reasoning_tokens","cache_read_input_tokens","cache_creation_input_tokens","computed_total_tokens","duration_ms","time_to_first_token_ms"] { cols.push(s.num("turn_usage","t",c)); }
        let row = con.query_row(&format!("SELECT {} FROM turn_usage t WHERE session_id=?1 ORDER BY started_at DESC,turn_id DESC LIMIT 1",cols.join(",")),[sid],|r| {
            let id:String=r.get(0)?; let status:String=r.get(1)?;
            let started:Option<i64>=r.get(2)?; let completed:Option<i64>=r.get(3)?;
            let duration:Option<i64>=r.get(4)?; let errors:Option<i64>=r.get(5)?; let cancelled:Option<i64>=r.get(6)?;
            let stats=TurnStats {requests:r.get(7)?,retries:r.get(8)?,tool_calls:r.get(9)?,tool_errors:r.get(10)?,input:r.get(11)?,output:r.get(12)?,reasoning:r.get(13)?,cache_read:r.get(14)?,cache_write:r.get(15)?,total:r.get(16)?,duration_ms:r.get(17)?,ttft_ms:r.get(18)?};
            Ok((id,status,started,completed,duration,errors,cancelled,stats))
        }).optional()?;
        if let Some((id,mut status,started,completed,duration,errors,cancelled,stats))=row {
            // 新请求已经开始但 turn_usage 行尚未落盘时，不展示旧轮终态。
            let newer_request = !last.turn_id.is_empty() && last.turn_id!=id && last.started_at>started.unwrap_or(0);
            if !newer_request {
                turn_id=id.clone();
                if cancelled==Some(1) { status="cancelled".into(); }
                let terminal=matches!(status.as_str(),"completed"|"error"|"cancelled");
                let valid = !id.is_empty() && started.is_some_and(|v|v>0) && errors.is_some_and(|v|v>=0)
                    && matches!(cancelled,Some(0|1))
                    && ((status=="running" && completed.unwrap_or(0)==0 && duration.unwrap_or(0)>=0)
                        || (terminal && completed.is_some_and(|v|v>=started.unwrap_or(0)) && duration.is_some_and(|v|v>=0)));
                state=TurnState {available:valid && !unavailable,session_id:sid.into(),turn_id:id,status,
                    started_at:started.unwrap_or(0),completed_at:completed.unwrap_or(0),duration_ms:duration.unwrap_or(0),
                    tool_errors:errors.unwrap_or(0),cancelled_by_user:cancelled==Some(1),is_sub:false};
                if terminal { return Ok((stats,state)); }
            }
        }
    }
    let mut stats=TurnStats::default();
    if !turn_id.is_empty() && s.has_all("model_usage", &["session_id","turn_id","status"]) {
        let cols=usage_columns(s,"model_usage","m"); let tools=s.num("model_usage","m","tool_call_count");
        let ttft=s.col("model_usage","m","first_token_at","NULL"); let started=s.col("model_usage","m","started_at","NULL");
        stats=con.query_row(&format!("SELECT {cols},coalesce(sum({tools}),0),coalesce(min({ttft})-min({started}),0)
            FROM model_usage m WHERE session_id=?1 AND turn_id=?2 AND status='completed'"),[sid,&turn_id],|r|{
            let v=range_row(r,0)?; Ok(TurnStats {requests:v.requests,retries:v.retries,tool_calls:r.get(9)?,tool_errors:0,input:v.input,output:v.output,reasoning:v.reasoning,cache_read:v.cache_read,cache_write:v.cache_write,total:v.total,duration_ms:v.duration_ms,ttft_ms:r.get::<_,i64>(10)?.max(0)})
        })?;
        if s.has_all("tool_usage", &["session_id","turn_id","status"]) {
            let (count,errors):(i64,i64)=con.query_row("SELECT count(*),coalesce(sum(status='error'),0) FROM tool_usage WHERE session_id=?1 AND turn_id=?2 AND status IN ('completed','error')",[sid,&turn_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            stats.tool_calls=count; stats.tool_errors=errors;
        }
    }
    if state.available && state.status=="running" {
        stats.duration_ms=(now_ms()-state.started_at).max(0);
    }
    Ok((stats,state))
}

pub(super) fn context_error(con:&Connection,s:&Schema,sid:&str)->rusqlite::Result<i64>{
    if !s.has_all("model_usage", &["session_id","context_exceeded"]) {return Ok(0);}
    con.query_row(&format!("SELECT coalesce(max(coalesce({},{},0)),0) FROM model_usage m WHERE session_id=?1 AND context_exceeded=1",
        s.col("model_usage","m","completed_at","NULL"),s.col("model_usage","m","started_at","NULL")),[sid],|r|r.get(0))
}
pub(super) fn tools(con:&Connection,s:&Schema,sid:&str)->rusqlite::Result<ToolSummary>{
    if !s.has_all("tool_usage", &["session_id","tool_name","status"]) {return Ok(ToolSummary::default());}
    cached(con,&format!("tools:{sid}"),Duration::from_secs(2),||{
        let dur=s.num("tool_usage","t","duration_ms");
        let mut stmt=con.prepare(&format!("SELECT coalesce(tool_name,''),count(*),coalesce(sum({dur}),0),coalesce(sum(status='error'),0)
            FROM tool_usage t WHERE session_id=?1 AND status IN ('completed','error') GROUP BY tool_name ORDER BY count(*) DESC,tool_name"))?;
        let list=stmt.query_map([sid],|r|Ok(ToolInfo{name:r.get(0)?,count:r.get(1)?,duration_ms:r.get(2)?,errors:r.get(3)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ToolSummary {total:list.iter().map(|v|v.count).sum(),errors:list.iter().map(|v|v.errors).sum(),list})
    })
}

pub(super) fn subagents(con:&Connection,s:&Schema,sid:&str,now:i64)->rusqlite::Result<SubSummary>{
    if !s.has_all("session", &["id","parent_id"]) {return Ok(SubSummary::default());}
    let mut result:SubSummary=cached(con,&format!("sub:{sid}"),Duration::from_secs(2),||{
        let title=s.text("session","c","title");
        let mut list=if s.has_all("model_usage", &["session_id","status"]) {
            let source=if s.has("model_usage","query_source") {"AND m.query_source='subagent'"} else {""};
            let sums=["computed_total_tokens","input_tokens","output_tokens","cache_read_input_tokens","reasoning_tokens","cache_creation_input_tokens"].iter().map(|c|format!("coalesce(sum({}),0)",s.num("model_usage","m",c))).collect::<Vec<_>>().join(",");
            let at=s.num("model_usage","m","completed_at"); let agent=s.text("model_usage","m","agent");
            let mut stmt=con.prepare(&format!("SELECT c.id,{title},coalesce(max({agent}),''),count(m.session_id),{sums},coalesce(max({at}),0)
                FROM session c LEFT JOIN model_usage m ON m.session_id=c.id AND m.status='completed' {source}
                WHERE c.parent_id=?1 AND {} GROUP BY c.id ORDER BY max({at}) DESC,c.id",s.visible("c",false)))?;
            let rows=stmt.query_map([sid],|r|Ok(SubInfo{sid:r.get(0)?,title:r.get(1)?,agent:r.get(2)?,requests:r.get(3)?,total:r.get(4)?,input:r.get(5)?,output:r.get(6)?,cache_read:r.get(7)?,reasoning:r.get(8)?,cache_write:r.get(9)?,last:r.get(10)?,..Default::default()}))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut stmt=con.prepare(&format!("SELECT c.id,{title} FROM session c WHERE parent_id=?1 AND {}",s.visible("c",false)))?;
            let rows=stmt.query_map([sid],|r|Ok(SubInfo{sid:r.get(0)?,title:r.get(1)?,..Default::default()}))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if !list.is_empty() {
            let tasks=sub_tasks(con,s,sid,&list)?;
            for row in &mut list {row.task=tasks.get(&row.sid).cloned().unwrap_or_default();}
        }
        Ok(SubSummary {requests:list.iter().map(|v|v.requests).sum(),total:list.iter().map(|v|v.total).sum(),input:list.iter().map(|v|v.input).sum(),output:list.iter().map(|v|v.output).sum(),cache_read:list.iter().map(|v|v.cache_read).sum(),reasoning:list.iter().map(|v|v.reasoning).sum(),cache_write:list.iter().map(|v|v.cache_write).sum(),active:false,list})
    })?;
    for row in &mut result.list {row.active=recent(row.last,now,30_000);}
    result.active=result.list.iter().any(|r|r.active);
    Ok(result)
}
fn sub_tasks(con:&Connection,s:&Schema,sid:&str,children:&[SubInfo])->rusqlite::Result<HashMap<String,String>>{
    if !s.has_all("part", &["session_id","data"]) {return Ok(HashMap::new());}
    let stamp=s.num("part","p","time_updated");
    let (count,last):(i64,i64)=con.query_row(&format!("SELECT count(*),coalesce(max({stamp}),0) FROM part p WHERE session_id=?1"),[sid],|r|Ok((r.get(0)?,r.get(1)?)))?;
    // part时间戳与子会话列表共同作为失效键；不反复读取大JSON。
    let key=format!("tasks:{sid}:{count}:{last}:{}",children.iter().map(|r|format!("{}:{}",r.sid,r.title)).collect::<Vec<_>>().join("|"));
    cached(con,&key,Duration::from_secs(3600),||{
        let at=s.num("part","p","time_created");
        let mut stmt=con.prepare(&format!("SELECT data FROM part p WHERE session_id=?1 AND data LIKE '%Agent%' ORDER BY {at}"))?;
        let data=stmt.query_map([sid],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut parts=Vec::new();
        for text in data {
            let Ok(v)=serde_json::from_str::<serde_json::Value>(&text) else {continue;};
            if v["tool"].as_str()!=Some("Agent") {continue;}
            let st=&v["state"];let input=&st["input"];
            let desc=input["description"].as_str().unwrap_or("").trim();
            if desc.is_empty(){continue;}
            let output=st["output"].as_str().unwrap_or("");
            let agent=output.lines().find_map(|line|line.trim().strip_prefix("agentId:").and_then(|rest|rest.trim().split(|c:char|c.is_whitespace()||c=='(').next()).filter(|v|valid_sid(v))).map(|id|format!("sess_subagent_{id}"));
            parts.push((desc.to_string(),input["prompt"].as_str().unwrap_or("").trim().to_string(),agent));
        }
        let mut out=HashMap::new();
        for (desc,_,agent) in &parts {if let Some(id)=agent {if children.iter().any(|c|c.sid==*id){out.insert(id.clone(),desc.clone());}}}
        for (desc,prompt,agent) in &parts {
            if agent.is_some(){continue;}
            let matches=children.iter().filter(|c|!out.contains_key(&c.sid)&&prompt_matches(&c.title,prompt)).collect::<Vec<_>>();
            if matches.len()==1 {out.insert(matches[0].sid.clone(),desc.clone());}
        }
        Ok(out)
    })
}
fn prompt_matches(title:&str,prompt:&str)->bool{
    let title=title.trim();if title.is_empty()||prompt.is_empty(){return false;}
    if let Some(base)=title.strip_suffix("..."){base.chars().count()>=15&&prompt.starts_with(base)}else{prompt.starts_with(title)}
}

pub(super) fn daily(con:&Connection,s:&Schema,days:usize)->rusqlite::Result<Vec<DailyUsage>>{
    let mut values:HashMap<String,RangeUsage>=HashMap::new();
    if s.has_all("model_usage", &["status","completed_at"]) {
        let (start,end)=day_bounds(days);let cols=usage_columns(s,"model_usage","m");
        let mut stmt=con.prepare(&format!("SELECT strftime('%Y-%m-%d',completed_at/1000,'unixepoch','localtime') day,{cols}
            FROM model_usage m WHERE status='completed' AND completed_at>=?1 AND completed_at<?2 GROUP BY day"))?;
        let rows=stmt.query_map(params![start,end],|r|Ok((r.get::<_,String>(0)?,range_row(r,1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        values.extend(rows);
    }
    let date=Local::now().date_naive();
    Ok((0..days).rev().map(|i|{let date=(date-chrono::Duration::days(i as i64)).format("%Y-%m-%d").to_string();let usage=values.remove(&date).unwrap_or_default();DailyUsage{date,usage}}).collect())
}
pub(super) fn models(con:&Connection,s:&Schema,days:usize)->rusqlite::Result<Vec<ModelUsage>>{
    if !s.has_all("model_usage", &["status","completed_at"]) {return Ok(Vec::new());}
    let provider=s.text("model_usage","m","provider_id");let model=s.text("model_usage","m","model_id");
    let (start,end)=day_bounds(days);let cols=usage_columns(s,"model_usage","m");
    let mut stmt=con.prepare(&format!("SELECT {provider},{model},{cols} FROM model_usage m
        WHERE status='completed' AND completed_at>=?1 AND completed_at<?2 GROUP BY 1,2 ORDER BY 7 DESC,1,2"))?;
    let rows=stmt.query_map(params![start,end],|r|Ok(ModelUsage{provider:r.get(0)?,model:r.get(1)?,usage:range_row(r,2)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn sessions(con:&Connection,s:&Schema,limit:usize)->rusqlite::Result<Vec<SessionInfo>>{
    if limit==0{return Ok(Vec::new());}
    let model=s.has_all("model_usage", &["session_id","status"]);
    let sess=s.has("session","id");
    if !model&&!sess{return Ok(Vec::new());}
    let mut ctes=Vec::new();
    if model {
        let at=format!("coalesce({},{},0)",s.col("model_usage","m","completed_at","NULL"),s.col("model_usage","m","started_at","NULL"));
        let mut sums=Vec::new();
        for (db,out) in [("input_tokens","input"),("output_tokens","output"),("computed_total_tokens","total"),("cache_read_input_tokens","cache_read"),("cache_creation_input_tokens","cache_write"),("reasoning_tokens","reasoning")] {
            sums.push(format!("coalesce(sum(CASE WHEN status='completed' THEN {} ELSE 0 END),0) AS {out}",s.num("model_usage","m",db)));
        }
        let turn=s.col("model_usage","m","turn_id","NULL");
        ctes.push(format!("u AS (SELECT session_id,max({at}) last_at,sum(status='completed') requests,
            count(DISTINCT CASE WHEN status='completed' THEN {turn} END) turns,{} FROM model_usage m GROUP BY session_id)",sums.join(",")));
    } else {
        ctes.push("u AS (SELECT '' session_id,0 last_at,0 requests,0 turns,0 input,0 output,0 total,0 cache_read,0 cache_write,0 reasoning WHERE 0)".into());
    }
    // 旧库可能保留 usage 却缺 session 行；与 session_exists 的存在性口径一致。
    if sess { ctes.push("ids AS (SELECT id AS sid FROM session UNION SELECT session_id AS sid FROM u)".into()); }
    let from=if sess{"ids i LEFT JOIN session s ON s.id=i.sid LEFT JOIN u ON u.session_id=i.sid"}else{"u"};
    let sid=if sess{"i.sid"}else{"u.session_id"};
    let meta=if sess{[s.text("session","s","title"),s.text("session","s","directory"),s.col("session","s","parent_id","NULL")]}
        else{["''".into(),"''".into(),"NULL".into()]};
    let at=if sess{format!("max(coalesce(u.last_at,0),{})",s.num("session","s","time_updated"))}else{"coalesce(u.last_at,0)".into()};
    let filter=if sess{s.visible("s",false)}else{"1".into()};
    let mut stmt=con.prepare(&format!("WITH {} SELECT {sid},{},{},{},{at},coalesce(u.requests,0),coalesce(u.turns,0),
        coalesce(u.input,0),coalesce(u.output,0),coalesce(u.total,0),coalesce(u.cache_read,0),coalesce(u.cache_write,0),coalesce(u.reasoning,0)
        FROM {from} WHERE {filter} ORDER BY 5 DESC,1 DESC LIMIT ?1",ctes.join(","),meta[0],meta[1],meta[2]))?;
    let rows=stmt.query_map([limit as i64],|r|{
        let sid:String=r.get(0)?;let parent:Option<String>=r.get(3)?;
        let is_sub=sid.starts_with("sess_subagent")||parent.as_ref().is_some_and(|v|!v.is_empty());
        Ok(SessionInfo{sid,title:r.get(1)?,directory:r.get(2)?,parent,is_sub,active:false,last_at:r.get(4)?,requests:r.get(5)?,turns:r.get(6)?,input:r.get(7)?,output:r.get(8)?,total:r.get(9)?,cache_read:r.get(10)?,cache_write:r.get(11)?,reasoning:r.get(12)?})
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// CLI sessions 与 Python recent_sessions 一致：只列有成功请求的会话，按成功完成时间排序。
pub(super) fn recent_completed(con: &Connection, s: &Schema, limit: usize) -> rusqlite::Result<Vec<SessionInfo>> {
    if !s.has_all("model_usage", &["session_id", "status"]) { return Ok(Vec::new()); }
    let meta = s.has("session", "id");
    let join = if meta { "LEFT JOIN session s ON s.id=m.session_id" } else { "" };
    let title = if meta { s.text("session", "s", "title") } else { "''".into() };
    let directory = if meta { s.text("session", "s", "directory") } else { "''".into() };
    let parent = if meta { s.col("session", "s", "parent_id", "NULL") } else { "NULL".into() };
    let at = s.num("model_usage", "m", "completed_at");
    let turn = s.col("model_usage", "m", "turn_id", "NULL");
    let cols = usage_columns(s, "model_usage", "m");
    let mut stmt = con.prepare(&format!("SELECT m.session_id,{title},{directory},{parent},coalesce(max({at}),0),
        {cols},count(DISTINCT {turn}) FROM model_usage m {join} WHERE m.status='completed'
        GROUP BY m.session_id ORDER BY 5 DESC,1 DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], |r| {
        let sid: String = r.get(0)?; let parent: Option<String> = r.get(3)?; let u = range_row(r, 5)?;
        let is_sub = sid.starts_with("sess_subagent") || parent.as_ref().is_some_and(|p| !p.is_empty());
        Ok(SessionInfo { sid, title:r.get(1)?, directory:r.get(2)?, parent, is_sub,
            last_at:r.get(4)?, requests:u.requests, turns:r.get(14)?, input:u.input, output:u.output,
            total:u.total, cache_read:u.cache_read, cache_write:u.cache_write, reasoning:u.reasoning, active:false })
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn session_candidates(con: &Connection, s: &Schema, prefix: &str) -> rusqlite::Result<Vec<String>> {
    if session_exists(con, s, prefix)? { return Ok(vec![prefix.to_string()]); }
    let mut sources = Vec::new();
    if s.has("session", "id") {
        sources.push(format!("SELECT s.id AS sid FROM session s WHERE {}", s.visible("s", false)));
    }
    if s.has("model_usage", "session_id") {
        let visible = if s.has("session", "id") {
            format!(" AND NOT EXISTS(SELECT 1 FROM session s WHERE s.id=m.session_id AND {} IS NOT NULL)", s.col("session", "s", "time_deleted", "NULL"))
        } else { String::new() };
        sources.push(format!("SELECT DISTINCT m.session_id AS sid FROM model_usage m WHERE 1{visible}"));
    }
    if sources.is_empty() { return Ok(Vec::new()); }
    // 不使用 LIKE：SID 中的下划线是字面字符，不能变成通配符。
    let mut stmt = con.prepare(&format!("SELECT sid FROM ({}) WHERE substr(sid,1,length(?1))=?1 ORDER BY sid LIMIT 11", sources.join(" UNION ")))?;
    let rows = stmt.query_map([prefix], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Python models N 使用滚动 N*24 小时，而 days N 使用本地自然日。
pub(super) fn models_since(con: &Connection, s: &Schema, since: i64) -> rusqlite::Result<Vec<ModelUsage>> {
    if !s.has_all("model_usage", &["status", "completed_at"]) { return Ok(Vec::new()); }
    let provider = s.text("model_usage", "m", "provider_id"); let model = s.text("model_usage", "m", "model_id");
    let cols = usage_columns(s, "model_usage", "m");
    let mut stmt = con.prepare(&format!("SELECT {provider},{model},{cols} FROM model_usage m
        WHERE status='completed' AND completed_at>=?1 GROUP BY 1,2 ORDER BY 7 DESC,1,2"))?;
    let rows = stmt.query_map([since], |r| Ok(ModelUsage { provider:r.get(0)?, model:r.get(1)?, usage:range_row(r,2)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn models_for_sessions(con: &Connection, s: &Schema, ids: &[String]) -> rusqlite::Result<Vec<ModelUsage>> {
    if ids.is_empty() || !s.has_all("model_usage", &["session_id", "status"]) { return Ok(Vec::new()); }
    let model = s.text("model_usage", "m", "model_id"); let cols = usage_columns(s, "model_usage", "m");
    let mut stmt = con.prepare(&format!("SELECT {model},{cols} FROM model_usage m
        WHERE status='completed' AND session_id IN (SELECT value FROM json_each(?1)) GROUP BY 1 ORDER BY 6 DESC,1"))?;
    let rows = stmt.query_map([serde_json::to_string(ids).unwrap()], |r| Ok(ModelUsage {
        provider:String::new(), model:r.get(0)?, usage:range_row(r,1)?,
    }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Default)]
pub(super) struct WorkspaceSession {
    pub sid: String, pub title: String, pub parent: String, pub directory: String,
    pub usage: RangeUsage, pub first_at: i64, pub last_at: i64,
}
#[derive(Default)]
pub(super) struct WorkspaceReport {
    pub directories: Vec<(String, usize)>, pub existing: Vec<(String, usize)>,
    pub mains: Vec<WorkspaceSession>, pub children: Vec<WorkspaceSession>, pub models: Vec<ModelUsage>,
}
fn workspace_matches(directory: &str, pattern: &str) -> bool {
    if directory.to_lowercase().contains(pattern) { return true; }
    let initials: String = directory.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|word| word.chars().next()).collect::<String>().to_lowercase();
    pattern.chars().count() >= 2 && initials.contains(pattern)
}
/// 一次短只读事务取齐目录、主/子与用量；事务在渲染/等待前释放。
pub(super) fn workspace(con: &Connection, pattern: &str) -> rusqlite::Result<WorkspaceReport> {
    let tx = con.unchecked_transaction()?;
    let s = Schema::load(&tx)?;
    let mut report = WorkspaceReport::default();
    if !s.has_all("session", &["id", "directory"]) { return Ok(report); }
    let title = s.text("session", "s", "title"); let parent = s.text("session", "s", "parent_id");
    let directory = s.text("session", "s", "directory");
    // 历史工作区统计与 Python 一样包含仍保留在 session 表中的软删除记录。
    let rows = {
        let mut stmt = tx.prepare(&format!("SELECT s.id,{title},{parent},{directory} FROM session s ORDER BY s.id"))?;
        let rows = stmt.query_map([], |r| Ok(WorkspaceSession { sid:r.get(0)?, title:r.get(1)?, parent:r.get(2)?, directory:r.get(3)?, ..Default::default() }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut counts = HashMap::<String, usize>::new();
    for r in &rows { *counts.entry(r.directory.clone()).or_default() += 1; }
    report.existing = counts.into_iter().collect();
    report.existing.sort_by(|a,b| b.1.cmp(&a.1).then_with(||a.0.cmp(&b.0)));
    let pattern = pattern.trim().to_lowercase();
    if !pattern.is_empty() {
        report.directories = report.existing.iter().filter(|(d,_)| workspace_matches(d,&pattern)).cloned().collect();
    }
    let matched: HashSet<_> = report.directories.iter().map(|(d,_)|d.as_str()).collect();
    report.mains = rows.iter().filter(|r| r.parent.is_empty() && !r.sid.starts_with("sess_subagent") && matched.contains(r.directory.as_str())).cloned().collect();
    let mains: HashSet<_> = report.mains.iter().map(|r|r.sid.as_str()).collect();
    report.children = rows.iter().filter(|r|mains.contains(r.parent.as_str())).cloned().collect();
    let ids: Vec<String> = report.mains.iter().chain(&report.children).map(|r|r.sid.clone()).collect();
    if !ids.is_empty() && s.has_all("model_usage", &["session_id", "status"]) {
        let cols = usage_columns(&s,"model_usage","m"); let completed = s.num("model_usage","m","completed_at");
        let mut stmt = tx.prepare(&format!("SELECT session_id,{cols},coalesce(min({completed}),0),coalesce(max({completed}),0)
            FROM model_usage m WHERE status='completed' AND session_id IN (SELECT value FROM json_each(?1)) GROUP BY session_id"))?;
        let per = stmt.query_map([serde_json::to_string(&ids).unwrap()], |r| Ok((r.get::<_,String>(0)?, (range_row(r,1)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?))))?
            .collect::<rusqlite::Result<HashMap<_,_>>>()?;
        for row in report.mains.iter_mut().chain(&mut report.children) {
            if let Some((usage,first,last)) = per.get(&row.sid) { row.usage=usage.clone(); row.first_at=*first; row.last_at=*last; }
        }
        report.models = models_for_sessions(&tx,&s,&ids)?;
    }
    tx.commit()?;
    Ok(report)
}
