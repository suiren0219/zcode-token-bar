//! 独立本地回归：仅使用临时假配置及 127.0.0.1 HTTP，不读取私人连接文件。
use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Condvar};
use std::thread;

const KEY: &str = "fixture-only-key-never-live";
const PRIVATE: &str = "private-sentinel-never-display";
const REPLY: &str = "哥哥，我在。慢慢来就好。";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture { directory: PathBuf, path: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("companion-rs-{}-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("connection.json");
        Self { directory, path }
    }
    fn write(&self, value: Value) { fs::write(&self.path, serde_json::to_vec(&value).unwrap()).unwrap(); }
    fn settings(&self) -> CompanionSettings {
        CompanionSettings { enabled: true, config_path: self.path.to_string_lossy().into_owned(),
            cooldown_seconds: 0, hourly_limit: 100, ..Default::default() }
    }
    fn connection(&self, url: &str, protocol: Protocol) -> CompanionSettings {
        self.write(match protocol {
            Protocol::AstrBot => json!({"base_url":url,"key":KEY,"config_id":"fixture-route"}),
            Protocol::OpenAi => json!({"base_url":url,"key":KEY,"protocol":"openai","model":"fixture-model"}),
            Protocol::Anthropic => json!({"base_url":url,"key":KEY,"protocol":"anthropic","model":"fixture-model"}),
            Protocol::Responses => json!({"base_url":url,"key":KEY,"protocol":"responses","model":"fixture-model"}),
        });
        self.settings()
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.directory); } }

#[derive(Clone)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Gate {
    fn new() -> Self { Self(Arc::new((Mutex::new(false), Condvar::new()))) }
    fn release(&self) { let (lock, wake) = &*self.0; *lock.lock().unwrap() = true; wake.notify_all(); }
    fn wait(&self) {
        let (lock, wake) = &*self.0;
        let _ = wake.wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |open| !*open).unwrap();
    }
}
struct Plan {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    gate: Option<Gate>,
    chunk_delay: Duration,
    chunk_size: usize,
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}
struct Ticket { started: Arc<AtomicBool>, finished: Arc<AtomicBool>, gate: Option<Gate> }
impl Ticket {
    fn started(&self) { wait(|| self.started.load(Ordering::Acquire)); }
    fn release(&self) { if let Some(gate) = &self.gate { gate.release(); } }
}
#[derive(Debug)]
struct Request { path: String, headers: HashMap<String, String>, body: Value }
struct Server {
    url: String,
    plans: mpsc::Sender<Plan>,
    requests: mpsc::Receiver<Request>,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    gates: Mutex<Vec<Gate>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (plans, plan_rx) = mpsc::channel::<Plan>();
        let (request_tx, requests) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let plan = plan_rx.try_recv().unwrap_or_else(|_| Plan {
                            status: 500, body: b"unexpected-fixture-request".to_vec(), headers: Vec::new(), gate: None,
                            chunk_delay: Duration::ZERO, chunk_size: 65536,
                            started: Arc::new(AtomicBool::new(false)), finished: Arc::new(AtomicBool::new(false)),
                        });
                        let tx = request_tx.clone();
                        workers.push(thread::spawn(move || serve(stream, plan, tx)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            for worker in workers { worker.join().unwrap(); }
        });
        Self { url, plans, requests, stopping, thread: Some(thread), gates: Mutex::new(Vec::new()) }
    }
    fn plan(&self, status: u16, body: impl AsRef<[u8]>, headers: Vec<(String,String)>, gated: bool,
        chunk_size: usize, delay: Duration) -> Ticket {
        let gate = gated.then(Gate::new);
        if let Some(gate) = &gate { self.gates.lock().unwrap().push(gate.clone()); }
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        self.plans.send(Plan { status, body: body.as_ref().into(), headers, gate: gate.clone(),
            chunk_delay: delay, chunk_size, started: started.clone(), finished: finished.clone() }).unwrap();
        Ticket { started, finished, gate }
    }
    fn reply(&self, text: &str, gated: bool) -> Ticket {
        self.plan(200, serde_json::to_vec(&json!({"reply":text})).unwrap(), Vec::new(), gated, 65536, Duration::ZERO)
    }
    fn request(&self) -> Request { self.requests.recv_timeout(Duration::from_secs(3)).expect("本地服务未收到请求") }
    fn no_request(&self) { assert!(self.requests.recv_timeout(Duration::from_millis(260)).is_err(), "取消或过期后发出了额外请求"); }
}
impl Drop for Server {
    fn drop(&mut self) {
        for gate in self.gates.lock().unwrap().iter() { gate.release(); }
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
    }
}
fn serve(mut stream: TcpStream, plan: Plan, tx: mpsc::Sender<Request>) {
    // Windows 的 accept 会继承 listener 的非阻塞模式。这里是同步 HTTP
    // fixture，必须恢复阻塞，否则首字节/正文稍晚到达的 WouldBlock 会被
    // 当成网络断开，消耗 plan 并把预期 429/timeout 偶发变为 Network。
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = Vec::new();
    let mut block = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut block) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&block[..n]),
        }
        if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") { break index + 4; }
        assert!(bytes.len() < 32768);
    };
    let raw = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let path = raw.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_owned();
    let headers: HashMap<_,_> = raw.lines().skip(1).filter_map(|line| line.split_once(':'))
        .map(|(k,v)| (k.to_ascii_lowercase(),v.trim().to_owned())).collect();
    let length = headers.get("content-length").unwrap().parse::<usize>().unwrap();
    while bytes.len() < header_end + length {
        match stream.read(&mut block) { Ok(0) | Err(_) => return, Ok(n) => bytes.extend_from_slice(&block[..n]) }
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    tx.send(Request { path, headers, body }).unwrap();
    plan.started.store(true, Ordering::Release);
    if let Some(gate) = &plan.gate { gate.wait(); }
    let mut response = format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\n", plan.status);
    if !plan.headers.iter().any(|(name,_)| name.eq_ignore_ascii_case("Content-Type")) { response.push_str("Content-Type: application/json\r\n"); }
    if !plan.headers.iter().any(|(name,_)| name.eq_ignore_ascii_case("Content-Length") || name.eq_ignore_ascii_case("Transfer-Encoding")) {
        response.push_str(&format!("Content-Length: {}\r\n", plan.body.len()));
    }
    for (name,value) in plan.headers { response.push_str(&format!("{name}: {value}\r\n")); }
    response.push_str("\r\n");
    if stream.write_all(response.as_bytes()).is_ok() {
        for chunk in plan.body.chunks(plan.chunk_size.max(1)) {
            thread::sleep(plan.chunk_delay);
            if stream.write_all(chunk).and_then(|_| stream.flush()).is_err() { break; }
        }
    }
    plan.finished.store(true, Ordering::Release);
}
#[track_caller]
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !predicate() {
        assert!(Instant::now() < deadline, "等待陪伴测试状态超时");
        thread::sleep(Duration::from_millis(5));
    }
}
fn ready(runtime: &Runtime) { wait(|| runtime.status().configured); }
fn event(runtime: &Runtime, kind: EventKind) -> CompanionEvent {
    let mut found = None;
    wait(|| {
        for e in runtime.drain() { if e.kind == kind && found.is_none() { found = Some(e); } }
        found.is_some()
    });
    found.unwrap()
}
#[track_caller]
fn error_event(runtime: &Runtime, expected: CompanionError) -> CompanionEvent {
    let event = event(runtime, EventKind::Error);
    safe(&event.message);
    assert_eq!(event.status.error, Some(expected),
        "错误类别不符：message={:?}, health={:?}, source={:?}, request_id={}",
        event.message, event.status.state, event.source, event.request_id);
    assert_eq!(event.message, expected.message());
    event
}
fn automatic(errors: u64) -> Signal {
    Signal::Auto(AutoSignal { kind: AutoKind::ToolFailures, occurred_at: epoch_seconds(), duration_ms: 1000, tool_errors: errors })
}
fn safe(text: &str) { for private in [KEY, PRIVATE, "fixture.invalid", "Bearer ", "Traceback"] { assert!(!text.contains(private), "泄露测试私密字段"); } }

#[test]
fn mock_accepts_delayed_headers_and_body_without_consuming_response_early() {
    let server = Server::new();
    let reply = server.reply(REPLY, false);
    let mut stream = TcpStream::connect(server.url.strip_prefix("http://").unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    // accept 先于首字节，正文又晚于请求头：覆盖 Windows 继承非阻塞模式的竞态。
    thread::sleep(Duration::from_millis(40));
    let body = b"{\"fixture\":true}";
    let headers = format!("POST /api/v1/chat HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(headers.as_bytes()).expect("mock 在收到请求头前关闭了连接");
    thread::sleep(Duration::from_millis(40));
    stream.write_all(body).expect("mock 在收到完整正文前关闭了连接");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("mock 未返回完整 HTTP 响应");
    assert!(response.starts_with("HTTP/1.1 200"), "mock 返回了错误状态");
    assert!(response.contains(REPLY));
    assert!(reply.started.load(Ordering::Acquire));
    assert_eq!(server.request().body, json!({"fixture":true}));
    server.no_request();
}
#[test]
fn settings_default_and_python_migration_never_serialize_credentials() {
    let defaults: CompanionSettings = serde_json::from_value(json!({})).unwrap();
    assert!(!defaults.enabled);
    assert!(!defaults.persist_history);
    let settings = migrate_settings(&json!({"companion_enabled":true,"companion_config":"local.json",
        "companion_auto":false,"companion_paused":true,"companion_cooldown_s":12,"companion_hourly_limit":2,"api_key":KEY,"prompt":PRIVATE}));
    assert!(settings.enabled && settings.paused && !settings.auto);
    assert_eq!(settings.config_path,"local.json");
    assert_eq!(settings.cooldown_seconds,12);
    let raw = serde_json::to_string(&settings).unwrap(); safe(&raw);
    assert!(!raw.contains("api_key")); assert!(!raw.contains("prompt"));
}
#[test]
fn disabled_constructor_does_not_open_configuration_or_send() {
    let server = Server::new();
    let fixture = Fixture::new();
    let mut settings = fixture.connection(&server.url, Protocol::AstrBot);
    settings.enabled = false;
    let runtime = Runtime::new(settings);
    assert_eq!(runtime.status().state, Health::Disabled);
    assert_eq!(runtime.request(Signal::Greeting), Err(CompanionError::Disabled));
    server.no_request();
    assert!(!runtime.status().configured);
}
#[test]
fn legacy_config_route_key_file_and_debug_are_safe() {
    let fixture = Fixture::new();
    fs::write(fixture.directory.join("key.txt"), format!("\u{feff}{KEY}\n")).unwrap();
    fixture.write(json!({"base_url":"https://fixture.invalid/prefix/api/v1/","key_file":"key.txt",
        "config_id":"route-id","config_name":"ignored-name","expected_model":"fixture-model","persona_label":"小亚"}));
    let connection = load_connection(&fixture.settings()).unwrap();
    assert_eq!(connection.protocol,Protocol::AstrBot);
    assert_eq!(connection.endpoint.as_str(),"https://fixture.invalid/prefix/api/v1/chat");
    assert_eq!(connection.key,KEY); assert_eq!(connection.route_field,"config_id");
    assert_eq!(connection.route_value,"route-id");
    safe(&format!("{connection:?}"));
    let before = fs::read(&fixture.path).unwrap();
    let selected = select_connection(&CompanionSettings::default(),&fixture.path).unwrap();
    assert!(!selected.enabled);
    assert_eq!(before,fs::read(&fixture.path).unwrap());
}
#[test]
fn config_rejects_unsafe_urls_and_ambiguous_credentials() {
    for url in ["http://example.com","http://192.168.1.2","http://0.0.0.0","http://127.1","http://2130706433",
        "https://user:pass@fixture.invalid","https://fixture.invalid?","https://fixture.invalid#",
        "https://fixture.invalid/\nprivate","https://fixture.invalid\\private","https://fixture.invalid:0",
        "http://localhost.","http://[::1%25eth0]","https:///missing"] {
        assert!(config::endpoint(url,Protocol::AstrBot).is_err(), "接受了不安全地址 {url}");
    }
    assert_eq!(config::endpoint("http://localhost:8080",Protocol::OpenAi).unwrap().as_str(),"http://127.0.0.1:8080/v1/chat/completions");
    assert_eq!(config::endpoint("http://[::1]:8080/v1",Protocol::Responses).unwrap().as_str(),"http://[::1]:8080/v1/responses");
    let fixture = Fixture::new();
    for value in [json!({"base_url":"https://fixture.invalid","key":KEY,"key_file":"key.txt","config_id":"id"}),
        json!({"base_url":"https://fixture.invalid","key":"bad\r\nsecret","config_id":"id"}),
        json!({"base_url":"https://fixture.invalid","key":KEY}),
        json!({"base_url":"https://fixture.invalid","key":"${PRIVATE}","config_id":"id"})] {
        fixture.write(value); assert!(load_connection(&fixture.settings()).is_err());
    }
}
#[test]
fn candidates_and_explicit_provider_model_selection_are_local() {
    let fixture = Fixture::new();
    fixture.write(json!({"providers":[
        {"id":"a","name":"A","protocol":"openai","base_url":"https://fixture.invalid","key":KEY,"models":["m1","m2"]},
        {"id":"b","name":"B","protocol":"responses","base_url":"https://fixture.invalid/v1","key":KEY,"models":["m3"]}]}));
    let rows = connection_candidates(&fixture.path).unwrap(); assert_eq!(rows.len(),3);
    let mut settings = fixture.settings();
    assert_eq!(load_connection(&settings).unwrap_err(),CompanionError::Selection);
    settings.provider_id="a".into(); settings.model_id="m2".into();
    assert_eq!(load_connection(&settings).unwrap().model,"m2");
    settings.model_id="not-listed".into(); assert_eq!(load_connection(&settings).unwrap_err(),CompanionError::Selection);
    safe(&serde_json::to_string(&rows).unwrap());
}
#[test]
fn explicit_zcode_provider_file_is_supported_without_copying_credentials() {
    let fixture = Fixture::new();
    fixture.write(json!({"config":{"providerConfigRules":{"providerRules":[{"providerId":"personal","providerName":"fixture",
        "config":{"api":{"type":"anthropic-messages","baseUrl":"https://fixture.invalid/v1"},"access":{"apiKey":KEY},"personalModelIds":["m1"]}}]}}}));
    let conn = load_connection(&fixture.settings()).unwrap();
    assert_eq!(conn.model,"m1"); assert_eq!(conn.protocol,Protocol::Anthropic);
    assert_eq!(conn.endpoint.as_str(),"https://fixture.invalid/v1/messages");
    safe(&serde_json::to_string(conn.info()).unwrap());
}
#[test]
fn all_four_protocols_send_expected_payload_and_only_return_text() {
    for protocol in [Protocol::AstrBot,Protocol::OpenAi,Protocol::Anthropic,Protocol::Responses] {
        let server=Server::new(); let fixture=Fixture::new();
        let body=match protocol {
            Protocol::AstrBot=>json!({"data":{"reply":REPLY,"reasoning":PRIVATE}}),
            Protocol::OpenAi=>json!({"choices":[{"message":{"role":"assistant","content":REPLY,"reasoning_content":PRIVATE}}]}),
            Protocol::Anthropic=>json!({"role":"assistant","content":[{"type":"thinking","thinking":PRIVATE},{"type":"text","text":REPLY}]}),
            Protocol::Responses=>json!({"status":"completed","output":[{"type":"reasoning","text":PRIVATE},{"type":"message","role":"assistant","content":[{"type":"output_text","text":REPLY}]}]}),
        };
        server.plan(200,serde_json::to_vec(&body).unwrap(),Vec::new(),false,65536,Duration::ZERO);
        let runtime=Runtime::new(fixture.connection(&server.url,protocol)); ready(&runtime);
        server.no_request(); // start/configure/本地健康不会请求服务。
        runtime.request(Signal::Greeting).unwrap();
        assert_eq!(event(&runtime,EventKind::Reply).message,REPLY);
        let sent=server.request();
        assert_eq!(sent.path,match protocol { Protocol::AstrBot=>"/api/v1/chat",Protocol::OpenAi=>"/v1/chat/completions",Protocol::Anthropic=>"/v1/messages",Protocol::Responses=>"/v1/responses" });
        assert_eq!(sent.headers.get(if protocol==Protocol::Anthropic {"x-api-key"} else {"authorization"}).unwrap(),
            &if protocol==Protocol::Anthropic {KEY.into()} else {format!("Bearer {KEY}")});
        if protocol==Protocol::Responses { assert_eq!(sent.body["store"],false); }
        if protocol==Protocol::AstrBot { assert_eq!(sent.body["config_id"],"fixture-route"); }
        else { assert_eq!(sent.body["model"],"fixture-model"); }
        safe(&serde_json::to_string(&runtime.history()).unwrap());
    }
}
#[test]
fn parser_filters_thinking_tools_user_echo_and_split_thought_tags() {
    let raw=format!("data: {{\"type\":\"plain\",\"chain_type\":\"reasoning\",\"data\":\"{PRIVATE}\"}}\r\n\r\n\
        data: {{\"type\":\"plain\",\"data\":\"<thi\"}}\r\n\r\n\
        data: {{\"type\":\"plain\",\"data\":\"nk>{PRIVATE}</think>{REPLY}\"}}\r\n\r\n\
        data: [DONE]\r\n\r\ndata: ignored-after-done");
    assert_eq!(parse_reply(raw.as_bytes(),"text/event-stream",Protocol::AstrBot).unwrap(),REPLY);
    for obj in [json!({"type":"tool_call","text":PRIVATE}),json!({"role":"user","text":PRIVATE}),
        json!({"reply":"<think>unfinished"}),json!({"reply":"<think"})] {
        assert_eq!(parse_reply(&serde_json::to_vec(&obj).unwrap(),"",Protocol::AstrBot),Err(CompanionError::Empty));
    }
    assert_eq!(parse_reply(b"data: broken\n\n","",Protocol::OpenAi),Err(CompanionError::Format));
}
#[test]
fn http_error_redirect_and_oversize_never_expose_server_body_or_retry() {
    let target=Server::new(); let server=Server::new(); let fixture=Fixture::new();
    let runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime); runtime.drain();
    for (status,expected) in [(401,CompanionError::Auth),(403,CompanionError::Auth),(429,CompanionError::RateLimit),
        (500,CompanionError::Server),(400,CompanionError::Request),(307,CompanionError::Redirect)] {
        server.plan(status,format!("{PRIVATE}{KEY}"),vec![("Location".into(),format!("{}/steal",target.url))],false,65536,Duration::ZERO);
        runtime.request(Signal::SaySomething).unwrap();
        let e=event(&runtime,EventKind::Error); assert!(e.message.contains(expected.category())); safe(&e.message);
        server.request(); server.no_request(); target.no_request();
    }
    server.plan(200,b"",vec![("Content-Length".into(),(http::MAX_RESPONSE_BYTES+1).to_string())],false,65536,Duration::ZERO);
    runtime.request(Signal::Greeting).unwrap(); assert!(event(&runtime,EventKind::Error).message.contains("size")); server.request();
}
#[test]
fn request_has_total_timeout_and_does_not_retry() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply(REPLY,true);
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot); settings.timeout_seconds=1;
    let runtime=Runtime::new(settings); ready(&runtime); runtime.drain();
    let start=Instant::now(); runtime.request(Signal::Greeting).unwrap(); old.started();
    assert!(event(&runtime,EventKind::Error).message.contains("timeout"));
    assert!(start.elapsed()<Duration::from_secs(3)); server.request(); server.no_request(); old.release();
}
#[test]
fn pause_cancels_active_and_queue_without_replaying_on_resume() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply(PRIVATE,true); server.reply(REPLY,false);
    let mut runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime);
    runtime.request(Signal::Message("old request".into())).unwrap(); old.started(); server.request();
    runtime.request(Signal::Message("queued must not send".into())).unwrap(); runtime.request(automatic(3)).unwrap();
    let start=Instant::now(); runtime.set_paused(true); assert!(start.elapsed()<Duration::from_millis(200));
    assert_eq!(runtime.request(Signal::Greeting),Err(CompanionError::Paused));
    server.no_request(); runtime.set_paused(false); server.no_request();
    runtime.request(Signal::Greeting).unwrap(); assert_eq!(event(&runtime,EventKind::Reply).message,REPLY); server.request();
    old.release(); wait(||old.finished.load(Ordering::Acquire)); server.no_request();
    safe(&serde_json::to_string(&runtime.drain()).unwrap()); assert_eq!(runtime.history().len(),1);
}
#[test]
fn cancel_clears_pending_requests_but_allows_new_manual_request() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply(PRIVATE,true); server.reply(REPLY,false);
    let runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime);
    runtime.request(Signal::Greeting).unwrap(); old.started(); server.request();
    runtime.request(Signal::Message("pending".into())).unwrap(); runtime.request(automatic(3)).unwrap();
    runtime.cancel(); server.no_request();
    runtime.request(Signal::SaySomething).unwrap(); assert_eq!(event(&runtime,EventKind::Reply).message,REPLY); server.request();
    old.release(); wait(||old.finished.load(Ordering::Acquire)); server.no_request();
    safe(&serde_json::to_string(&runtime.history()).unwrap());
}
#[test]
fn close_and_drop_do_not_dispatch_pending_or_publish_late_results() {
    for drop_runtime in [false,true] {
        let server=Server::new(); let fixture=Fixture::new(); let old=server.reply(PRIVATE,true);
        let runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime);
        runtime.request(Signal::Greeting).unwrap(); old.started(); server.request();
        runtime.request(Signal::SaySomething).unwrap(); runtime.request(automatic(3)).unwrap();
        let shared=runtime.shared.clone();
        let start=Instant::now();
        if drop_runtime { drop(runtime); } else { runtime.close(); assert_eq!(runtime.request(Signal::Greeting),Err(CompanionError::Closed)); drop(runtime); }
        assert!(start.elapsed()<Duration::from_millis(200));
        old.release(); wait(||old.finished.load(Ordering::Acquire)); server.no_request();
        wait(||Arc::strong_count(&shared)==1);
        let s=shared.lock(); assert_eq!(s.state,Health::Stopped); assert!(s.history.is_empty());
        assert!(s.events.iter().all(|e|e.kind!=EventKind::Reply));
    }
}
#[test]
fn discard_auto_interrupts_only_auto_and_preserves_manual_queue() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply(PRIVATE,true); server.reply(REPLY,false);
    let runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime);
    runtime.request(automatic(3)).unwrap(); old.started(); server.request();
    runtime.request(Signal::Message("manual survives".into())).unwrap(); runtime.request(automatic(4)).unwrap();
    runtime.discard_auto(); assert_eq!(event(&runtime,EventKind::Reply).message,REPLY);
    let sent=server.request(); assert!(sent.body["message"].as_str().unwrap().ends_with("manual survives"));
    old.release(); wait(||old.finished.load(Ordering::Acquire)); server.no_request();
    assert_eq!(runtime.history().len(),1); assert_eq!(runtime.history()[0].source,Source::Manual);
}
#[test]
fn manual_priority_auto_coalescing_and_queue_limit() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply("first",true); server.reply("手动一已回复",false); server.reply("手动二已回复",false); server.reply("latest",false);
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot); settings.max_pending=3;
    let runtime=Runtime::new(settings); ready(&runtime);
    runtime.request(Signal::Greeting).unwrap(); old.started(); server.request();
    runtime.request(automatic(3)).unwrap(); runtime.request(automatic(9)).unwrap();
    runtime.request(Signal::Message("manual-one".into())).unwrap(); runtime.request(Signal::Message("manual-two".into())).unwrap();
    old.release(); wait(||runtime.history().len()==4);
    let one=server.request(); let two=server.request(); let auto=server.request();
    assert!(one.body["message"].as_str().unwrap().ends_with("manual-one"));
    assert!(two.body["message"].as_str().unwrap().ends_with("manual-two"));
    assert!(auto.body["message"].as_str().unwrap().contains("\"tool_errors\":9")); server.no_request();
}
#[test]
fn auto_ttl_and_backoff_never_replay_failed_event() {
    let server=Server::new(); let fixture=Fixture::new();
    let old=server.reply(REPLY,true); server.reply("after backoff",false);
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot); settings.event_ttl_seconds=1;
    let runtime=Runtime::new(settings); ready(&runtime);
    runtime.request(Signal::Greeting).unwrap(); old.started(); server.request(); runtime.request(automatic(3)).unwrap();
    thread::sleep(Duration::from_millis(1100)); old.release(); event(&runtime,EventKind::Reply); server.no_request();
    // 退避是时间门控，不重发旧请求；只允许新的最新事件进入队列。
    { let mut s=runtime.shared.lock(); s.backoff_until=Some(Instant::now()+Duration::from_millis(450)); }
    runtime.request(automatic(3)).unwrap(); runtime.request(automatic(4)).unwrap();
    server.no_request(); assert_eq!(event(&runtime,EventKind::Reply).message,"after backoff"); server.request(); server.no_request();
}
#[test]
fn history_only_persists_replies_and_clear_updates_file() {
    let server=Server::new(); let fixture=Fixture::new();
    server.reply(REPLY,false);
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot);
    settings.persist_history=true; settings.history_path=fixture.directory.join("history.json").to_string_lossy().into_owned();
    let runtime=Runtime::new(settings.clone()); ready(&runtime);
    runtime.request(Signal::Message(PRIVATE.into())).unwrap(); event(&runtime,EventKind::Reply); server.request();
    wait(||fs::read_to_string(&settings.history_path).ok().is_some_and(|s|s.contains(REPLY)));
    let raw=fs::read_to_string(&settings.history_path).unwrap(); safe(&raw);
    assert!(!raw.contains("prompt")); assert!(!raw.contains("model"));
    runtime.clear_history(); wait(||read_history(Path::new(&settings.history_path)).ok().is_some_and(|items|items.is_empty()));
}
#[test]
fn history_path_cannot_overwrite_connection_or_unrelated_json() {
    let fixture=Fixture::new(); let mut settings=fixture.connection("https://fixture.invalid",Protocol::AstrBot);
    settings.persist_history=true; settings.history_path=settings.config_path.clone();
    assert_eq!(history_path(&settings),Err(CompanionError::History));
    let other=fixture.directory.join("settings.json"); fs::write(&other,b"{\"unrelated\":true}").unwrap();
    settings.history_path=other.to_string_lossy().into_owned(); assert_eq!(history_path(&settings),Err(CompanionError::History));
    assert_eq!(fs::read(&other).unwrap(),b"{\"unrelated\":true}");
}

#[test]
fn streaming_protocols_support_utf8_wire_chunks_and_text_channels() {
    let cases = [
        (Protocol::AstrBot, format!("data: {{\"type\":\"plain\",\"data\":\"{REPLY}\"}}\r\n\r\ndata: [DONE]\r\n\r\n")),
        (Protocol::OpenAi, format!("data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{PRIVATE}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"{REPLY}\"}}}}]}}\n\ndata: [DONE]\n\n")),
        (Protocol::Anthropic, format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"{PRIVATE}\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{REPLY}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n")),
        (Protocol::Responses, format!("event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"{REPLY}\"}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n")),
    ];
    for (protocol, body) in cases {
        let server=Server::new(); let fixture=Fixture::new();
        let mut settings=fixture.connection(&server.url,protocol); settings.stream=true;
        server.plan(200,body,vec![("Content-Type".into(),"text/event-stream".into())],false,1,Duration::ZERO);
        let runtime=Runtime::new(settings); ready(&runtime); runtime.request(Signal::Greeting).unwrap();
        assert_eq!(event(&runtime,EventKind::Reply).message,REPLY);
        let sent=server.request(); assert_eq!(sent.body[if protocol==Protocol::AstrBot {"enable_streaming"} else {"stream"}],true);
        safe(&serde_json::to_string(&runtime.history()).unwrap());
    }
}
#[test]
fn streamed_partial_never_exposes_split_key_thought_or_prompt() {
    let server=Server::new(); let fixture=Fixture::new();
    let mut settings=fixture.connection(&server.url,Protocol::OpenAi); settings.stream=true;
    let mut body=String::new();
    for piece in ["<thi", "nk>private-sentinel-never-display", "</think>哥哥，我在。", "fixture-only-key-", "never-live", "。", "private-manual-", "input"] {
        body.push_str(&format!("data: {}\n\n",json!({"choices":[{"delta":{"content":piece}}]})));
    }
    body.push_str("data: [DONE]\n\n");
    server.plan(200,body,vec![("Content-Type".into(),"text/event-stream".into())],false,8,Duration::from_millis(5));
    let runtime=Runtime::new(settings); ready(&runtime); runtime.drain();
    runtime.request(Signal::Message("private-manual-input".into())).unwrap();
    let mut partials=0; let mut final_text=None;
    wait(|| {
        for e in runtime.drain() {
            safe(&e.message);
            for forbidden in ["fixture-only-","private-manual-","<thi","private-sentinel"] { assert!(!e.message.contains(forbidden)); }
            if e.kind==EventKind::Partial { partials+=1; }
            if e.kind==EventKind::Reply { final_text=Some(e.message); }
        }
        final_text.is_some()
    });
    assert!(partials>0); assert_eq!(final_text.unwrap(),"哥哥，我在。[已隐藏]。"); server.request();
}
#[test]
fn malformed_content_and_slow_trickle_report_safe_errors() {
    let server=Server::new(); let fixture=Fixture::new();
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot); settings.timeout_seconds=1;
    let runtime=Runtime::new(settings); ready(&runtime); runtime.drain();
    for (body,headers,category) in [
        (PRIVATE.as_bytes().to_vec(),Vec::new(),CompanionError::Format),
        (b"{\"reply\":\"\"}".to_vec(),Vec::new(),CompanionError::Empty),
        (b"{\"reply\":\"ok\"}".to_vec(),vec![("Content-Length".into(),"500".into())],CompanionError::Format),
        (b"bad".to_vec(),vec![("Content-Encoding".into(),"gzip".into())],CompanionError::Format),
    ] {
        server.plan(200,body,headers,false,65536,Duration::ZERO);
        runtime.request(Signal::Greeting).unwrap();
        let e=event(&runtime,EventKind::Error); assert!(e.message.contains(category.category())); safe(&e.message); server.request();
    }
    server.plan(200,serde_json::to_vec(&json!({"reply":REPLY})).unwrap(),Vec::new(),false,1,Duration::from_millis(40));
    runtime.request(Signal::Greeting).unwrap(); error_event(&runtime, CompanionError::Timeout); server.request(); server.no_request();
}
#[test]
fn reload_switches_connection_and_invalidates_old_requests() {
    let first=Server::new(); let second=Server::new(); let fixture=Fixture::new();
    let old=first.reply(PRIVATE,true); second.reply(REPLY,false);
    let mut runtime=Runtime::new(fixture.connection(&first.url,Protocol::AstrBot)); ready(&runtime);
    runtime.request(Signal::Greeting).unwrap(); old.started(); first.request();
    runtime.request(Signal::SaySomething).unwrap();
    fixture.connection(&second.url,Protocol::AstrBot); runtime.reload(); ready(&runtime);
    second.no_request(); first.no_request();
    runtime.request(Signal::Greeting).unwrap(); assert_eq!(event(&runtime,EventKind::Reply).message,REPLY); second.request();
    old.release(); wait(||old.finished.load(Ordering::Acquire)); first.no_request();
    fixture.write(json!({})); runtime.reload(); wait(||runtime.status().state==Health::Error);
    assert!(!runtime.status().configured); assert!(runtime.request(Signal::Greeting).is_err()); second.no_request();
}
#[test]
fn manual_can_evict_auto_but_not_overfill_queue() {
    let server=Server::new(); let fixture=Fixture::new(); let old=server.reply(REPLY,true); server.reply("新回复",false);
    let mut settings=fixture.connection(&server.url,Protocol::AstrBot); settings.max_pending=1;
    let runtime=Runtime::new(settings); ready(&runtime); runtime.request(Signal::Greeting).unwrap(); old.started(); server.request();
    runtime.request(automatic(3)).unwrap(); runtime.request(Signal::SaySomething).unwrap();
    assert_eq!(runtime.request(Signal::Greeting),Err(CompanionError::Busy));
    assert_eq!(runtime.request(automatic(4)),Err(CompanionError::Busy));
    old.release(); wait(||runtime.history().len()==2); let next=server.request();
    assert!(!next.body["message"].as_str().unwrap().contains("\"tool_errors\"")); server.no_request();
}

#[test]
fn runtime_observe_deduplicates_terminal_and_resets_after_pause() {
    let server=Server::new(); let fixture=Fixture::new(); server.reply(REPLY,false); server.reply("下一轮回复",false);
    let mut runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot));
    let wait_ready = |runtime: &Runtime| wait(|| {
        let status = runtime.status();
        status.configured && status.state == Health::Ready
    });
    wait_ready(&runtime);
    // Runtime::request 使用真实 epoch 检查 TTL；本测试的观察时刻也必须来自
    // 同一个真实时钟，并和 TurnState 的毫秒精度一致。不能先推进到整秒+1，
    // 再切回真实 now，否则正确的回拨/旧事件保护会抑制随后终态。
    let clock_ms = || (epoch_seconds() * 1000.0).floor() / 1000.0;
    let now=clock_ms();
    let running=state("session-private","turn-private","running",now-10.0,0.0,0);
    let completed=state("session-private","turn-private","completed",now-10.0,now,0);
    assert!(!runtime.observe(&running,now-1.0)); assert!(runtime.observe(&completed,now));
    for _ in 0..20 { assert!(!runtime.observe(&completed,clock_ms())); }
    event(&runtime,EventKind::Reply); let sent=server.request();
    assert!(!sent.body.to_string().contains("session-private")); assert!(!sent.body.to_string().contains("turn-private")); server.no_request();
    runtime.set_paused(true);
    assert_eq!(runtime.status().state,Health::Paused);
    assert!(!runtime.observe(&completed,clock_ms()));
    runtime.set_paused(false);
    wait_ready(&runtime);
    let resumed_at=clock_ms();
    assert!(!runtime.observe(&completed,resumed_at)); server.no_request();
    let next_now=clock_ms();
    assert!(next_now>=resumed_at,"本地测试时钟回拨");
    let next=state("session-private","next","completed",next_now,next_now,0);
    let next_running=TurnState { status:"running".into(),completed_at:0,..next.clone() };
    assert!(!runtime.observe(&next_running,next_now));
    assert!(runtime.observe(&next,next_now),
        "恢复后未触发：health={:?},configured={},恢复观察时刻={:.6},新轮观察时刻={:.6},完成时刻={:.6}",
        runtime.status().state,runtime.status().configured,resumed_at,next_now,next.completed_at as f64/1000.0);
    event(&runtime,EventKind::Reply); server.request(); server.no_request();
}
#[test]
fn pending_auto_cancel_pause_disable_or_close_never_reaches_server() {
    for action in 0..4 {
        let server=Server::new(); let fixture=Fixture::new();
        let mut runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime);
        runtime.request(automatic(3)).unwrap();
        match action {
            0=>runtime.cancel(), 1=>runtime.set_paused(true),
            2=>{let mut s=runtime.settings();s.enabled=false;runtime.configure(s);},
            _=>runtime.close(),
        }
        server.no_request(); assert!(runtime.history().is_empty());
    }
}
#[test]
fn auto_failure_backoff_does_not_retry_and_manual_still_works() {
    let server=Server::new(); let fixture=Fixture::new();
    server.plan(429,PRIVATE,Vec::new(),false,65536,Duration::ZERO); server.reply(REPLY,false);
    let runtime=Runtime::new(fixture.connection(&server.url,Protocol::AstrBot)); ready(&runtime); runtime.drain();
    runtime.request(automatic(3)).unwrap(); error_event(&runtime, CompanionError::RateLimit); server.request();
    runtime.request(automatic(4)).unwrap(); runtime.discard_auto(); server.no_request();
    assert!(runtime.shared.lock().backoff_until.is_some());
    runtime.request(Signal::Greeting).unwrap(); assert_eq!(event(&runtime,EventKind::Reply).message,REPLY); server.request(); server.no_request();
}

const T: f64 = 1_700_000_000.0;
fn state(sid: &str, tid: &str, status: &str, start: f64, end: f64, errors: i64) -> TurnState {
    TurnState { available:true,is_sub:false,session_id:sid.into(),turn_id:tid.into(),status:status.into(),
        started_at:(start*1000.0) as i64,completed_at:if status=="running" {0} else {(end*1000.0) as i64},
        duration_ms:if status=="running" {0} else {((end-start)*1000.0) as i64},tool_errors:errors,cancelled_by_user:false }
}
fn watcher_settings() -> CompanionSettings { CompanionSettings { cooldown_seconds:0,hourly_limit:100,..Default::default() } }
#[test]
fn watch_initial_terminal_baseline_duplicates_and_cancelled_are_suppressed() {
    let settings=watcher_settings(); let mut watch=watch::TurnWatch::default();
    let completed=state("s","t","completed",T-10.0,T,0);
    assert!(watch.observe(&completed,T,&settings).is_none()); assert!(watch.observe(&completed,T+1.0,&settings).is_none());
    let next=state("s","next","completed",T+2.0,T+3.0,0);
    assert_eq!(watch.observe(&next,T+3.0,&settings).unwrap().kind,AutoKind::TurnFinished);
    assert!(watch.observe(&next,T+4000.0,&settings).is_none());
    let mut cancelled=state("s","cancelled","completed",T+4001.0,T+4002.0,5); cancelled.cancelled_by_user=true;
    assert!(watch.observe(&cancelled,T+4002.0,&settings).is_none());
}
#[test]
fn watch_success_long_error_and_legacy_running_thresholds() {
    let mut settings=watcher_settings(); let mut watch=watch::TurnWatch::default();
    watch.observe(&state("s","t","running",T-130.0,0.0,0),T-2.0,&settings);
    assert!(watch.observe(&state("s","t","running",T-130.0,0.0,3),T-1.0,&settings).is_none());
    assert_eq!(watch.observe(&state("s","t","error",T-130.0,T,3),T,&settings).unwrap().kind,AutoKind::ToolFailures);
    watch.reset(); settings.trigger_running_errors=true;
    watch.observe(&state("s","t","running",T-130.0,0.0,1),T-2.0,&settings);
    assert_eq!(watch.observe(&state("s","t","running",T-130.0,0.0,3),T-1.0,&settings).unwrap().kind,AutoKind::ToolFailures);
    assert!(watch.observe(&state("s","t","completed",T-130.0,T,3),T,&settings).is_none());
    watch.reset(); watch.observe(&state("s","t","running",T-130.0,0.0,0),T-1.0,&settings);
    assert_eq!(watch.observe(&state("s","t","completed",T-130.0,T,0),T,&settings).unwrap().kind,AutoKind::LongTurn);
}
#[test]
fn watch_cooldown_hourly_limit_and_session_switch_do_not_replay() {
    let mut settings=watcher_settings(); settings.cooldown_seconds=300; settings.hourly_limit=1;
    let mut watch=watch::TurnWatch::default();
    watch.observe(&state("s","t","running",T-130.0,0.0,0),T-1.0,&settings);
    assert!(watch.observe(&state("s","t","completed",T-130.0,T,0),T,&settings).is_some());
    watch.observe(&state("other","a","running",T+1.0,0.0,0),T+2.0,&settings);
    let blocked=state("other","a","completed",T+1.0,T+10.0,0);
    assert!(watch.observe(&blocked,T+10.0,&settings).is_none()); assert!(watch.observe(&blocked,T+3600.0,&settings).is_none());
    assert!(watch.observe(&state("other","b","completed",T+3590.0,T+3600.0,0),T+3600.0,&settings).is_some());
}
#[test]
fn watch_invalid_subagent_rollback_and_stale_rows_rebaseline() {
    let settings=watcher_settings();
    let running=state("s","t","running",T-130.0,0.0,0); let completed=state("s","t","completed",T-130.0,T,0);
    for invalid in [TurnState::default(),TurnState{is_sub:true,..completed.clone()},TurnState{status:"unknown".into(),..completed.clone()},
        TurnState{session_id:"sess_subagent_test".into(),..completed.clone()},TurnState{tool_errors:-1,..completed.clone()}] {
        let mut watch=watch::TurnWatch::default(); watch.observe(&running,T-1.0,&settings);
        assert!(watch.observe(&invalid,T,&settings).is_none()); assert!(watch.observe(&completed,T+1.0,&settings).is_none());
    }
    let mut watch=watch::TurnWatch::default(); watch.observe(&running,T,&settings);
    assert!(watch.observe(&completed,T-1.0,&settings).is_none()); assert!(watch.observe(&completed,T+1.0,&settings).is_none());
    watch.reset(); watch.observe(&running,T-1.0,&settings); assert!(watch.observe(&completed,T+121.0,&settings).is_none());
    assert!(watch.observe(&completed,T+122.0,&settings).is_none());
}
#[test]
fn automatic_prompt_allowlist_has_no_work_identifiers_or_content() {
    let signal=Signal::Auto(AutoSignal{kind:AutoKind::TurnFinished,occurred_at:T,duration_ms:1000,tool_errors:2});
    let prompt=http::prompt(&signal).unwrap(); let meta:Value=serde_json::from_str(prompt.split_once('\n').unwrap().1).unwrap();
    let keys:std::collections::HashSet<_>=meta.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys,["kind","occurred_at","duration_ms","tool_errors","time_of_day"].into_iter().collect());
    assert!(prompt.contains("1～3句")); assert!(prompt.contains("不得据此编造用户情绪或动作"));
    assert!(!prompt.contains("session_id")); assert!(!prompt.contains("turn_id")); safe(&prompt);
}
