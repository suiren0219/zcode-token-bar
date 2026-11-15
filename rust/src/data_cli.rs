//! 与 Python zusage.py 同源的只读文本 CLI；不启动 GUI，也不调用模型/API。
use super::*;
use std::io::{self, IsTerminal, Write};

/// args 不含 argv[0]。空参数交还 GUI；json/serve 继续由原远端协议处理。
pub fn handle_cli(args: &[String]) -> bool {
    if remote::handle_cli(args) { return true; }
    let command = match parse(args) {
        Ok(Some(command)) => command,
        Ok(None) => return false,
        Err(error) => { let _ = writeln!(io::stderr().lock(), "{error}"); return true; }
    };
    let mut reader = DataReader::new();
    let stdout = io::stdout();
    let terminal = stdout.is_terminal();
    let mut output = stdout.lock();
    let result = if let Command::Watch(interval) = command {
        watch(&mut reader, interval, &mut output, terminal)
    } else {
        match render(&mut reader, &command) {
            Ok(text) => write_frame(&mut output, &text),
            Err(error) => { let _ = writeln!(io::stderr().lock(), "{error}"); Ok(()) }
        }
    };
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::BrokenPipe { let _ = writeln!(io::stderr().lock(), "输出失败：{error}"); }
    }
    true
}

#[derive(Debug, PartialEq)]
enum Command { Now, Today, Days(usize), Sessions(usize), Models(usize), Workspace(String), Session(String), Watch(Duration) }
fn parse(args: &[String]) -> Result<Option<Command>, String> {
    let Some(name) = args.first().map(String::as_str) else { return Ok(None); };
    if !matches!(name, "now"|"today"|"days"|"sessions"|"models"|"workspace"|"session"|"watch") { return Ok(None); }
    let usage = "用法：now | today | days [N] | sessions [N] | models [天数] | workspace [目录关键词] | session <SID或唯一前缀> | watch [秒]";
    let max_args = if matches!(name,"now"|"today") { 1 } else { 2 };
    if args.len() > max_args { return Err(usage.into()); }
    let positive = |default: usize, max: usize| -> Result<usize,String> {
        match args.get(1) {
            None => Ok(default),
            Some(raw) => raw.parse::<usize>().ok().filter(|n| (1..=max).contains(n))
                .ok_or_else(|| format!("{name} 参数必须为 1 至 {max} 的整数。\n{usage}")),
        }
    };
    let command = match name {
        "now" => Command::Now,
        "today" => Command::Today,
        "days" => Command::Days(positive(7,366)?),
        "sessions" => Command::Sessions(positive(10,10_000)?),
        "models" => Command::Models(positive(7,366)?),
        "workspace" => Command::Workspace(args.get(1).map(|s|s.trim().to_string()).unwrap_or_default()),
        "session" => {
            let sid = args.get(1).map(|s|s.trim()).unwrap_or("");
            if !valid_sid(sid) { return Err(format!("session 需要有效的 SID 或唯一前缀。\n{usage}")); }
            Command::Session(sid.into())
        }
        "watch" => {
            let secs = match args.get(1) { Some(v) => v.parse::<f64>().unwrap_or(f64::NAN), None => 5.0 };
            if !secs.is_finite() || !(0.1..=86400.0).contains(&secs) { return Err("watch 刷新间隔必须为 0.1 至 86400 秒。".into()); }
            Command::Watch(Duration::from_secs_f64(secs))
        }
        _ => unreachable!(),
    };
    Ok(Some(command))
}
fn render(reader: &mut DataReader, command: &Command) -> Result<String, String> {
    match command {
        Command::Now => render_current(&reader.snapshot(0,"")),
        Command::Today => {
            let con = reader.connection().map_err(|e|format!("读取数据库失败：{e}"))?;
            let schema = sql::Schema::load(con).map_err(|e|e.to_string())?;
            let (today,_,_) = periods(con,&schema).map_err(|e|e.to_string())?;
            Ok(render_today(&today))
        }
        Command::Days(days) => {
            let report = reader.reports(*days).map_err(|e|format!("读取报表失败：{e}"))?;
            Ok(render_days(&report))
        }
        Command::Sessions(limit) => {
            let con = reader.connection().map_err(|e|format!("读取数据库失败：{e}"))?;
            let schema = sql::Schema::load(con).map_err(|e|e.to_string())?;
            let rows = sql::recent_completed(con,&schema,*limit).map_err(|e|e.to_string())?;
            Ok(render_sessions(&rows,*limit))
        }
        Command::Models(days) => {
            let con = reader.connection().map_err(|e|format!("读取数据库失败：{e}"))?;
            let schema = sql::Schema::load(con).map_err(|e|e.to_string())?;
            let since = now_ms() - chrono::Duration::days(*days as i64).num_milliseconds();
            let rows = sql::models_since(con,&schema,since).map_err(|e|e.to_string())?;
            Ok(render_models(&rows,&format!("近 {days} 天按模型（滚动 {days} × 24 小时）"),true))
        }
        Command::Workspace(pattern) => {
            let con = reader.connection().map_err(|e|format!("读取数据库失败：{e}"))?;
            let report = sql::workspace(con,pattern).map_err(|e|format!("读取工作区失败：{e}"))?;
            Ok(render_workspace(&report,pattern))
        }
        Command::Session(prefix) => render_session(reader,prefix),
        Command::Watch(_) => unreachable!("watch 由流式入口处理"),
    }
}
fn title(value: &str) -> String { if value.trim().is_empty() { "(无标题)".into() } else { plain(value,80) } }
fn stamp(ms: i64) -> String {
    Local.timestamp_millis_opt(ms).single().filter(|_| ms>0)
        .map(|d|d.format("%m-%d %H:%M").to_string()).unwrap_or_else(||"未知".into())
}
fn usage_line(u: &RangeUsage) -> String {
    format!("  {} 次请求  input {}（缓存读 {}）  output {}  合计 {}", u.requests,fmt_tokens(u.input),fmt_tokens(u.cache_read),fmt_tokens(u.output),fmt_tokens(u.total))
}
fn render_today(today: &RangeUsage) -> String {
    format!("今日（{}）\n{}",Local::now().format("%Y-%m-%d"),usage_line(today))
}
fn render_current(s: &Snapshot) -> Result<String,String> {
    if !s.ok { return Err(format!("读取数据库失败：{}",s.error)); }
    let mut rows = Vec::new();
    if !s.sid.is_empty() {
        let state=if s.generating {"生成中"} else if s.active {"活跃"} else {"闲置"};
        rows.push(format!("当前会话 [{state}]  {}  [{}]",title(&s.title),s.sid));
        if !s.directory.is_empty() {rows.push(format!("  目录: {}",plain(&s.directory,1000)));}
        rows.push(format!("  {} 轮 / {} 次请求 / {} 次工具调用",s.turns,s.requests,s.tool_calls));
        rows.push(format!("  input {}（缓存读 {}）  output {}  合计 {}",fmt_tokens(s.input),fmt_tokens(s.cache_read),fmt_tokens(s.output),fmt_tokens(s.total)));
        rows.push(context_line(s));
        if s.last_at>0 {rows.push(format!("  最后活动: {}",stamp(s.last_at)));}
    } else {rows.push("当前会话：暂无数据".into());}
    rows.push(render_today(&s.today));
    Ok(rows.join("\n"))
}
fn context_line(s: &Snapshot) -> String {
    let window=if s.context_window>0 {
        format!(" / {}（{}）",fmt_tokens(s.context_window),if s.context_inferred {"推断窗口"} else if s.context_known {"已知窗口"} else {"设置参考值"})
    } else {"，窗口未知".into()};
    format!("  上下文 ≈ {} tokens{window}（最近成功请求输入）",fmt_tokens(s.ctx))
}
fn render_days(report: &UsageReports) -> String {
    let mut rows=vec![format!("近 {} 天每日用量（本地自然日，合计=input+output）",report.days)];
    for day in &report.daily {
        let u=&day.usage;
        rows.push(format!("  {}  {:>4} 次  in {:>8}  out {:>8}  合计 {:>8}",day.date,u.requests,fmt_tokens(u.input),fmt_tokens(u.output),fmt_tokens(u.total)));
    }
    rows.join("\n")
}
fn render_sessions(sessions: &[SessionInfo], limit: usize) -> String {
    let mut rows=vec![format!("最近 {limit} 个会话（已完成请求）")];
    for s in sessions {
        let name=s.directory.rsplit(['/', '\\']).find(|p|!p.is_empty()).unwrap_or("?");
        rows.push(format!("  {}  {}（in {} / out {}，{} 请求）  [{}] {}  [{}]{}",
            stamp(s.last_at),fmt_tokens(s.total),fmt_tokens(s.input),fmt_tokens(s.output),s.requests,plain(name,100),title(&s.title),s.sid,if s.is_sub {" · 子代理"}else{""}));
    }
    if sessions.is_empty() {rows.push("暂无 completed 请求。".into());}
    rows.join("\n")
}
fn render_models(models: &[ModelUsage], heading: &str, provider: bool) -> String {
    let mut rows=vec![heading.to_string()];
    for m in models {
        rows.push(format!("  {}  {} 次  in {}  out {}  合计 {}{}",plain(&m.model,200),m.usage.requests,fmt_tokens(m.usage.input),fmt_tokens(m.usage.output),fmt_tokens(m.usage.total),
            if provider {format!("（{}）",plain(&m.provider,200))}else{String::new()}));
    }
    if models.is_empty() {rows.push("暂无 completed 请求。".into());}
    rows.join("\n")
}
fn render_session(reader: &mut DataReader, prefix: &str) -> Result<String,String> {
    let sid = {
        let con=reader.connection().map_err(|e|format!("读取数据库失败：{e}"))?;
        let schema=sql::Schema::load(con).map_err(|e|e.to_string())?;
        let candidates=sql::session_candidates(con,&schema,prefix).map_err(|e|e.to_string())?;
        match candidates.len() {
            0 => return Ok(format!("未找到 id 前缀为 {prefix:?} 的会话")),
            1 => candidates[0].clone(),
            _ => return Ok(format!("会话前缀 {prefix:?} 有多个匹配，请指定完整 SID：\n{}{}",
                candidates.iter().take(10).map(|s|format!("  {s}")).collect::<Vec<_>>().join("\n"),if candidates.len()>10 {"\n  …"} else {""})),
        }
    };
    let s=reader.snapshot(0,&sid);
    if !s.ok {return Err(format!("读取会话失败：{}",s.error));}
    if s.pin_lost || s.sid!=sid {return Ok(format!("会话 {sid} 已不存在"));}
    let models={
        let con=reader.connection().map_err(|e|e.to_string())?;
        let schema=sql::Schema::load(con).map_err(|e|e.to_string())?;
        sql::models_for_sessions(con,&schema,std::slice::from_ref(&sid)).map_err(|e|e.to_string())?
    };
    let mut rows=vec![format!("会话 {sid}  {}",title(&s.title))];
    if !s.directory.is_empty() {rows.push(format!("  目录: {}",plain(&s.directory,1000)));}
    rows.push(format!("  {} 轮 / {} 次请求",s.turns,s.requests));
    rows.push(format!("  input {}（缓存读 {}）  output {}  合计 {}（合计=input+output，缓存读已含在 input 内）",fmt_tokens(s.input),fmt_tokens(s.cache_read),fmt_tokens(s.output),fmt_tokens(s.total)));
    rows.push(context_line(&s));
    if s.last_at>0 {rows.push(format!("  最后活动: {}",stamp(s.last_at)));}
    if !models.is_empty() {rows.push(render_models(&models[..models.len().min(6)],"按模型",false));}
    if !s.sub.list.is_empty() {
        rows.push(format!("子代理: {} 个 / {}req / 合计 {}",s.sub.list.len(),s.sub.requests,fmt_tokens(s.sub.total)));
        for child in s.sub.list.iter().take(8) {
            let name=if child.task.is_empty(){title(&child.title)}else{plain(&child.task,80)};
            rows.push(format!("  {}  {}req  {}  [{}]",fmt_tokens(child.total),child.requests,name,child.sid));
        }
        if s.sub.list.len()>8 {rows.push(format!("  （另有 {} 个）",s.sub.list.len()-8));}
    }
    Ok(rows.join("\n"))
}
fn render_workspace(report: &sql::WorkspaceReport, pattern: &str) -> String {
    if report.directories.is_empty() {
        let mut rows=vec![format!("未找到目录匹配 {pattern:?} 的工作区。现有工作区（按会话数前 10）：")];
        for (directory,count) in report.existing.iter().take(10) {rows.push(format!("  {count:>4}  {}",plain(directory,1000)));}
        if report.existing.is_empty() {rows.push("  暂无工作区。".into());}
        return rows.join("\n");
    }
    let mut rows=vec![if report.directories.len()==1 {format!("工作区 {}",plain(&report.directories[0].0,1000))}
        else {format!("工作区（匹配 {pattern:?}，命中 {} 个）",report.directories.len())}];
    if report.directories.len()>1 {for (directory,count) in report.directories.iter().take(5) {rows.push(format!("  {count:>4}  {}",plain(directory,1000)));}}
    let mut main=RangeUsage::default();let mut children=RangeUsage::default();
    for row in &report.mains {main.add(&row.usage);}
    for row in &report.children {children.add(&row.usage);}
    let mut all=main.clone();all.add(&children);
    rows.push(format!("主会话 {} 个（其中无请求 {} 个） + 子代理 {} 个",report.mains.len(),report.mains.iter().filter(|m|m.usage.requests==0).count(),report.children.len()));
    if all.requests==0 {rows.push("该工作区暂无 completed 请求。".into());return rows.join("\n");}
    let first=report.mains.iter().chain(&report.children).map(|s|s.first_at).filter(|t|*t>0).min().unwrap_or(0);
    let last=report.mains.iter().chain(&report.children).map(|s|s.last_at).max().unwrap_or(0);
    rows.push(format!("{} ~ {}",stamp(first),stamp(last)));
    rows.push(format!("请求 {}（主 {} / 子 {}）  input {}（缓存读 {}）  output {}（含 reasoning {}）",all.requests,main.requests,children.requests,all.input,all.cache_read,all.output,all.reasoning));
    rows.push(format!("合计（=input+output，缓存读已含在 input 内） {}",all.total));
    let mut child_by=HashMap::<&str,(usize,RangeUsage)>::new();
    for child in &report.children {let entry=child_by.entry(&child.parent).or_default();entry.0+=1;entry.1.add(&child.usage);}
    let mut ranked=report.mains.iter().filter_map(|m|{
        let (count,sub)=child_by.get(m.sid.as_str()).cloned().unwrap_or_default();
        (m.usage.requests>0||sub.requests>0).then_some((m,count,sub))
    }).collect::<Vec<_>>();
    ranked.sort_by(|a,b|(b.0.usage.total+b.2.total).cmp(&(a.0.usage.total+a.2.total)).then_with(||a.0.sid.cmp(&b.0.sid)));
    rows.push("按会话（含其子代理，按含子合计降序，前 15）".into());
    for (m,count,sub) in ranked.iter().take(15) {
        rows.push(format!("  {}  {}req  in {}  out {}  合计 {}{}  {}  [{}]",stamp(m.last_at),m.usage.requests,m.usage.input,m.usage.output,m.usage.total,
            if *count>0 {format!(" +子{count}个 {}，含子 {}",sub.total,m.usage.total+sub.total)} else {String::new()},title(&m.title),m.sid));
    }
    if ranked.len()>15 {rows.push(format!("  （另有 {} 个会话，含子合计 {}）",ranked.len()-15,ranked.iter().skip(15).map(|r|r.0.usage.total+r.2.total).sum::<i64>()));}
    rows.push(render_models(&report.models,"按模型（主会话 + 子代理）",false));
    rows.join("\n")
}
fn write_frame(output: &mut impl Write, text: &str) -> io::Result<()> {
    output.write_all(text.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}
#[cfg(windows)]
mod watch_interrupt {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    #[link(name="kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    unsafe extern "system" fn handle(event: u32) -> i32 {
        if event == 0 || event == 1 { // CTRL_C_EVENT / CTRL_BREAK_EVENT
            INTERRUPTED.store(true, Ordering::Relaxed);
            1
        } else { 0 }
    }
    pub struct Guard { installed: bool }
    impl Guard {
        pub fn new() -> std::io::Result<Self> {
            INTERRUPTED.store(false, Ordering::Relaxed);
            // Git Bash/父进程可能继承「忽略 Ctrl+C」属性。仅在已识别的 watch 命令中清除。
            unsafe {
                if SetConsoleCtrlHandler(None, 0) == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(6) { return Ok(Self { installed: false }); }
                    return Err(error);
                }
                if SetConsoleCtrlHandler(Some(handle), 1) == 0 { return Err(std::io::Error::last_os_error()); }
            }
            Ok(Self { installed: true })
        }
        pub fn interrupted(&self) -> bool { INTERRUPTED.load(Ordering::Relaxed) }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.installed { unsafe { SetConsoleCtrlHandler(Some(handle), 0); } }
        }
    }
}
#[cfg(not(windows))]
mod watch_interrupt {
    // Unix CLI 保留默认 SIGINT 行为；无新增 libc/tokio 依赖。
    pub struct Guard;
    impl Guard {
        pub fn new() -> std::io::Result<Self> { Ok(Self) }
        pub fn interrupted(&self) -> bool { false }
    }
}
fn watch(reader: &mut DataReader, interval: Duration, output: &mut impl Write, terminal: bool) -> io::Result<()> {
    let interrupt = watch_interrupt::Guard::new()?;
    while !interrupt.interrupted() {
        let snapshot=reader.snapshot(0,"");
        let mut text=render_current(&snapshot).unwrap_or_else(|error|error);
        text.push_str(&format!("\n（每 {}s 刷新，Ctrl+C 退出）",interval.as_secs_f64()));
        if terminal {text.insert_str(0,"\x1b[2J\x1b[H");}
        match write_frame(output,&text) {
            Err(error) if error.kind()==io::ErrorKind::BrokenPipe => return Ok(()),
            result => result?,
        }
        let waited = Instant::now();
        while waited.elapsed() < interval {
            if interrupt.interrupted() { return Ok(()); }
            std::thread::sleep(interval.saturating_sub(waited.elapsed()).min(Duration::from_millis(100)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<String> { items.iter().map(|v|v.to_string()).collect() }
    #[test] fn parser_keeps_gui_and_remote_routes_and_rejects_bad_intervals() {
        assert_eq!(parse(&[]).unwrap(),None);
        for name in ["--discover","json","serve","--once"] {assert_eq!(parse(&args(&[name])).unwrap(),None);}
        assert_eq!(parse(&args(&["models"])).unwrap(),Some(Command::Models(7)));
        assert_eq!(parse(&args(&["watch"])).unwrap(),Some(Command::Watch(Duration::from_secs(5))));
        for raw in ["0","-1","NaN","inf","0.001"] {assert!(parse(&args(&["watch",raw])).is_err());}
        assert!(parse(&args(&["session"])).is_err());assert!(parse(&args(&["session","bad;sql"])).is_err());
        assert!(parse(&args(&["days","0"])).is_err());assert!(parse(&args(&["sessions","2","extra"])).is_err());
    }
    #[test] fn workspace_includes_children_once_and_only_for_matched_roots() {
        let con=Connection::open_in_memory().unwrap();
        con.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT,directory TEXT,parent_id TEXT);
            INSERT INTO session VALUES('root','Root','D:/Apple Banana Cherry',NULL),('empty','Empty','D:/Apple Banana Cherry',''),
            ('child','Child','D:/Apple Banana Cherry','root'),('moved','Moved','D:/elsewhere','root'),
            ('other','Other','D:/elsewhere',NULL),('foreign','Foreign','D:/Apple Banana Cherry','other');
            CREATE TABLE model_usage(session_id TEXT,status TEXT,model_id TEXT,input_tokens INTEGER,output_tokens INTEGER,computed_total_tokens INTEGER,cache_read_input_tokens INTEGER,completed_at INTEGER);
            INSERT INTO model_usage VALUES('root','completed','main-model',100,10,110,80,1000),
            ('child','completed','worker-model',200,20,220,100,2000),('moved','completed','worker-model',300,30,330,200,3000),
            ('foreign','completed','foreign-model',900,90,990,0,4000),('root','error','failed-model',999,0,999,0,5000);").unwrap();
        let report=sql::workspace(&con,"abc").unwrap();
        assert_eq!(report.mains.len(),2);assert_eq!(report.children.len(),2);
        assert_eq!(report.models.iter().map(|m|m.usage.total).sum::<i64>(),660);
        let text=render_workspace(&report,"abc");assert!(text.contains("请求 3（主 1 / 子 2）"));assert!(text.contains("缓存读 380"));assert!(text.contains("input 内） 660"));
        assert!(!text.contains("foreign-model"));assert!(!text.contains("failed-model"));
        assert!(con.is_autocommit());
    }
    #[test] fn session_prefix_is_literal_and_full_id_wins() {
        let con=Connection::open_in_memory().unwrap();
        con.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT); INSERT INTO session VALUES('a_b','One'),('a_b_more','Two'),('axb','Three');").unwrap();
        let schema=sql::Schema::load(&con).unwrap();
        assert_eq!(sql::session_candidates(&con,&schema,"a_b").unwrap(),vec!["a_b"]);
        assert_eq!(sql::session_candidates(&con,&schema,"a_").unwrap(),vec!["a_b","a_b_more"]);
        assert!(sql::session_candidates(&con,&schema,"missing").unwrap().is_empty());
    }
    #[test] fn recent_sessions_and_models_ignore_errors_and_follow_requested_period() {
        let con=Connection::open_in_memory().unwrap();
        con.execute_batch("CREATE TABLE model_usage(session_id TEXT,status TEXT,completed_at INTEGER,provider_id TEXT,model_id TEXT,input_tokens INTEGER,output_tokens INTEGER,computed_total_tokens INTEGER);
            INSERT INTO model_usage VALUES('old','completed',100,'p','m',10,1,11),('recent','completed',300,'p','m',20,2,22),('bad','error',500,'p','failed',999,0,999);").unwrap();
        let schema=sql::Schema::load(&con).unwrap();let rows=sql::recent_completed(&con,&schema,1).unwrap();
        assert_eq!(rows[0].sid,"recent");assert_eq!(rows[0].total,22);
        let models=sql::models_since(&con,&schema,200).unwrap();assert_eq!(models.len(),1);assert_eq!(models[0].usage.total,22);
    }
    #[test] fn watch_returns_normally_when_output_pipe_is_closed() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self,_:&[u8])->io::Result<usize>{Err(io::ErrorKind::BrokenPipe.into())}
            fn flush(&mut self)->io::Result<()>{Ok(())}
        }
        let path=std::env::temp_dir().join(format!("zcode-watch-absent-{}-{}",std::process::id(),now_ms()));
        let mut reader=DataReader::with_path(&path);
        assert!(watch(&mut reader,Duration::from_millis(100),&mut Closed,false).is_ok());assert!(!path.exists());
    }
    #[test] fn empty_schema_reports_are_readable() {
        let con=Connection::open_in_memory().unwrap();let report=sql::workspace(&con,"").unwrap();
        assert!(render_workspace(&report,"").contains("暂无工作区"));
        let schema=sql::Schema::load(&con).unwrap();assert!(sql::recent_completed(&con,&schema,10).unwrap().is_empty());
        assert!(sql::models_since(&con,&schema,0).unwrap().is_empty());
        assert!(render_current(&Snapshot{ok:true,..Default::default()}).unwrap().contains("暂无数据"));
    }
}
