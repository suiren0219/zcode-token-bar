//! SSH 远端协议仍为 known_sids/recent/today；远端执行 Rust helper，零 Python 依赖。
//! 本模块可由独立 Linux bin 通过 #[path="../data.rs"] mod data 复用。
use super::*;
use serde_json::{json, Value};
use std::io::{BufRead, Write as _};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub ssh_argv: Vec<String>, pub executable: String, pub timeout_s: f64, pub host_label: String,
}
impl RemoteConfig {
    /// 支持 UI 的 host/user/port/key/helper，也兼容旧 ssh 字符串或 argv 数组。
    /// key 仅是本地私钥文件路径；不读取、不落盘、不在错误消息中回显密钥正文。
    pub fn from_value(value: &Value) -> Result<Option<Self>, String> {
        let v = value.get("remote").unwrap_or(value);
        if v.get("enabled").and_then(Value::as_bool) != Some(true) { return Ok(None); }
        let argv = match v.get("ssh") {
            Some(Value::Array(a)) if !a.is_empty() => a.iter().map(|x| x.as_str().map(str::to_string)
                .ok_or_else(|| "remote.ssh 数组只能包含字符串".to_string())).collect::<Result<Vec<_>, _>>()?,
            Some(Value::String(s)) if !s.trim().is_empty() => split_argv(s)?,
            None | Some(Value::Null | Value::String(_) | Value::Array(_)) => host_argv(v)?,
            _ => return Err("remote.ssh 必须是命令字符串或 argv 数组".into()),
        };
        if argv.first().is_none_or(|v| v.trim().is_empty())
            || argv.iter().any(|v| v.contains(['\0', '\r', '\n'])) {
            return Err("无效的 SSH 参数".into());
        }
        let executable = v["executable"].as_str().filter(|s| !s.trim().is_empty())
            .or_else(|| v["helper"].as_str().filter(|s| !s.trim().is_empty()))
            .unwrap_or(".zcode/zcode-token-usage-statusbar/zcode-token-data").trim().to_string();
        if executable.contains(['\0', '\r', '\n']) { return Err("远端 Rust helper 路径无效".into()); }
        let timeout = v["timeout_s"].as_f64().unwrap_or(6.0);
        let host_label = v["host_label"].as_str().filter(|s| !s.trim().is_empty())
            .or_else(|| v["host"].as_str().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| argv.last().map(String::as_str).unwrap_or("remote")).trim().to_string();
        Ok(Some(Self { ssh_argv: argv, executable, host_label,
            timeout_s: if timeout.is_finite() { timeout.clamp(1.0, 14.0) } else { 6.0 } }))
    }
}
fn host_argv(v: &Value) -> Result<Vec<String>, String> {
    let host = v["host"].as_str().unwrap_or("").trim();
    if host.is_empty() { return Err("远端已启用，但未配置 SSH 主机".into()); }
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || host.contains(['/', '\\']) {
        return Err("SSH 主机应为主机名、IP 或 SSH 配置别名".into());
    }
    let user = v["user"].as_str().unwrap_or("").trim();
    if user.starts_with('-') || user.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("SSH 用户名无效".into());
    }
    let port = match v.get("port") {
        None | Some(Value::Null) => 22,
        Some(Value::String(s)) if s.trim().is_empty() => 22,
        Some(Value::String(s)) => s.trim().parse::<u16>().ok().filter(|p| *p > 0)
            .ok_or_else(|| "SSH 端口应为 1 至 65535 的整数".to_string())?,
        Some(n) => n.as_u64().filter(|p| (1..=65535).contains(p)).map(|p| p as u16)
            .ok_or_else(|| "SSH 端口应为 1 至 65535 的整数".to_string())?,
    };
    let key = match v.get("key") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.trim(),
        _ => return Err("SSH key 应为本地私钥文件路径".into()),
    };
    if key.contains(['\0', '\r', '\n']) || key.contains("-----BEGIN")
        || key.contains("PRIVATE KEY") || key.starts_with("PuTTY-User-Key-File-") {
        return Err("SSH key 只接受私钥文件路径，不能填写私钥正文".into());
    }
    let mut args = vec!["ssh".into(), "-oBatchMode=yes".into(), "-p".into(), port.to_string()];
    if !user.is_empty() { args.extend(["-l".into(), user.into()]); }
    if !key.is_empty() {
        let path = if let Some(rest) = key.strip_prefix("~/").or_else(|| key.strip_prefix("~\\")) {
            context::home().join(rest).to_string_lossy().into_owned()
        } else { key.to_string() };
        args.extend(["-i".into(), path]);
    }
    args.extend(["--".into(), host.into()]);
    Ok(args)
}
fn split_argv(s: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new(); let mut current = String::new(); let mut quote = None; let mut started = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        // 保留 Windows 路径中的反斜线，只把引号/空白前的反斜线作为转义。
        if c == '\\' && quote != Some('\'') {
            let escape = chars.peek().is_some_and(|next| *next == '"' || *next == '\''
                || (quote.is_none() && next.is_whitespace()));
            if escape { current.push(chars.next().unwrap()); } else { current.push(c); }
            started = true; continue;
        }
        if let Some(q) = quote { if c == q { quote = None; } else { current.push(c); } started = true; }
        else if c == '\'' || c == '"' { quote = Some(c); started = true; }
        else if c.is_whitespace() { if started { result.push(std::mem::take(&mut current)); started = false; } }
        else { current.push(c); started = true; }
    }
    if quote.is_some() { return Err("remote.ssh 引号不完整，建议使用 argv 数组".into()); }
    if started { result.push(current); } Ok(result)
}
fn shell_quote(s:&str)->String{format!("'{}'",s.replace('\'',"'\\''"))}
fn helper_command(executable:&str,sids:&[String])->String{
    // ~/.path 中只展开固定 $HOME；其他所有内容均为单引号字面值。
    let exe=if let Some(tail)=executable.strip_prefix("~/"){format!("\"$HOME\"/{}",shell_quote(tail))}else{shell_quote(executable)};
    format!("{exe} --data-json {}",shell_quote(&sids.join(",")))
}

#[derive(Clone)]
struct Reply { by:HashMap<String,Snapshot>, today:RangeUsage, selected:Option<Snapshot> }
fn remove_null_fields(value:&mut Value){
    match value {
        Value::Object(map)=>{map.retain(|_,v|!v.is_null());for v in map.values_mut(){remove_null_fields(v);}}
        Value::Array(items)=>{for v in items{remove_null_fields(v);}}
        _=>{}
    }
}
fn parse_payload(stdout: &str, sids: &[String], config: &RemoteConfig) -> Result<Reply, String> {
    let payload = stdout.lines().rev().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|v| v.is_object() && (v.get("recent").is_some() || v.get("error").is_some()))
        .ok_or_else(|| "远端没有返回有效 JSON 快照".to_string())?;
    if let Some(error) = payload["error"].as_str().filter(|e| !e.is_empty()) {
        return Err(format!("远端查询失败：{}", plain(error, 180)));
    }
    let known = match payload.get("known_sids") {
        None | Some(Value::Null) => None,
        Some(Value::Array(ids)) if ids.iter().all(|v| v.as_str().is_some_and(valid_sid)) =>
            Some(ids.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        _ => return Err("远端 known_sids 字段无效".into()),
    };
    let records = payload["recent"].as_array().ok_or_else(|| "远端 JSON 缺少 recent 会话列表".to_string())?;
    let mut by = HashMap::new();
    for record in records {
        let Some(sid) = record["sid"].as_str().filter(|id| sids.iter().any(|s| s == id)) else { continue; };
        let found = known.as_ref().map(|k| k.contains(&sid)).unwrap_or_else(||
            record["requests"].as_i64().unwrap_or(0) > 0 || record["last_at"].as_i64().unwrap_or(0) > 0);
        if found { by.insert(sid.to_string(), remote_snapshot(record, &payload, config)?); }
    }
    if let Some(known) = &known {
        if sids.iter().any(|sid| known.contains(&sid.as_str()) && !by.contains_key(sid)) {
            return Err("远端声明会话存在，但缺少对应完整快照".into());
        }
    }
    let selected = if sids.is_empty() {
        payload["session"]["sid"].as_str().filter(|s| valid_sid(s))
            .map(|_| remote_snapshot(&payload["session"], &payload, config)).transpose()?
    } else { None };
    let mut today = payload["today"].clone();
    if today.is_null() { today = json!({}); }
    remove_null_fields(&mut today);
    let today = serde_json::from_value(today).map_err(|_| "远端 today 字段类型无效".to_string())?;
    Ok(Reply { by, today, selected })
}
fn remote_snapshot(record: &Value, payload: &Value, config: &RemoteConfig) -> Result<Snapshot, String> {
    if record["ok"].as_bool() == Some(false) { return Err("远端会话读取失败".into()); }
    let mut normalized = record.clone();
    remove_null_fields(&mut normalized);
    let mut s: Snapshot = serde_json::from_value(normalized)
        .map_err(|_| "远端会话字段类型与数据协议不符".to_string())?;
    s.ok = true; s.remote = true; s.remote_host = config.host_label.clone(); s.remote_enabled = true;
    if record.get("tools").is_some_and(Value::is_object) { s.tools_total = s.tools.total; s.tools_errors = s.tools.errors; }
    if s.sid == payload["session"]["sid"].as_str().unwrap_or("") {
        if let Some(state) = payload.get("companion") {
            if let Ok(state) = serde_json::from_value::<TurnState>(state.clone()) { s.turn_state = state; }
        }
    }
    s.is_sub |= s.sid.starts_with("sess_subagent") || s.parent.as_ref().is_some_and(|p| !p.is_empty());
    s.turn_state.is_sub |= s.is_sub;
    if s.is_sub || !valid_remote_turn(&s.turn_state, &s.sid) { s.turn_state.available = false; }
    if !record.get("context_known").is_some_and(Value::is_boolean) {
        s.context_known = s.context_auto && !s.context_inferred && s.context_window > 0;
        if s.context_source.is_empty() { s.context_source = "远端快照提供的窗口声明".into(); }
    }
    if !record.get("context_exceeded").is_some_and(Value::is_boolean) {
        // 旧版没有最近成功请求时间，只能使用原协议的 last_at。
        s.context_exceeded = s.ctx_exc > 0 && s.ctx_exc > if s.last.completed_at > 0 { s.last.completed_at } else { s.last_at };
    }
    Ok(s)
}
fn valid_remote_turn(state: &TurnState, sid: &str) -> bool {
    state.session_id == sid && !state.turn_id.is_empty() && state.started_at > 0
        && state.duration_ms >= 0 && state.tool_errors >= 0
        && if state.status == "running" { state.completed_at == 0 && !state.cancelled_by_user }
            else { matches!(state.status.as_str(), "completed" | "error" | "cancelled") && state.completed_at >= state.started_at }
}
fn capture(mut stream:impl std::io::Read)->std::io::Result<Vec<u8>>{
    // 持续排空管道，不因远端横幅填满而死锁；超长输出不无限占内存。
    let mut out=Vec::new();let mut buf=[0u8;8192];
    loop{let n=stream.read(&mut buf)?;if n==0{break;}let remaining=(4*1024*1024usize).saturating_sub(out.len());out.extend_from_slice(&buf[..n.min(remaining)]);}
    Ok(out)
}
fn execute(sids:&[String],config:&RemoteConfig)->Result<Reply,String>{
    let Some(program)=config.ssh_argv.first() else{return Err("未配置 SSH 命令".into());};
    let mut command=Command::new(program);
    command.args(&config.ssh_argv[1..]).arg(helper_command(&config.executable,sids)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]{use std::os::windows::process::CommandExt;command.creation_flags(0x08000000);}
    let mut child=command.spawn().map_err(|e|format!("无法启动 SSH：{e}"))?;
    let stdout=child.stdout.take().ok_or_else(||"无法读取 SSH 输出".to_string())?;
    let stderr=child.stderr.take().ok_or_else(||"无法读取 SSH 错误输出".to_string())?;
    let (out_send, out_recv) = std::sync::mpsc::sync_channel(1);
    let (err_send, err_recv) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || { let _ = out_send.send(capture(stdout)); });
    std::thread::spawn(move || { let _ = err_send.send(capture(stderr)); });
    let started = Instant::now();
    let timeout = Duration::from_secs_f64(if config.timeout_s.is_finite() { config.timeout_s.clamp(1.0, 14.0) } else { 6.0 });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => { let _ = child.kill(); let _ = child.wait(); break Err(format!("SSH 超时（{:.0}秒）", timeout.as_secs_f64())); }
            Err(e) => { let _ = child.kill(); let _ = child.wait(); break Err(format!("SSH 等待失败：{e}")); }
        }
    }?;
    // SSH 已退出但后代仍持有管道时也必须按同一截止时间返回。
    let out = out_recv.recv_timeout(timeout.saturating_sub(started.elapsed()))
        .map_err(|_| "读取 SSH 输出超时".to_string())?.map_err(|e| format!("读取 SSH 输出失败：{e}"))?;
    let err = err_recv.recv_timeout(timeout.saturating_sub(started.elapsed()))
        .map_err(|_| "读取 SSH 错误输出超时".to_string())?.unwrap_or_default();
    if !status.success(){return Err(format!("SSH 退出码 {}：{}",status.code().map(|v|v.to_string()).unwrap_or_else(||"未知".into()),plain(&String::from_utf8_lossy(&err),160)));}
    parse_payload(&String::from_utf8_lossy(&out),sids,config)
}

#[derive(Clone)]
struct Entry { at:Instant, value:Result<Reply,String>, ttl:Duration }
#[derive(Default)]
pub(super) struct RemoteReader { cache:HashMap<String,Entry> }
impl RemoteReader {
    pub fn merge(&mut self, mut local: Snapshot, sid: &str, config: &RemoteConfig) -> Snapshot {
        local.remote_enabled = true;
        let sid = sid.trim();
        let following = sid.is_empty();
        if (!following && !valid_sid(sid)) || (local.ok && if following { !local.sid.is_empty() } else { !local.pin_lost }) {
            finish(&mut local); return local;
        }
        let key = format!("{}\n{sid}", serde_json::to_string(config).unwrap_or_default());
        let value = if let Some(e) = self.cache.get(&key).filter(|e| e.at.elapsed() < e.ttl) { e.value.clone() } else {
            let wanted = if following { Vec::new() } else { vec![sid.to_string()] };
            let value = execute(&wanted, config);
            let ttl = match &value {
                Ok(_) if following => Duration::from_secs(2),
                Ok(r) if r.by.contains_key(sid) => Duration::from_secs(2),
                Ok(_) => Duration::from_secs(120), Err(_) => Duration::from_secs(60),
            };
            if self.cache.len() >= 256 { self.cache.clear(); }
            self.cache.insert(key, Entry { at: Instant::now(), value: value.clone(), ttl }); value
        };
        match value {
            Ok(mut reply) => {
                let selected = if following { reply.selected.take() } else { reply.by.remove(sid) };
                if let Some(mut selected) = selected {
                    selected.today = reply.today; selected.pinned = sid.into(); selected.pin_lost = false;
                    selected.known_sids = vec![selected.sid.clone()]; selected.not_local = vec![selected.sid.clone()];
                    finish(&mut selected); selected
                } else {
                    // 本地读失败时尚不能确认本地不存在，因此不解除钉住。
                    local.pin_lost = !following && local.ok; finish(&mut local); local
                }
            }
            Err(error) => {
                local.remote_error = error; local.pinned = sid.into(); local.pin_lost = false;
                finish(&mut local); local
            }
        }
    }
}

fn parse_request(line:&str)->(String,String){
    if let Ok(v)=serde_json::from_str::<Value>(line){
        (v["force"].as_str().unwrap_or("").to_string(),v["pin"].as_str().unwrap_or("").to_string())
    }else{(line.trim().to_string(),String::new())}
}
fn payload(reader:&mut DataReader,force:&str,pin:&str)->Value{
    let mut sids=force.split(',').map(str::trim).filter(|s|valid_sid(s)).take(64).map(str::to_string).collect::<Vec<_>>();
    if valid_sid(pin){sids.push(pin.into());}
    let mut seen=std::collections::HashSet::new();sids.retain(|s|seen.insert(s.clone()));
    let base=reader.snapshot(0,pin);
    if !base.ok{return json!({"error":base.error});}
    let mut known=Vec::new();let mut recent=Vec::new();
    for sid in sids{
        let row=if base.sid==sid&&!base.pin_lost{base.clone()}else{reader.snapshot(0,&sid)};
        if row.ok&&!row.pin_lost&&row.sid==sid{known.push(sid);recent.push(row);}
    }
    if !base.sid.is_empty()&&!recent.iter().any(|v|v.sid==base.sid){recent.push(base.clone());}
    json!({"protocol":"zcode-token-data/1","session":base,"companion":base.turn_state,"last_turn":base.last_turn,"last":base.last,
        "today":base.today,"known_sids":known,"recent":recent,"pinned":pin,"pin_lost":base.pin_lost,
        "context_window":base.context_window,"context_auto":base.context_auto,"context_inferred":base.context_inferred,
        "code":base.code,"ctx_exc":base.ctx_exc,"tools":base.tools})
}

/// args 不含 argv[0]，必须在窗口/托盘/单实例初始化前调用。
/// Linux helper 也只需调用此函数，依赖仅 rusqlite/chrono/serde/serde_json/regex。
pub fn handle_cli(args:&[String])->bool{
    let Some(cmd)=args.first().map(String::as_str)else{return false;};
    if !matches!(cmd,"json"|"serve"|"--data-json"|"--data-serve"){return false;}
    let mut reader=DataReader::new();
    if matches!(cmd,"json"|"--data-json"){
        let force=args.get(1).map(String::as_str).unwrap_or("");
        let pin=args.windows(2).find(|a|a[0]=="--pin").map(|a|a[1].as_str()).unwrap_or("");
        println!("{}",payload(&mut reader,force,pin));
    }else{
        let input=std::io::stdin();let mut output=std::io::stdout().lock();
        for line in input.lock().lines(){
            let Ok(line)=line else{break;};let (force,pin)=parse_request(&line);
            let out=payload(&mut reader,&force,&pin);
            if writeln!(output,"{out}").and_then(|_|output.flush()).is_err(){break;}
        }
    }
    true
}

#[cfg(test)]
mod tests{
    use super::*;
    fn config()->RemoteConfig{RemoteConfig{ssh_argv:vec!["ssh".into(),"host".into()],executable:"~/.zcode/token-data".into(),timeout_s:6.0,host_label:"server".into()}}
    #[test]fn known_sids_accept_zero_usage_and_reject_placeholders(){
        let body=json!({"known_sids":["new"],"recent":[{"sid":"new","requests":0},{"sid":"missing","requests":0}],"today":{"total":42}}).to_string();
        let reply=parse_payload(&format!("login banner\n{body}\n"),&["new".into(),"missing".into()],&config()).unwrap();
        assert!(reply.by.contains_key("new"));assert!(!reply.by.contains_key("missing"));assert_eq!(reply.today.total,42);
    }
    #[test]fn helper_argv_is_quoted_and_never_uses_python(){
        let c=helper_command("~/a b/it'works",&["session-1".into()]);
        assert_eq!(c,"\"$HOME\"/'a b/it'\\''works' --data-json 'session-1'");
        assert!(!c.contains("python"));
        assert_eq!(split_argv("ssh -p 22 'host alias'").unwrap(),vec!["ssh","-p","22","host alias"]);
    }
    #[test]fn ui_fields_build_ssh_argv_without_reading_key_contents(){
        let c=RemoteConfig::from_value(&json!({"enabled":true,"host":"server-alias","user":"dev","port":2222,"key":r"C:\keys with spaces\id_ed25519","helper":"~/bin/token data"})).unwrap().unwrap();
        assert_eq!(c.ssh_argv,vec!["ssh","-oBatchMode=yes","-p","2222","-l","dev","-i",r"C:\keys with spaces\id_ed25519","--","server-alias"]);
        assert_eq!(c.executable,"~/bin/token data");assert_eq!(c.host_label,"server-alias");
        let c=RemoteConfig::from_value(&json!({"remote":{"enabled":true,"host":"example.org","port":"2200","key":"~/.ssh/id_ed25519"}})).unwrap().unwrap();
        assert!(c.ssh_argv.contains(&context::home().join(".ssh/id_ed25519").to_string_lossy().into_owned()));
    }
    #[test]fn private_key_body_and_invalid_host_port_are_rejected(){
        for key in ["-----BEGIN OPENSSH PRIVATE KEY----- fake", "line1\nline2", "PuTTY-User-Key-File-3: ssh-ed25519"] {
            let err=RemoteConfig::from_value(&json!({"enabled":true,"host":"server","key":key})).unwrap_err();
            assert!(err.contains("路径"));assert!(!err.contains(key));
        }
        for host in ["-oProxyCommand=bad", "server host", "ssh://server"] {assert!(RemoteConfig::from_value(&json!({"enabled":true,"host":host})).is_err());}
        for port in [json!(0),json!(-1),json!(65536),json!(1.5),json!("bad")] {assert!(RemoteConfig::from_value(&json!({"enabled":true,"host":"server","port":port})).is_err());}
        assert!(RemoteConfig::from_value(&json!({"enabled":false})).unwrap().is_none());
    }
    #[test]fn legacy_ssh_keeps_windows_paths_and_config_limits(){
        let c=RemoteConfig::from_value(&json!({"enabled":true,"ssh":r#""C:\Program Files\OpenSSH\ssh.exe" -i "C:\keys\id_ed25519" server"#,"timeout_s":99})).unwrap().unwrap();
        assert_eq!(c.ssh_argv[0],r"C:\Program Files\OpenSSH\ssh.exe");assert_eq!(c.ssh_argv[2],r"C:\keys\id_ed25519");assert_eq!(c.timeout_s,14.0);
        assert!(RemoteConfig::from_value(&json!({"enabled":true,"ssh":["ssh","server"],"executable":"bad\npath"})).is_err());
    }
    #[test]fn legacy_nulls_are_tolerated_and_unknown_schema_does_not_unpin(){
        let body=json!({"recent":[{"sid":"s","requests":1,"title":null,"last":{"duration_ms":null},"code":{"add":null}}],"today":{"total":null,"input":40}}).to_string();
        let reply=parse_payload(&body,&["s".into()],&config()).unwrap();assert!(reply.by.contains_key("s"));assert_eq!(reply.today.input,40);
        let missing=json!({"known_sids":["s"],"recent":[],"today":{}}).to_string();
        assert!(parse_payload(&missing,&["s".into()],&config()).is_err());
        let invalid=json!({"known_sids":"wrong","recent":[],"today":{}}).to_string();
        assert!(parse_payload(&invalid,&["s".into()],&config()).is_err());
    }
    #[test]fn local_new_session_is_not_queried_remotely(){
        let s=RemoteReader::default().merge(Snapshot{ok:true,sid:"new".into(),pinned:"new".into(),..Default::default()},"new",&config());
        assert_eq!(s.sid,"new");assert!(!s.remote);assert!(s.remote_error.is_empty());
    }
    #[test]fn confirmed_missing_unpins_only_if_local_was_readable(){
        let c=config();let key=format!("{}\nmissing",serde_json::to_string(&c).unwrap());
        let mut reader=RemoteReader::default();reader.cache.insert(key,Entry{at:Instant::now(),value:Ok(Reply{by:HashMap::new(),today:RangeUsage::default(),selected:None}),ttl:Duration::from_secs(120)});
        assert!(reader.merge(Snapshot{ok:true,pin_lost:true,..Default::default()},"missing",&c).pin_lost);
        assert!(!reader.merge(Snapshot::default(),"missing",&c).pin_lost);
    }
    #[test]fn empty_local_follow_uses_remote_current_session(){
        let c=config();let key=format!("{}\n",serde_json::to_string(&c).unwrap());
        let body=json!({"known_sids":[],"recent":[],"session":{"sid":"remote-current","requests":3},"today":{"total":500}}).to_string();
        let reply=parse_payload(&body,&[],&c).unwrap();let mut reader=RemoteReader::default();
        reader.cache.insert(key,Entry{at:Instant::now(),value:Ok(reply),ttl:Duration::from_secs(2)});
        let s=reader.merge(Snapshot{ok:true,..Default::default()},"",&c);assert!(s.remote);assert_eq!(s.sid,"remote-current");assert!(s.pinned.is_empty());assert_eq!(s.today.total,500);
        let s=reader.merge(Snapshot{ok:true,sid:"local".into(),..Default::default()},"",&c);assert_eq!(s.sid,"local");assert!(!s.remote);
    }
    #[test]fn wrong_session_or_child_turn_is_not_a_completion_signal(){
        for (sid,parent,state_sid) in [("s","","other"),("child","root","child")] {
            let body=json!({"known_sids":[sid],"recent":[{"sid":sid,"parent":parent,"turn_state":{"available":true,"session_id":state_sid,"turn_id":"t","status":"completed","started_at":1,"completed_at":10}}],"today":{}}).to_string();
            let reply=parse_payload(&body,&[sid.into()],&config()).unwrap();assert!(!reply.by[sid].turn_state.available);
        }
    }
    #[test]fn helper_known_sids_keeps_fallback_current_when_pin_is_lost(){
        let dir=std::env::temp_dir().join(format!("zcode-data-protocol-{}-{}",std::process::id(),now_ms()));
        std::fs::create_dir_all(&dir).unwrap();let path=dir.join("fixture.sqlite");
        {let con=Connection::open(&path).unwrap();con.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT);INSERT INTO session VALUES('current','Current');").unwrap();}
        let mut reader=DataReader::with_path(&path);let p=payload(&mut reader,"current","missing");
        assert_eq!(p["known_sids"],json!(["current"]));assert_eq!(p["session"]["sid"],"current");assert_eq!(p["pin_lost"],true);
        assert_eq!(p["recent"][0]["sid"],"current");assert_eq!(p["recent"][0]["pin_lost"],false);
        drop(reader);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]fn remote_error_does_not_unpin(){
        let c=config();let key=format!("{}\nmissing",serde_json::to_string(&c).unwrap());
        let mut reader=RemoteReader::default();reader.cache.insert(key,Entry{at:Instant::now(),value:Err("timeout".into()),ttl:Duration::from_secs(60)});
        let s=reader.merge(Snapshot{ok:true,pin_lost:true,..Default::default()},"missing",&c);
        assert!(!s.pin_lost);assert_eq!(s.pinned,"missing");assert_eq!(s.remote_error,"timeout");
    }
}
