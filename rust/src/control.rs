//! 原生控制入口；必须先于目录初始化、日志、单实例和 GUI 分流。
//! MCP 沿用原插件的三个工具与 UI URI，bar_restart 只确保运行、不打开设置。
//! --state/--dump/--config-get/--config-update/--open-settings 是新增 CLI，不是 MCP 工具。
//! --install-plugin 只生成本地市场，绝不注册、启用或修改客户端插件库存。
use serde_json::{json, Map, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type ControlResult<T> = Result<T, &'static str>;
const PLUGIN: &str = "zcode-token-bar-control";
const VERSION: &str = "0.2.0";
const MARKET: &str = "dev-zcode-token-bar-native";
const RESOURCE: &str = "ui://zcode-token-bar-control/panel.html";
const MIME: &str = "text/html;profile=mcp-app";
const MAX_BYTES: usize = 1024 * 1024;
const FRESH_MS: u64 = 30_000;
const MARKER: &str = ".token-bar-control.json";
static SERIAL: AtomicU64 = AtomicU64::new(0);

// 浏览器内的 MCP Apps 消息桥，不依赖 Node、SDK 下载、HTTP 服务或外部脚本。
// 协议名及 2026-01-26 版本来自原 panel.html 内联的 @modelcontextprotocol/ext-apps。
const PANEL_HTML: &str = r##"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Token Bar</title><style>
:root{color-scheme:dark;--bg:var(--color-background-primary,#0d0d0d);--fg:var(--color-text-primary,#e6e6e6);--muted:var(--color-text-secondary,#8a8a8a);--line:var(--color-border-secondary,#2e2e2e)}
body{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:13px/1.6 "Cascadia Mono",Consolas,monospace}
h1{font-size:13px;margin:0 0 12px;letter-spacing:.5px}.row{display:flex;gap:10px;align-items:center;margin:10px 0;flex-wrap:wrap}.k{color:var(--muted);width:88px}.v{font-weight:600}.ok{color:var(--color-text-success,#3fb950)}.bad{color:var(--color-text-danger,#f85149)}
button{font:inherit;padding:8px 14px;cursor:pointer;background:var(--bg);color:var(--fg);border:1px solid var(--line)}button:hover{background:#242424}button:active{background:#333}button:focus-visible{outline:2px solid var(--fg);outline-offset:2px}button.primary{background:var(--fg);color:var(--bg)}button:disabled{opacity:.5;cursor:wait}
#msg{min-height:20px;color:var(--muted);white-space:pre-wrap;overflow-wrap:anywhere}pre{max-height:45vh;overflow:auto;white-space:pre-wrap;overflow-wrap:anywhere;font:inherit;border-top:1px solid var(--line);padding-top:10px}.hint{font-size:12px;color:var(--muted)}
</style></head><body><h1>TOKEN BAR</h1>
<div class="row"><span class="k">状态</span><span class="v" id="state">连接中…</span></div>
<div class="row"><span class="k">PID</span><span class="v" id="pid">—</span></div>
<div class="row"><button id="refresh" disabled>刷新状态</button><button id="start" class="primary" disabled>启动悬浮条</button><button id="doctor" disabled>查看诊断</button></div>
<p class="hint">已运行时保持当前窗口与会话，不会强制重启。</p><div id="msg" role="status" aria-live="polite"></div><pre id="details" hidden></pre>
<script>
(()=>{'use strict';const host=window.parent,pending=new Map();let next=1,ready=false,closed=false,busy=false,origin=null;
const el=id=>document.getElementById(id);const buttons=['refresh','start','doctor'];
function controls(){buttons.forEach(id=>{el(id).disabled=!ready||busy||closed;});}
function post(value){host.postMessage(value,origin&&origin!=='null'?origin:'*');}
function notify(method,params={}){if(!closed)post({jsonrpc:'2.0',method,params});}
function request(method,params){return new Promise((resolve,reject)=>{if(closed){reject(new Error('连接已关闭'));return;}const id=next++;const timer=setTimeout(()=>{pending.delete(id);reject(new Error('宿主响应超时，请重新打开面板'));},15000);pending.set(id,{resolve,reject,timer});post({jsonrpc:'2.0',id,method,params});});}
function stop(){closed=true;ready=false;for(const p of pending.values()){clearTimeout(p.timer);p.reject(new Error('连接已关闭'));}pending.clear();controls();}
window.addEventListener('message',event=>{if(event.source!==host)return;if(origin!==null&&event.origin!==origin)return;const m=event.data;if(!m||typeof m!=='object'||m.jsonrpc!=='2.0')return;
 if(Object.prototype.hasOwnProperty.call(m,'id')&&pending.has(m.id)&&('result'in m||'error'in m)){if(origin===null)origin=event.origin;const p=pending.get(m.id);pending.delete(m.id);clearTimeout(p.timer);if(m.error)p.reject(new Error('宿主拒绝了控制请求'));else p.resolve(m.result);return;}
 if(m.method==='ping'&&Object.prototype.hasOwnProperty.call(m,'id'))post({jsonrpc:'2.0',id:m.id,result:{}});
 if(m.method==='ui/resource-teardown'){if(Object.prototype.hasOwnProperty.call(m,'id'))post({jsonrpc:'2.0',id:m.id,result:{}});stop();}
 if(m.method==='ui/notifications/request-teardown')stop();
});
async function call(name){const r=await request('tools/call',{name,arguments:{}});if(!r||r.isError)throw new Error('控制操作失败，请查看本机诊断');if(r.structuredContent!==undefined)return r.structuredContent;const t=(r.content||[]).find(c=>c.type==='text');try{return JSON.parse(t?t.text:'{}');}catch{throw new Error('状态响应格式无效');}}
async function status(){const s=await call('bar_status');el('state').className='v '+(s.running?'ok':'bad');el('state').textContent=s.running?(s.phase==='starting'?'启动中…':s.phase==='state-stale'?'运行中（快照未更新）':'运行中'):(s.stale?'未运行（残留快照）':'未运行');el('pid').textContent=s.pid??'—';}
async function action(fn){if(!ready||busy)return;busy=true;controls();el('msg').textContent='';try{await fn();}catch{el('msg').textContent='操作未完成，请刷新状态或重新打开面板。';}finally{busy=false;controls();notify('ui/notifications/size-changed',{height:document.documentElement.scrollHeight});}}
el('refresh').addEventListener('click',()=>action(status));el('start').addEventListener('click',()=>action(async()=>{el('msg').textContent='正在启动…';const r=await call('bar_restart');el('msg').textContent=r.action==='already-running'?'悬浮条已在运行，保持现状。':'已创建悬浮条进程，请刷新状态确认。';await status();}));el('doctor').addEventListener('click',()=>action(async()=>{el('details').textContent=JSON.stringify(await call('bar_doctor'),null,2);el('details').hidden=false;}));
(async()=>{try{if(host===window)throw new Error('无宿主');await request('ui/initialize',{appInfo:{name:'zcode-token-bar-control',version:'0.2.0'},appCapabilities:{},protocolVersion:'2026-01-26'});notify('ui/notifications/initialized');ready=true;controls();await action(status);}catch{el('state').textContent='未连接';el('msg').textContent='请从 ZCode 的 Token Bar 面板打开，连接失败时可关闭后重试。';controls();}})();
if(typeof ResizeObserver!=='undefined')new ResizeObserver(()=>{if(ready)notify('ui/notifications/size-changed',{height:document.documentElement.scrollHeight});}).observe(document.body);
window.addEventListener('pagehide',stop);
})();
</script></body></html>"##;

/// args 不含可执行文件名；Ok(true) 表示调用方应立即返回，不启动 GUI。
pub fn handle_cli(args: &[String]) -> Result<bool, String> {
    let Some(first) = args.first().map(String::as_str) else { return Ok(false) };
    if !["--mcp", "--state", "--dump", "--config-get", "--config-update", "--open-settings", "--install-plugin"].contains(&first) {
        return Ok(false);
    }
    if first == "--mcp" {
        if args.len() != 1 { return Err("--mcp 不接受附加参数".into()); }
        // 不预先发现路径或读配置；initialize/list/resource 即使配置损坏仍能工作。
        return serve(io::stdin().lock(), io::stdout().lock(), &mut NativeBackend).map(|_| true).map_err(str::to_owned);
    }
    let result: ControlResult<Value> = (|| {
        match first {
            "--install-plugin" => {
                let out = match args {
                    [_] => crate::data_dir().join("control-plugin"),
                    [_, flag, path] if flag == "--output" => PathBuf::from(path),
                    _ => return Err("用法：--install-plugin [--output <绝对目录>]"),
                };
                let paths = crate::system::AppPaths::discover().map_err(|_| "无法确定应用路径")?;
                install_plugin(&out, &paths.exe, &paths.data_dir)
            }
            "--config-update" => {
                if args.len() != 2 { return Err("--config-update 需要一个 JSON 对象参数"); }
                if args[1].len() > 16 * 1024 { return Err("配置更新参数过大"); }
                let patch: Value = serde_json::from_str(&args[1]).map_err(|_| "配置更新 JSON 无效")?;
                update_config(&patch)
            }
            _ => {
                if args.len() != 1 { return Err("控制命令不接受附加参数"); }
                match first {
                    "--state" => {
                        let status = NativeBackend.status()?;
                        Ok(json!({"status":status,"snapshot":read_state()}))
                    }
                    "--dump" | "--open-settings" => {
                        let action = if first == "--dump" { "dump" } else { "settings" };
                        let requested = crate::win::signal(action);
                        Ok(json!({"requested":requested,"action":action,"message":if requested {
                            "请求已发送；不代表 GUI 已完成处理"
                        } else { "未找到运行中的实例事件" }}))
                    }
                    "--config-get" => {
                        let (settings, warning) = crate::settings::load_with_report();
                        if warning.is_some() { return Err("设置无法读取；原文件未修改"); }
                        let value = serde_json::to_value(&settings).map_err(|_| "无法读取设置")?;
                        Ok(public_config(&value))
                    }
                    _ => unreachable!(),
                }
            }
        }
    })();
    let value = result.map_err(str::to_owned)?;
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &value).map_err(|_| "无法写入控制输出".to_string())?;
    writeln!(out).map_err(|_| "无法写入控制输出".to_string())?;
    Ok(true)
}

trait Backend {
    fn status(&mut self) -> ControlResult<Value>;
    fn ensure_running(&mut self) -> ControlResult<Value>;
    fn doctor(&mut self) -> ControlResult<Value>;
}
struct NativeBackend;
impl Backend for NativeBackend {
    fn status(&mut self) -> ControlResult<Value> {
        let path = crate::data_dir().join("control-state.json");
        let state = read_state();
        let running = native::instance_exists()?;
        if running {
            let mut result = json!({"running":true,"stale":false});
            if let Some(state) = state { result["pid"] = state["pid"].clone(); }
            else { result["phase"] = json!(if path.exists() { "state-stale" } else { "starting" }); }
            Ok(result)
        } else { Ok(json!({"running":false,"stale":path.exists()})) }
    }
    fn ensure_running(&mut self) -> ControlResult<Value> {
        // 串行化不同 MCP server 的启动请求；此互斥量只在明确 tools/call 时创建。
        let _gate = native::start_guard()?;
        ensure_running(self.status()?, || {
            let paths = crate::system::AppPaths::discover().map_err(|_| "无法确定应用路径")?;
            let mut command = Command::new(&paths.exe);
            // main 对此内部标记的 Existing 分支静默返回，避免并发启动打开设置。
            command.arg("--control-start").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                .env("ZCODE_TOKEN_BAR_DATA", &paths.data_dir);
            #[cfg(windows)] {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000 | 0x0000_0200); // NO_WINDOW | NEW_PROCESS_GROUP
            }
            // 不用 --wait-zcode，也不发 settings 事件；原版面板也是直接拉起条面。
            let mut child = command.spawn().map_err(|_| "无法启动悬浮条进程")?;
            let pid = child.id();
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                if child.try_wait().map_err(|_| "无法查询新进程")?.is_some() {
                    if native::instance_exists()? { return Ok(pid); }
                    return Err("悬浮条进程提前退出，请检查本机日志");
                }
                if native::instance_exists()? { break; }
                std::thread::sleep(Duration::from_millis(40));
            }
            Ok(pid)
        })
    }
    fn doctor(&mut self) -> ControlResult<Value> {
        let paths = crate::system::AppPaths::discover().map_err(|_| "无法确定应用路径")?;
        let state_file = paths.data_dir.join("control-state.json");
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        let wp_dir = std::env::var_os("ZCODE_BEAUTIFY_DATA_DIR").filter(|s| !s.is_empty()).map(PathBuf::from)
            .or_else(|| home.map(|h| PathBuf::from(h).join(".zcode/cli/plugins/data/zcode-beautify@zcode-beautify")));
        let wp_file = wp_dir.map(|p| p.join("config.json"));
        let wp = wp_file.as_deref().and_then(|p| read_json(p).ok()).unwrap_or(Value::Null);
        let fit = wp.get("fit").and_then(Value::as_str).filter(|s| ["cover", "contain", "stretch", "smart"].contains(s));
        let image = wp.get("wallpaperPath").and_then(Value::as_str).map(Path::new);
        let backups = [paths.data_dir.join("registry-backups"), paths.backup_dir.clone()].iter()
            .filter_map(|p| fs::read_dir(p).ok()).map(|it| it.filter_map(Result::ok).count()).sum::<usize>();
        Ok(json!({
            "bar_dir":paths.exe.parent(),"bar_script":false,
            "bar_executable":paths.exe,"executable_exists":paths.exe.is_file(),"runtime":"rust",
            "data_dir":paths.data_dir,"data_dir_is_c":paths.data_dir.to_string_lossy().to_lowercase().starts_with("c:"),
            "lock":state_file,"lock_exists":state_file.is_file(),"lock_kind":"state-file; instance uses named mutex",
            "registry_backups":backups,"settings_file":crate::settings::settings_path(),
            "beautify_config":wp_file,"beautify_config_exists":wp_file.as_ref().is_some_and(|p| p.is_file()),
            "wallpaper_fit":fit,"wallpaper_path":image.and_then(Path::file_name).map(|s| s.to_string_lossy()),
            "wallpaper_exists":image.is_some_and(Path::is_file),
            "zcode_home":std::env::var_os("ZCODE_HOME").map(PathBuf::from),"status":self.status()?
        }))
    }
}
fn ensure_running(status: Value, start: impl FnOnce() -> ControlResult<u32>) -> ControlResult<Value> {
    if status.get("running").and_then(Value::as_bool) == Some(true) {
        let mut result = status;
        result["action"] = json!("already-running");
        Ok(result)
    } else {
        Ok(json!({"action":"started","started":true,"pid":start()?,"phase":"starting"}))
    }
}

fn tool_result(value: Value) -> Value {
    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
}
fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn dispatch(req: Value, backend: &mut impl Backend) -> Option<Value> {
    let Some(object) = req.as_object() else { return Some(error(Value::Null, -32600, "Invalid Request")); };
    // 通知不回包，也不执行 tools/call 等副作用。必须早于任何方法分派。
    let id = object.get("id")?.clone();
    if !(id.is_null() || id.is_string() || id.is_number()) {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    }
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error(id, -32600, "Invalid Request"));
    }
    let Some(method) = req.get("method").and_then(Value::as_str) else { return Some(error(id, -32600, "Invalid Request")); };
    let params = req.get("params");
    if params.is_some_and(|p| !p.is_object()) { return Some(error(id, -32602, "Invalid params")); }
    let result = match method {
        "initialize" => {
            let requested = req.pointer("/params/protocolVersion");
            if requested.is_some_and(|v| !v.is_string()) { return Some(error(id, -32602, "Invalid params")); }
            let version = requested.and_then(Value::as_str).filter(|v| ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(v)).unwrap_or("2025-06-18");
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false},"resources":{}},
                "serverInfo":{"name":PLUGIN,"version":VERSION}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":[
            {"name":"bar_status","description":"查询悬浮条是否在运行（含残留快照检测，不启动或打开窗口）","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"openWorldHint":false}},
            {"name":"bar_restart","description":"确保悬浮条运行：未运行时启动，已运行返回 already-running，不强制重启或打开设置","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bar_doctor","description":"只读列出悬浮条路径与状态，不修复配置、不联网","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"openWorldHint":false}}
        ]}),
        "tools/call" => {
            let Some(name) = req.pointer("/params/name").and_then(Value::as_str) else { return Some(error(id, -32602, "Invalid params")); };
            if req.pointer("/params/arguments").is_some_and(|v| !v.as_object().is_some_and(Map::is_empty)) {
                return Some(error(id, -32602, "工具只接受空参数对象"));
            }
            let value = match name {
                "bar_status" => backend.status(),
                "bar_restart" => backend.ensure_running(),
                "bar_doctor" => backend.doctor(),
                _ => return Some(error(id, -32602, "未知工具")),
            };
            match value {
                Ok(v) => tool_result(v),
                // 不传播系统错误、请求正文、参数、配置值和凭据。
                Err(_) => json!({"content":[{"type":"text","text":"控制操作失败，请在本机检查路径、权限和进程状态。"}],"isError":true}),
            }
        }
        "resources/list" => json!({"resources":[{"uri":RESOURCE,"name":"panel","mimeType":MIME}]}),
        "resources/read" => {
            if req.pointer("/params/uri").and_then(Value::as_str) != Some(RESOURCE) {
                return Some(error(id, -32602, "未知资源"));
            }
            json!({"contents":[{"uri":RESOURCE,"mimeType":MIME,"text":PANEL_HTML}]})
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

// 不用无限长的 lines()/read_line()；过长帧排空到换行后继续接受后续请求。
fn read_frame(input: &mut impl BufRead) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut any = false;
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() { return Ok(any.then_some((bytes, oversized))); }
        any = true;
        let end = chunk.iter().position(|b| *b == b'\n');
        let n = end.map_or(chunk.len(), |i| i + 1);
        if !oversized {
            if bytes.len().saturating_add(n) > MAX_BYTES { bytes.clear(); oversized = true; }
            else { bytes.extend_from_slice(&chunk[..n]); }
        }
        input.consume(n);
        if end.is_some() { return Ok(Some((bytes, oversized))); }
    }
}
fn serve(mut input: impl BufRead, mut output: impl Write, backend: &mut impl Backend) -> ControlResult<()> {
    while let Some((bytes, oversized)) = read_frame(&mut input).map_err(|_| "无法读取 MCP 输入")? {
        if !oversized && bytes.iter().all(u8::is_ascii_whitespace) { continue; }
        let response = if oversized { Some(error(Value::Null, -32600, "Request too large")) }
            else { match serde_json::from_slice(&bytes) {
                Ok(req) => dispatch(req, backend),
                Err(_) => Some(error(Value::Null, -32700, "Parse error")),
            }};
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response).map_err(|_| "无法写入 MCP 输出")?;
            output.write_all(b"\n").and_then(|_| output.flush()).map_err(|_| "无法写入 MCP 输出")?;
        }
    }
    Ok(())
}

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64 }
fn unique() -> String { format!("{}-{}-{}", now_ms(), std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed)) }
fn read_bytes(path: &Path) -> ControlResult<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("文件无法读取"),
    };
    if !file.metadata().map_err(|_| "文件无法读取")?.is_file() { return Err("目标不是普通文件"); }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| "文件无法读取")?;
    if bytes.len() > MAX_BYTES { return Err("文件超过大小限制"); }
    Ok(Some(bytes))
}
fn read_json(path: &Path) -> ControlResult<Value> {
    let bytes = read_bytes(path)?.ok_or("文件不存在")?;
    serde_json::from_slice(&bytes).map_err(|_| "JSON 文件格式无效")
}

#[derive(Clone)]
struct ProcessInfo { image: PathBuf, started_ms: u64 }
fn same_path(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        let p = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = p.to_string_lossy().replace('/', "\\");
        if cfg!(windows) { s.trim_start_matches("\\\\?\\").to_lowercase() } else { s }
    };
    key(a) == key(b)
}
fn safe_state(state: &Value) -> Value {
    let mut out = Map::new();
    for key in ["ok", "pinned", "visible", "hidden", "models_busy", "generating", "active"] {
        if let Some(v) = state.get(key).filter(|v| v.is_boolean()) { out.insert(key.into(), v.clone()); }
    }
    for key in ["tps", "ctx", "context_window", "cache_rate", "today_total", "session_total", "requests", "tools_total", "tools_errors"] {
        if let Some(v) = state.get(key).filter(|v| v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0)) {
            out.insert(key.into(), v.clone());
        }
    }
    // 标题、错误正文、壁纸诊断可能夹带路径或私密文本，不发布原文。
    if let Some(v) = state.get("error").and_then(Value::as_str) { out.insert("has_error".into(), json!(!v.is_empty())); }
    if let Some(v) = state.get("wallpaper").and_then(Value::as_str) { out.insert("wallpaper_reported".into(), json!(!v.is_empty())); }
    for key in ["has_error", "wallpaper_reported"] {
        if let Some(v) = state.get(key).filter(|v| v.is_boolean()) { out.insert(key.into(), v.clone()); }
    }
    Value::Object(out)
}
fn state_document(state: &Value, pid: u32, started_ms: u64, written_ms: u64) -> Value {
    json!({"pid":pid,"written_at_ms":written_ms,"process_started_at_ms":started_ms,"runtime":"rust","state":safe_state(state)})
}
fn validate_state(value: Value, exe: &Path, now: u64, inspect: impl FnOnce(u32) -> Option<ProcessInfo>) -> Option<Value> {
    if value.get("runtime")?.as_str()? != "rust" { return None; }
    let pid: u32 = value.get("pid")?.as_u64()?.try_into().ok()?;
    let written = value.get("written_at_ms")?.as_u64()?;
    if pid == 0 || written > now || now - written > FRESH_MS { return None; }
    let info = inspect(pid)?;
    let started = value.get("process_started_at_ms")?.as_u64()?;
    if started == 0 || started > written || info.started_ms != started || !same_path(&info.image, exe) { return None; }
    let state = value.get("state")?;
    if !state.is_object() { return None; }
    Some(state_document(state, pid, started, written))
}
/// 只读当前快照；要求 PID 存活、同 exe、创建时间一致、30 秒内且不来自未来。
pub fn read_state() -> Option<Value> {
    let exe = std::env::current_exe().ok()?;
    let value = read_json(&crate::data_dir().join("control-state.json")).ok()?;
    validate_state(value, &exe, now_ms(), native::process_info)
}
/// GUI 每 5 秒/收到 dump 时调用。过滤所有未知和自由文本字段，不存配置或凭据。
pub fn publish_state(state: &Value) -> Result<(), String> {
    if !state.is_object() { return Err("状态必须是 JSON 对象".into()); }
    let pid = std::process::id();
    let process = native::process_info(pid).ok_or("无法核验状态发布进程")?;
    let value = state_document(state, pid, process.started_ms, now_ms());
    crate::storage::json(&crate::data_dir().join("control-state.json"), &value)
        .map_err(|_| "无法发布状态快照".to_string())
}

const CONFIG_KEYS: &[&str] = &["anchor", "scope", "offset_x", "offset_y", "opacity", "font_px", "font_family", "panel_opacity", "labels", "poll_ms", "always_on_top", "hide_when_inactive", "auto_update", "show"];
const SHOW_KEYS: &[&str] = &["speed", "ctx", "turn", "session", "today", "tools", "cache"];
fn public_config(value: &Value) -> Value {
    let mut out = Map::new();
    for key in CONFIG_KEYS {
        if let Some(v) = value.get(key) {
            if *key == "show" {
                let mut items = Map::new();
                for name in SHOW_KEYS { if let Some(b) = v.get(name).and_then(Value::as_bool) { items.insert((*name).into(), json!(b)); } }
                out.insert((*key).into(), Value::Object(items));
            } else { out.insert((*key).into(), v.clone()); }
        }
    }
    Value::Object(out)
}
fn merge_config(mut current: Value, patch: &Value) -> ControlResult<Value> {
    let obj = patch.as_object().filter(|o| !o.is_empty()).ok_or("配置更新必须是非空对象")?;
    if !current.is_object() { return Err("现有设置格式无效"); }
    for (key, value) in obj {
        let integer = |min: i64, max: i64| value.as_i64().is_some_and(|n| (min..=max).contains(&n));
        let number = |min: f64, max: f64| value.as_f64().is_some_and(|n| n.is_finite() && n >= min && n <= max);
        let valid = match key.as_str() {
            "anchor" => value.as_str().is_some_and(|s| ["bottom-left", "bottom-center", "bottom-right", "top-left", "top-center", "top-right", "free"].contains(&s)),
            "scope" => value.as_str().is_some_and(|s| ["window", "screen"].contains(&s)),
            "offset_x" | "offset_y" => integer(-4000, 4000),
            "opacity" => number(0.30, 1.0), "panel_opacity" => number(0.55, 1.0),
            "font_px" => integer(9, 28), "poll_ms" => integer(300, 30_000),
            "font_family" => value.as_str().is_some_and(|s| s.len() <= 128 && !s.chars().any(char::is_control)),
            "labels" | "always_on_top" | "hide_when_inactive" | "auto_update" => value.is_boolean(),
            "show" => value.as_object().is_some_and(|m| !m.is_empty() && m.iter().all(|(k,v)| SHOW_KEYS.contains(&k.as_str()) && v.is_boolean())),
            _ => false,
        };
        if !valid { return Err("配置更新含不支持的字段、类型或范围"); }
        if key == "show" {
            if !current["show"].is_object() { current["show"] = json!({}); }
            for (k, v) in value.as_object().ok_or("显示项无效")? { current["show"][k] = v.clone(); }
        } else { current[key] = value.clone(); }
    }
    Ok(current)
}
fn update_config(patch: &Value) -> ControlResult<Value> {
    let path = crate::settings::settings_path();
    check_plain_path(&path)?;
    let original = read_bytes(&path)?;
    let (current, warning) = crate::settings::load_with_report();
    if warning.is_some() { return Err("现有设置无法读取，未修改任何配置"); }
    let value = merge_config(serde_json::to_value(current).map_err(|_| "设置转换失败")?, patch)?;
    let mut current: crate::settings::Settings = serde_json::from_value(value).map_err(|_| "更新后配置无效")?;
    current.normalize();
    let backup = commit_settings(&path, original.as_deref(), || crate::settings::save(&current).map_err(|_| "保存设置失败"))?;
    let requested = crate::win::signal("reload");
    Ok(json!({"saved":true,"reload_requested":requested,"applied":false,"backup":backup,
        "message":if requested {"已保存并请求重新载入；尚未确认 GUI 应用"} else {"已保存，未找到 reload 事件，下次启动生效"}}))
}
fn write_new(path: &Path, bytes: &[u8]) -> ControlResult<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|_| "无法新建文件")?;
    if file.write_all(bytes).and_then(|_| file.sync_all()).is_err() {
        drop(file); let _ = fs::remove_file(path); return Err("文件写入或刷盘失败");
    }
    Ok(())
}
fn commit_settings(path: &Path, original: Option<&[u8]>, save: impl FnOnce() -> ControlResult<()>) -> ControlResult<Option<PathBuf>> {
    if read_bytes(path)?.as_deref() != original { return Err("设置已被其他进程修改，请重新读取后重试"); }
    let backup = if let Some(bytes) = original {
        let dir = path.parent().ok_or("设置没有父目录")?.join("backups");
        check_plain_path(&dir)?;
        fs::create_dir_all(&dir).map_err(|_| "无法创建备份目录，未修改设置")?;
        let backup = dir.join(format!("settings-rs.json.bak-{}", unique()));
        write_new(&backup, bytes).map_err(|_| "备份失败，未修改设置")?;
        Some(backup)
    } else { None };
    if read_bytes(path)?.as_deref() != original { return Err("设置已被其他进程修改，请重新读取后重试"); }
    save()?;
    Ok(backup)
}

// 生成目录只包含以下三个文件；更新前对已生成内容和目录树做精确核验。
fn manifest(exe: &Path, data: &Path, version: &str) -> Value {
    json!({"name":PLUGIN,"version":version,"description":"Token Bar 原生控制面板：状态、启动与诊断。",
        "author":{"name":"suiren0219"},"license":"MIT",
        "mcpServers":{"bar":{"type":"stdio","command":exe,"args":["--mcp"],"env":{"ZCODE_TOKEN_BAR_DATA":data}}},
        "ui":{"surfaces":[{"id":"control","title":{"en":"Token Bar","zh-CN":"Token Bar"},"server":"bar","resourceUri":RESOURCE,"availability":"session"}]}})
}
fn marketplace(version: &str) -> Value {
    json!({"name":MARKET,"owner":{"name":"suiren0219"},"version":version,"plugins":[{
        "name":PLUGIN,"source":format!("./{PLUGIN}"),"version":version,"description":"Token Bar 原生控制面板：状态、启动与诊断。",
        "displayName":"Token Bar","category":"utilities","license":"MIT"
    }]})
}
fn marker(exe: &Path, data: &Path, version: &str) -> Value {
    json!({"generator":"zcode-token-bar-control-rust","format":1,"exe":exe,"data_dir":data,"version":version})
}
fn generated_files(exe: &Path, data: &Path, version: &str) -> [(PathBuf, Value); 3] {
    [(MARKER.into(), marker(exe, data, version)), ("marketplace.json".into(), marketplace(version)),
        (PathBuf::from(PLUGIN).join(".zcode-plugin/plugin.json"), manifest(exe, data, version))]
}
fn check_plain_path(path: &Path) -> ControlResult<()> {
    for p in path.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) => {
                if m.file_type().is_symlink() { return Err("不写入符号链接或重解析目录"); }
                #[cfg(windows)] {
                    use std::os::windows::fs::MetadataExt;
                    if m.file_attributes() & 0x400 != 0 { return Err("不写入符号链接或重解析目录"); }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return Err("无法核验目标路径"),
        }
    }
    Ok(())
}
fn validate_install_dir(out: &Path) -> ControlResult<bool> {
    check_plain_path(out)?;
    if !out.exists() { return Ok(false); }
    if !out.is_dir() { return Err("输出位置已有文件，未覆盖"); }
    let entries = fs::read_dir(out).map_err(|_| "无法读取输出目录")?.collect::<Result<Vec<_>, _>>().map_err(|_| "无法读取输出目录")?;
    if entries.is_empty() { return Ok(true); }
    let old = read_json(&out.join(MARKER)).map_err(|_| "输出目录不是已知生成的插件包，未覆盖")?;
    let exe = old.get("exe").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or("生成标记无效，未覆盖")?;
    let data = old.get("data_dir").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or("生成标记无效，未覆盖")?;
    let version = old.get("version").and_then(Value::as_str).filter(|s| s.len() < 64).ok_or("生成标记无效，未覆盖")?;
    if old != marker(&exe, &data, version) { return Err("生成标记已修改，未覆盖"); }
    let files = generated_files(&exe, &data, version);
    fn walk(root: &Path, relative: &Path, files: &[(PathBuf, Value)]) -> ControlResult<()> {
        let dir = root.join(relative);
        for entry in fs::read_dir(&dir).map_err(|_| "无法核验插件目录")? {
            let path = entry.map_err(|_| "无法核验插件目录")?.path();
            check_plain_path(&path)?;
            let rel = path.strip_prefix(root).map_err(|_| "插件目录无效")?;
            if path.is_dir() {
                if !files.iter().any(|(p,_)| p.starts_with(rel) && p != rel) { return Err("插件目录含未知目录，未覆盖"); }
                walk(root, rel, files)?;
            } else if !files.iter().any(|(p,_)| p == rel) { return Err("插件目录含未知文件，未覆盖"); }
        }
        Ok(())
    }
    walk(out, Path::new(""), &files)?;
    for (relative, value) in files {
        if read_json(&out.join(relative))? != value { return Err("插件生成文件已修改，未覆盖"); }
    }
    Ok(true)
}
fn install_plugin(out: &Path, exe: &Path, data: &Path) -> ControlResult<Value> {
    if !out.is_absolute() || !exe.is_absolute() || !data.is_absolute() { return Err("安装包、exe 和数据目录必须使用绝对路径"); }
    if out.file_name().is_none() || out.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return Err("安装包输出目录无效"); }
    let existed = validate_install_dir(out)?;
    let parent = out.parent().ok_or("输出目录没有父目录")?;
    check_plain_path(parent)?;
    fs::create_dir_all(parent).map_err(|_| "无法创建安装包父目录")?;
    let name = out.file_name().ok_or("输出目录无效")?.to_string_lossy();
    let stage = parent.join(format!(".{name}.stage-{}", unique()));
    fs::create_dir(&stage).map_err(|_| "无法创建安装包临时目录")?;
    let result = (|| {
        for (relative, value) in generated_files(exe, data, VERSION) {
            let path = stage.join(relative);
            fs::create_dir_all(path.parent().ok_or("插件路径无效")?).map_err(|_| "无法创建插件目录")?;
            let bytes = serde_json::to_vec_pretty(&value).map_err(|_| "插件序列化失败")?;
            write_new(&path, &bytes)?;
        }
        // 发布前再次检查，避免准备过程中出现的用户文件被覆盖。
        if validate_install_dir(out)? != existed { return Err("输出目录已变化，未覆盖"); }
        let backup = if existed {
            let backup = parent.join(format!("{name}.bak-{}", unique()));
            fs::rename(out, &backup).map_err(|_| "备份原插件目录失败，未覆盖")?;
            Some(backup)
        } else { None };
        if fs::rename(&stage, out).is_err() {
            if let Some(ref backup) = backup { if !out.exists() { let _ = fs::rename(backup, out); } }
            return Err("发布插件包失败；已有备份保留在输出目录旁");
        }
        Ok(json!({"generated":true,"registered":false,"installed":false,"marketplace":MARKET,
            "plugin_id":format!("{PLUGIN}@{MARKET}"),"version":VERSION,"marketplace_root":out,
            "catalog":out.join("marketplace.json"),"manifest":out.join(PLUGIN).join(".zcode-plugin/plugin.json"),
            "backup":backup,"message":"仅生成本地插件包。请在插件市场添加此市场根目录，再手动安装。"}))
    })();
    if stage.exists() { let _ = fs::remove_dir_all(stage); }
    result
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::{core::{PCWSTR, PWSTR}, Win32::{Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0}, System::Threading::{
        CreateMutexW, GetExitCodeProcess, GetProcessTimes, OpenMutexW, OpenProcess, QueryFullProcessImageNameW,
        ReleaseMutex, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZATION_SYNCHRONIZE,
    }}};
    struct Handle(HANDLE);
    impl Drop for Handle { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } } }
    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
    pub(super) fn process_info(pid: u32) -> Option<ProcessInfo> {
        unsafe {
            let h = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
            let mut code = 0;
            GetExitCodeProcess(h.0, &mut code).ok()?;
            if code != 259 { return None; }
            let mut buffer = vec![0u16; 32768]; let mut size = buffer.len() as u32;
            QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size).ok()?;
            let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
            // 退出码恰为 STILL_ACTIVE(259) 的已退出进程也不能接受。
            if exited.dwHighDateTime != 0 || exited.dwLowDateTime != 0 { return None; }
            let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            Some(ProcessInfo { image: PathBuf::from(String::from_utf16(&buffer[..size as usize]).ok()?), started_ms: ticks.checked_div(10_000)?.checked_sub(11_644_473_600_000)? })
        }
    }
    pub(super) fn instance_exists() -> ControlResult<bool> {
        unsafe {
            match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide("Local\\zcode-token-bar-rs").as_ptr())) {
                Ok(h) => { drop(Handle(h)); Ok(true) }
                Err(e) if e.code() == windows::core::HRESULT::from_win32(2) => Ok(false),
                Err(_) => Err("无法查询悬浮条实例"),
            }
        }
    }
    pub(super) struct StartGuard(Handle);
    impl Drop for StartGuard { fn drop(&mut self) { unsafe { let _ = ReleaseMutex(self.0.0); } } }
    pub(super) fn start_guard() -> ControlResult<StartGuard> {
        unsafe {
            let h = Handle(CreateMutexW(None, false, PCWSTR(wide("Local\\zcode-token-bar-rs-control-start").as_ptr())).map_err(|_| "无法串行化启动请求")?);
            let result = WaitForSingleObject(h.0, 5_000);
            if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED { Ok(StartGuard(h)) } else { Err("另一启动请求仍在处理，请稍后重试") }
        }
    }
}
#[cfg(not(windows))]
mod native {
    use super::*;
    pub(super) fn process_info(_: u32) -> Option<ProcessInfo> { None }
    pub(super) fn instance_exists() -> ControlResult<bool> { Err("实例控制仅支持 Windows") }
    pub(super) fn start_guard() -> ControlResult<()> { Err("实例控制仅支持 Windows") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[derive(Default)] struct Fake { reads: usize, starts: usize, fail: bool }
    impl Backend for Fake {
        fn status(&mut self) -> ControlResult<Value> { self.reads += 1; Ok(json!({"running":false,"stale":false})) }
        fn ensure_running(&mut self) -> ControlResult<Value> { self.starts += 1; if self.fail { Err("secret must not escape") } else { Ok(json!({"action":"started","pid":42})) } }
        fn doctor(&mut self) -> ControlResult<Value> { self.reads += 1; Ok(json!({"runtime":"rust"})) }
    }
    fn rpc(method: &str, params: Value) -> Value { json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}) }
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self {
        let p = std::env::current_exe().unwrap().parent().unwrap().join(format!("control-test-{}", unique()));
        fs::create_dir(&p).unwrap(); Self(p)
    }}
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    #[test] fn old_tools_and_resource_contract() {
        let mut f = Fake::default();
        let r = dispatch(rpc("initialize", json!({"protocolVersion":"2025-06-18"})), &mut f).unwrap();
        assert_eq!(r["result"]["serverInfo"]["name"], PLUGIN);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let r = dispatch(rpc("tools/list", json!({})), &mut f).unwrap();
        let names: Vec<_> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["bar_status", "bar_restart", "bar_doctor"]);
        let r = dispatch(rpc("resources/read", json!({"uri":RESOURCE})), &mut f).unwrap();
        assert_eq!(r["result"]["contents"][0]["mimeType"], MIME);
        assert!(r["result"]["contents"][0]["text"].as_str().unwrap().contains("ui/initialize"));
        assert!(!PANEL_HTML.contains("fetch("));
        assert!(!PANEL_HTML.contains("src=\"http"));
        assert_eq!((f.reads, f.starts), (0, 0));
    }
    #[test] fn notifications_never_reply_or_execute() {
        let mut f = Fake::default();
        for method in ["initialize", "ping", "tools/list", "tools/call", "resources/read", "notifications/initialized"] {
            let r = json!({"jsonrpc":"2.0","method":method,"params":{"name":"bar_restart"}});
            assert!(dispatch(r, &mut f).is_none());
        }
        assert_eq!((f.reads, f.starts), (0, 0));
    }
    #[test] fn tool_responses_are_structured_and_unknown_inputs_are_safe() {
        let mut f = Fake::default();
        let r = dispatch(rpc("tools/call", json!({"name":"bar_status","arguments":{}})), &mut f).unwrap();
        assert_eq!(r["result"]["structuredContent"]["running"], false);
        assert_eq!(serde_json::from_str::<Value>(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap(), r["result"]["structuredContent"]);
        for req in [rpc("secret-method", json!({})), rpc("resources/read", json!({"uri":"secret-uri"})), rpc("tools/call", json!({"name":"bar_restart","arguments":{"token":"secret"}}))] {
            let r = dispatch(req, &mut f).unwrap(); assert!(r.get("error").is_some()); assert!(!r.to_string().contains("secret"));
        }
        f.fail = true;
        let r = dispatch(rpc("tools/call", json!({"name":"bar_restart"})), &mut f).unwrap();
        assert_eq!(r["result"]["isError"], true); assert!(!r.to_string().contains("secret"));
    }
    #[test] fn ensure_running_does_not_start_or_signal_an_existing_instance() {
        let r = ensure_running(json!({"running":true,"stale":false,"pid":42}), || panic!("must not start")).unwrap();
        assert_eq!(r["action"], "already-running"); assert_eq!(r["pid"], 42);
        let count = Cell::new(0);
        let r = ensure_running(json!({"running":false,"stale":true}), || { count.set(count.get()+1); Ok(99) }).unwrap();
        assert_eq!(count.get(), 1); assert_eq!(r["pid"], 99);
    }
    #[test] fn bounded_stdio_recovers_from_bad_and_oversized_frames() {
        let mut input = b"not-json\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_BYTES + 1)); input.push(b'\n');
        input.extend_from_slice(rpc("ping", json!({})).to_string().as_bytes()); // EOF 无换行也处理
        let mut output = Vec::new(); serve(io::Cursor::new(input), &mut output, &mut Fake::default()).unwrap();
        let rows: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect();
        assert_eq!(rows.len(), 3); assert_eq!(rows[0]["error"]["code"], -32700);
        assert_eq!(rows[1]["error"]["code"], -32600); assert_eq!(rows[2]["result"], json!({}));
    }
    #[test] fn state_rejects_dead_wrong_reused_stale_and_future_processes() {
        let exe = std::env::current_exe().unwrap();
        let v = state_document(&json!({"ok":true}), 42, 1000, 2000);
        let inspect = |_:u32| Some(ProcessInfo {image:exe.clone(),started_ms:1000});
        assert!(validate_state(v.clone(), &exe, 3000, inspect).is_some());
        assert!(validate_state(v.clone(), &exe, 32001, inspect).is_none());
        assert!(validate_state(v.clone(), &exe, 1999, inspect).is_none());
        assert!(validate_state(v.clone(), &exe, 3000, |_| None).is_none());
        assert!(validate_state(v.clone(), &exe, 3000, |_| Some(ProcessInfo{image:exe.clone(),started_ms:2000})).is_none());
        assert!(validate_state(v, &exe, 3000, |_| Some(ProcessInfo{image:exe.with_extension("other"),started_ms:1000})).is_none());
    }
    #[test] fn state_and_config_never_dump_credentials_or_free_text() {
        let s = state_document(&json!({"ok":true,"requests":3,"token":"secret","session":"secret","error":"secret","wallpaper":"secret","nested":{"apiKey":"secret"}}),1,1,2);
        assert!(!s.to_string().contains("secret")); assert_eq!(s["state"]["requests"],3); assert_eq!(s["state"]["has_error"],true);
        let c = public_config(&json!({"anchor":"top-left","remote":{"key":"secret"},"extra":"secret","show":{"tools":true,"token":"secret"}}));
        assert!(!c.to_string().contains("secret")); assert_eq!(c["show"]["tools"],true);
    }
    #[test] fn config_patch_merges_show_and_rejects_unknown_or_invalid_values() {
        let old = json!({"show":{"speed":false,"cache":true},"future":{"keep":7}});
        let new = merge_config(old.clone(), &json!({"show":{"cache":false}})).unwrap();
        assert_eq!(new["show"]["speed"],false); assert_eq!(new["future"],old["future"]);
        for patch in [json!({"show":{"token":true}}), json!({"show":{"tools":"true"}}),json!({"opacity":5}),json!({"poll_ms":1}),json!({"remote":{}}),json!({})] { assert!(merge_config(old.clone(), &patch).is_err()); }
    }
    #[test] fn failed_backup_never_calls_save_and_old_bytes_survive() {
        let t=Temp::new(); let p=t.0.join("settings-rs.json"); fs::write(&p,b"original").unwrap(); fs::write(t.0.join("backups"),b"blocking file").unwrap();
        let called=Cell::new(false);
        assert!(commit_settings(&p,Some(b"original"),||{called.set(true);Ok(())}).is_err());
        assert!(!called.get()); assert_eq!(fs::read(&p).unwrap(),b"original");
    }
    #[test] fn backup_is_exact_and_concurrent_change_prevents_save() {
        let t=Temp::new(); let p=t.0.join("settings-rs.json"); fs::write(&p,b"old").unwrap();
        let backup=commit_settings(&p,Some(b"old"),||{fs::write(&p,b"new").unwrap();Ok(())}).unwrap().unwrap();
        assert_eq!(fs::read(backup).unwrap(),b"old");
        assert!(commit_settings(&p,Some(b"old"),||panic!("changed file must not save")).is_err());
    }
    #[test] fn plugin_generation_matches_host_schema_and_preserves_backup() {
        let t=Temp::new(); let out=t.0.join("market"); let exe=std::env::current_exe().unwrap();
        let r=install_plugin(&out,&exe,&t.0).unwrap();
        assert_eq!(r["registered"],false);assert_eq!(r["installed"],false);
        let m=read_json(&out.join(PLUGIN).join(".zcode-plugin/plugin.json")).unwrap();
        assert_eq!(m["mcpServers"]["bar"]["args"],json!(["--mcp"]));assert_eq!(m["mcpServers"]["bar"]["type"],"stdio");
        assert_eq!(m["ui"]["surfaces"][0]["resourceUri"],RESOURCE);
        assert_eq!(read_json(&out.join("marketplace.json")).unwrap()["plugins"][0]["source"],format!("./{PLUGIN}"));
        let r=install_plugin(&out,&exe,&t.0).unwrap();let backup=PathBuf::from(r["backup"].as_str().unwrap());
        assert_eq!(read_json(&backup.join(PLUGIN).join(".zcode-plugin/plugin.json")).unwrap(),m);
    }
    #[test] fn plugin_refuses_unknown_dirs_files_and_modified_generated_files() {
        let t=Temp::new();let exe=std::env::current_exe().unwrap();let out=t.0.join("market");
        fs::create_dir(&out).unwrap();fs::write(out.join("user.json"),b"private").unwrap();
        assert!(install_plugin(&out,&exe,&t.0).is_err());assert_eq!(fs::read(out.join("user.json")).unwrap(),b"private");
        fs::remove_file(out.join("user.json")).unwrap();install_plugin(&out,&exe,&t.0).unwrap();
        fs::create_dir(out.join("unknown")).unwrap();assert!(install_plugin(&out,&exe,&t.0).is_err());
        fs::remove_dir(out.join("unknown")).unwrap();fs::write(out.join("marketplace.json"),b"{}").unwrap();
        assert!(install_plugin(&out,&exe,&t.0).is_err());assert_eq!(fs::read(out.join("marketplace.json")).unwrap(),b"{}");
        let file=t.0.join("file");fs::write(&file,b"private").unwrap();assert!(install_plugin(&file,&exe,&t.0).is_err());
    }
    #[cfg(windows)]
    #[test] fn native_identity_checks_current_process_without_signalling() {
        let exe=std::env::current_exe().unwrap();let p=native::process_info(std::process::id()).unwrap();
        assert!(same_path(&p.image,&exe));assert!(p.started_ms>0&&p.started_ms<=now_ms());
        assert!(native::process_info(0).is_none());
    }
}
