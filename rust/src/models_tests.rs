//! 仅临时配置和 127.0.0.1 mock 服务；绝不枚举真实配置或请求真实 provider。
use super::*;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{atomic::AtomicU64, Mutex};
use std::thread;

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const KEY: &str = "fixture-credential-not-real-29ba8d";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("zcode-model-fixture-{}-{}", std::process::id(), FIXTURE_ID.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&p).unwrap(); Self(p)
    }
    fn put(&self, path: &str, data: &Value) -> PathBuf {
        let p = self.0.join(path); fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, serde_json::to_vec_pretty(data).unwrap()).unwrap(); p
    }
    fn service(&self, base: &str, kind: &str) -> ModelsService {
        let personal = self.put("v2/provider_config.json", &personal(base, kind));
        let v2 = self.put("v2/config.json", &json!({"provider":{"account:acct":{"kind":"account","models":{}}},"unrelated":{"keep":[1,2]}}));
        ModelsService::new(ConfigPaths { personal_config: Some(personal), v2_config: Some(v2), catalog: None, backup_dir: self.0.join("backups") })
    }
}
impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
fn personal(base: &str, kind: &str) -> Value {
    json!({"schemaVersion":1,"other":{"keep":true},"config":{"providerOrder":["p"],
        "providerConfigRules":{"providerRules":[{"providerId":"p","providerName":"fixture","config":{
            "group":"standard-personal","access":{"type":"api-key","credential":KEY},
            "api":{"type":kind,"baseUrl":base},"personalModelIds":["existing"],"modelOrder":["existing"]}}]},
        "modelConfigRules":{"providerModelRules":[{"providerId":"p","modelId":"existing","config":{"enabled":false,"properties":{"contextWindow":777}}}]}}})
}
fn read(path: &Path) -> Value { serde_json::from_slice(&fs::read(path).unwrap()).unwrap() }
fn rows(ids: &[&str], pid: &str) -> Vec<RichModelRow> {
    ids.iter().map(|id| RichModelRow { row: ModelRow { provider: "fixture".into(), provider_id: pid.into(), id: (*id).into() }, ..Default::default() }).collect()
}
#[derive(Clone)]
struct Request { path: String, headers: HashMap<String, String>, body: Value }
#[derive(Clone)]
struct Response { status: u16, body: Vec<u8>, headers: Vec<(String,String)>, delay: Duration }
impl Response {
    fn json(status: u16, value: Value) -> Self { Self { status, body: serde_json::to_vec(&value).unwrap(), headers: Vec::new(), delay: Duration::ZERO } }
}
struct Server { base: String, calls: Arc<Mutex<Vec<Request>>>, stop: Arc<AtomicBool>, thread: Option<thread::JoinHandle<()>> }
impl Server {
    fn new(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap(); listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap()); let calls = Arc::new(Mutex::new(Vec::new())); let stop = Arc::new(AtomicBool::new(false));
        let captured = calls.clone(); let ending = stop.clone(); let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !ending.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => { let h = handler.clone(); let captured = captured.clone(); workers.push(thread::spawn(move || {
                        // Windows accept 继承监听 socket 的 nonblocking，读前必须恢复阻塞。
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        let Some(request) = read_request(&mut stream) else { return; };
                        captured.lock().unwrap().push(request.clone()); let response = h(request);
                        if !response.delay.is_zero() { thread::sleep(response.delay); }
                        let mut prefix = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", response.status, response.body.len());
                        for (name, value) in response.headers { prefix.push_str(&format!("{name}: {value}\r\n")); }
                        prefix.push_str("\r\n"); let _ = stream.write_all(prefix.as_bytes()); let _ = stream.write_all(&response.body);
                    })); },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
            for worker in workers { let _ = worker.join(); }
        });
        Self { base, calls, stop, thread: Some(thread) }
    }
    fn queue(responses: Vec<Response>) -> Self {
        let responses = Mutex::new(VecDeque::from(responses));
        Self::new(move |_| responses.lock().unwrap().pop_front().unwrap_or_else(|| Response::json(500, json!({"error":"unexpected request"}))))
    }
    fn requests(&self) -> Vec<Request> { self.calls.lock().unwrap().clone() }
}
impl Drop for Server { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); if let Some(h) = self.thread.take() { h.join().unwrap(); } } }
fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut bytes = Vec::new(); let mut b = [0; 4096]; let end;
    loop {
        let n = s.read(&mut b).ok()?; if n == 0 { return None; } bytes.extend_from_slice(&b[..n]);
        if bytes.len() > MAX_BODY { return None; }
        if let Some(pos) = bytes.windows(4).position(|s| s == b"\r\n\r\n") { end = pos + 4; break; }
    }
    let prefix = String::from_utf8_lossy(&bytes[..end]); let mut lines = prefix.lines(); let first = lines.next()?;
    let path = first.split_whitespace().nth(1)?.to_owned();
    let headers: HashMap<_,_> = lines.filter_map(|l| l.split_once(':').map(|(k,v)| (k.to_ascii_lowercase(),v.trim().to_owned()))).collect();
    let len = headers.get("content-length").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    while bytes.len() < end + len { let n = s.read(&mut b).ok()?; if n == 0 { return None; } bytes.extend_from_slice(&b[..n]); }
    let body = serde_json::from_slice(&bytes[end..end+len]).unwrap_or(Value::Null);
    Some(Request { path, headers, body })
}
fn protocol_response(request: &Request) -> Value {
    let body = &request.body;
    if request.path.ends_with("/responses") {
        if body.get("tool_choice").is_some() { return json!({"status":"completed","output":[{"type":"function_call","call_id":"c1","name":"zb_probe","arguments":"{}"}]}); }
        json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}],"usage":{"image_tokens":1,"pdf_tokens":1}})
    } else if request.path.ends_with("/messages") {
        if body.get("tool_choice").is_some() { return json!({"content":[{"type":"tool_use","id":"c1","name":"zb_probe","input":{}}]}); }
        json!({"content":[{"type":"text","text":"OK"}],"usage":{"image_tokens":1,"pdf_tokens":1}})
    } else {
        if body.get("tool_choice").is_some() { return json!({"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"zb_probe","arguments":"{}"}}]}}]}); }
        json!({"choices":[{"message":{"role":"assistant","content":"OK"}}],"usage":{"image_tokens":1,"pdf_tokens":1}})
    }
}
#[test]
fn paths_respect_active_settings_and_do_not_fall_back_after_corruption() {
    let t = Temp::new(); let current = t.put("actual/.zcode/v2/config.json", &json!({"provider":{}}));
    let old = t.put("old/v2/config.json", &json!({"provider":{"old":{"models":{}}}}));
    t.put("old/v2/setting.json", &json!({"dataBaseDir":t.0.join("actual")}));
    let env = HashMap::from([("ZCODE_HOME", t.0.join("old").to_string_lossy().to_string())]);
    let p = ConfigPaths::resolve(|k| env.get(k).cloned(), Vec::new()).unwrap();
    assert_eq!(p.v2_config, Some(current.clone())); assert_eq!(p.personal_config, Some(current.with_file_name("provider_config.json")));
    fs::write(&current,b"not-json-sensitive-value").unwrap();
    let error = ModelsService::new(p).providers().unwrap_err(); assert!(!error.contains("sensitive")); assert!(old.exists());
}
#[test]
fn paths_cover_cli_home_data_root_and_catalog_versions() {
    let t = Temp::new(); let config = t.put("data/.zcode/v2/config.json", &json!({"provider":{}}));
    t.put("old/v2/config.json", &json!({"provider":{}}));
    let newest = t.put("data/.zcode/v2/runtime/provider/windows-x86_64/3.18.2/endpoint-x/zcode-builtin.json", &json!({}));
    t.put("data/.zcode/v2/runtime/provider/windows-x86_64/3.9.0/endpoint-x/zcode-builtin.json", &json!({}));
    let env = HashMap::from([("ZCODE_DATA_BASE_DIR", t.0.join("data").to_string_lossy().to_string()),("ZCODE_HOME",t.0.join("old").to_string_lossy().to_string())]);
    let p = ConfigPaths::resolve(|k| env.get(k).cloned(),Vec::new()).unwrap(); assert_eq!(p.v2_config,Some(config)); assert_eq!(p.catalog,Some(newest));
    let p = ConfigPaths::resolve(|k| (k=="ZCODE_CLI_HOME").then(||t.0.join("old/cli").to_string_lossy().to_string()),Vec::new()).unwrap();
    assert_eq!(p.v2_config,Some(t.0.join("old/v2/config.json")));
}
#[test]
fn credentials_use_access_credential_and_explicit_headers_override() {
    let server = Server::queue(vec![Response::json(200,json!({"data":[{"id":"ok"}]}))]); let t=Temp::new(); let s=t.service(&server.base,"openai");
    let path=s.paths.personal_config.as_ref().unwrap(); let mut doc=read(path);
    let cfg=&mut doc["config"]["providerConfigRules"]["providerRules"][0]["config"];
    cfg["access"]["apiKey"]=json!("obsolete-key");
    cfg["api"]["headers"]=json!({"Authorization":"Bearer header-fixture","Host":"wrong.host","OpenAI-Project":"fixture-project"});
    fs::write(path,serde_json::to_vec(&doc).unwrap()).unwrap();
    let d=s.discover("p",&Operation::discovery()).unwrap(); assert_eq!(d.rows.len(),1);
    let request=&server.requests()[0]; assert_eq!(request.headers["authorization"],"Bearer header-fixture");
    assert_eq!(request.headers["openai-project"],"fixture-project"); assert_ne!(request.headers["host"],"wrong.host");
    assert!(!serde_json::to_string(&d).unwrap().contains(KEY));
}
#[test]
fn credential_shapes_and_bad_values_are_rejected_without_echo() {
    for v in [json!("a\nb"),json!("a\rb"),json!(["key"]),json!({"type":"oauth","value":"key"}),json!("${MISSING_MODEL_FIXTURE_21935}"),json!("{env:INVALID-NAME}")] {
        let err=config::credential(&v).unwrap_err(); assert!(!err.contains("21935"));
    }
    assert_eq!(config::credential(&json!({"type":"api-key","value":KEY})).unwrap(),KEY);
    assert_eq!(config::credential(&json!({"apiKey":KEY})).unwrap(),KEY);
    // 仅为专用测试变量，不读取真实凭据，也不与其它测试共用变量。
    let key="ZCODE_MODELS_TEST_83A_ENV"; std::env::set_var(key,KEY);
    for v in [json!(format!("{{env:{key}}}")),json!(format!("${{{key}}}")),json!({"type":"env","name":key})] { assert_eq!(config::credential(&v).unwrap(),KEY); }
    std::env::remove_var(key);
}
#[test]
fn endpoints_handle_http_prefixes_and_all_protocols() {
    let t=Temp::new();
    for (base,kind,list,probe) in [
        ("http://x", "openai", "http://x/v1/models", "http://x/v1/chat/completions"),
        ("http://[::1]:8765/custom", "openai-responses", "http://[::1]:8765/custom/models", "http://[::1]:8765/custom/responses"),
        ("http://x/anthropic", "anthropic", "http://x/anthropic/v1/models", "http://x/anthropic/v1/messages"),
        ("https://x/v1/messages", "anthropic-messages", "https://x/v1/models", "https://x/v1/messages")
    ] {
        let s=t.service(base,kind); let r=config::find_record(&s.paths,"p").unwrap(); let e=http::Endpoint::for_record(&r).unwrap();
        assert_eq!(e.list.as_str(),list); assert_eq!(e.probe.as_str(),probe);
    }
    for base in ["https://u:p@x/v1","https://x?key=secret","https://x/#f","file:///x","https://x:wrong","https://x:0","https://x/\nv1"] {
        let s=t.service(base,"openai"); let r=config::find_record(&s.paths,"p").unwrap(); assert!(http::Endpoint::for_record(&r).is_err());
    }
}
#[test]
fn discovery_paginates_deduplicates_and_sanitizes() {
    let server=Server::new(|r| if r.path.contains("after_id=") { Response::json(200,json!({"data":[{"id":"second"},{"id":"first"}],"has_more":false})) }
        else {Response::json(200,json!({"data":[{"id":"first"},{"id":"first"},{"id":KEY},{"id":"bad\nmodel"},{"id":"rich","context_length":123456,"supported_parameters":["tools"],"architecture":{"input_modalities":["text","image"]}}],"has_more":true,"last_id":"first"}))});
    let t=Temp::new(); let s=t.service(&format!("{}/anthropic",server.base),"anthropic");
    let d=s.discover("p",&Operation::discovery()).unwrap(); assert_eq!(d.rows.iter().map(|r|r.row.id.as_str()).collect::<Vec<_>>(),vec!["first","rich","second"]);
    assert!(!d.truncated); assert_eq!(d.rows[1].capabilities.context_window,Some(123456)); assert_eq!(d.rows[1].capabilities.input.audio,Some(false));
    assert_eq!(server.requests()[0].headers["x-api-key"],KEY); assert_eq!(server.requests()[0].headers["anthropic-version"],"2023-06-01"); assert!(server.requests()[1].path.contains("after_id=first"));
    assert!(!serde_json::to_string(&d).unwrap().contains(KEY)); assert_eq!(d.rows[0].capabilities.tools,None);
}
#[test]
fn discovery_bounds_body_count_and_cursor_loops() {
    let t=Temp::new();
    let many=Server::queue(vec![Response::json(200,json!({"data":(0..205).map(|i|json!({"id":format!("model-{i}")})).collect::<Vec<_>>()}))]);
    let s=t.service(&many.base,"openai"); let d=s.discover("p",&Operation::discovery()).unwrap(); assert_eq!(d.rows.len(),200); assert!(d.truncated);
    let huge=Server::queue(vec![Response{status:200,body:vec![b' ';MAX_BODY+1],headers:vec![],delay:Duration::ZERO}]);
    let s=t.service(&huge.base,"openai"); assert!(s.discover("p",&Operation::discovery()).unwrap_err().contains("2 MiB"));
    let looped=Server::new(|_|Response::json(200,json!({"data":["one"],"has_more":true,"last_id":"same"})));
    let s=t.service(&looped.base,"openai"); let d=s.discover("p",&Operation::discovery()).unwrap(); assert!(d.truncated); assert_eq!(looped.requests().len(),2);
}
#[test]
fn redirects_never_forward_credentials_and_errors_are_fixed() {
    let caught=Server::new(|_|Response::json(200,json!({"data":["caught"]})));
    let redirect=Server::queue(vec![Response{status:302,body:vec![],headers:vec![("Location".into(),format!("{}/models",caught.base))],delay:Duration::ZERO}]);
    let t=Temp::new(); let s=t.service(&redirect.base,"openai"); let e=s.discover("p",&Operation::discovery()).unwrap_err(); assert!(e.contains("重定向")); assert!(caught.requests().is_empty());
    for status in [401,403,429,500] {
        let server=Server::queue(vec![Response::json(status,json!({"error":{"message":KEY}}))]);
        let s=t.service(&server.base,"openai"); let e=s.discover("p",&Operation::discovery()).unwrap_err(); assert!(e.contains(&status.to_string())); assert!(!e.contains(KEY));
    }
}
#[test]
fn fallback_is_local_and_cancellation_never_falls_back() {
    let t=Temp::new();
    for response in [Response::json(404,json!({})),Response::json(405,json!({})),Response::json(200,json!({"data":[]}))] {
        let server=Server::queue(vec![response]); let s=t.service(&server.base,"openai");
        let d=s.discover("p",&Operation::discovery()).unwrap(); assert_eq!(d.rows[0].row.id,"existing"); assert!(d.source_note.contains("配置"));
    }
    let server=Server::queue(vec![Response::json(200,json!({"data":["x"]}))]); let s=t.service(&server.base,"openai"); let op=Operation::discovery(); op.cancel.cancel();
    assert!(s.discover("p",&op).is_err()); assert!(server.requests().is_empty());
}
#[test]
fn account_without_endpoint_uses_runtime_catalog_and_prefix_normalization() {
    let t=Temp::new(); let mut s=t.service("http://127.0.0.1:9","openai");
    s.paths.catalog=Some(t.put("catalog.json",&json!({"config":{"modelConfigRules":{"builtinProviderModelRules":[
        {"providerId":"builtin:ACCT","modelId":"from-catalog","config":{"properties":{"supportsToolCall":true}}}]}}})));
    let d=s.discover("account:acct",&Operation::discovery()).unwrap(); assert_eq!(d.rows[0].row.id,"from-catalog"); assert_eq!(d.rows[0].capabilities.tools,Some(true));
    let p=s.probe_model("account:acct","from-catalog",&Operation::probe()); assert_eq!(p.usable,None);
}
#[test]
fn cancellation_drops_inflight_request_and_stops_remaining_probe_requests() {
    let mut response=Response::json(200,json!({"data":["late"]})); response.delay=Duration::from_millis(350);
    let server=Server::queue(vec![response]); let t=Temp::new(); let s=t.service(&server.base,"openai"); let op=Operation::discovery(); let cancel=op.cancel.clone();
    let handle=thread::spawn(move||{thread::sleep(Duration::from_millis(70));cancel.cancel();}); let start=Instant::now();
    assert!(s.discover("p",&op).unwrap_err().contains("取消")); assert!(start.elapsed()<Duration::from_millis(260)); handle.join().unwrap(); assert_eq!(server.requests().len(),1);
    let server=Server::new(|r|{let mut res=Response::json(200,protocol_response(&r));res.delay=Duration::from_millis(300);res});
    let s=t.service(&server.base,"openai"); let before=fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap(); let op=Operation::new(Duration::from_millis(80));
    let r=s.probe_and_import("p",&rows(&["new"],"p"),&op).unwrap(); assert!(r.probe.timed_out);assert_eq!(r.import.result.added,0);assert_eq!(server.requests().len(),1);assert_eq!(before,fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap());
}
#[test]
fn metadata_matches_constraints_and_merges_false_without_guessing() {
    let rules=json!({"modelRules":[{"modelMatch":".*","config":{"properties":{"supportsToolCall":true}}},
        {"modelMatch":"model-alpha","config":{"properties":{"inputFormat":{"supportsText":true,"supportsImage":true},"contextWindow":32000}}},
        {"modelMatch":"model-.*","config":{"properties":{"inputFormat":{"supportsImage":false},"contextWindow":64000}}}],
        "modelApiRules":[{"modelMatch":"model-alpha","apiTypeMatch":"openai-chat-completions","config":{"properties":{"inputFormat":{"supportsPdf":true}}}}],
        "providerSiteRules":[{"modelMatch":".*","apiTypeMatch":"anthropic-messages","baseUrlMatch":"https://example\\.test/v1/?","config":{"properties":{"supportsNativeWebSearch":true}}}]});
    let c=merge_capabilities("p","MODEL-ALPHA","openai-chat-completions","https://other.test",&json!({}),&json!({}),&rules);
    assert_eq!(c.input.text,Some(true));assert_eq!(c.input.image,Some(false));assert_eq!(c.input.pdf,Some(true));assert_eq!(c.context_window,Some(64000));assert_eq!(c.tools,None);assert_eq!(c.web_search,None);
    let c=merge_capabilities("p","opaque","openai-chat-completions","",&json!({}),&json!({}),&rules);assert_eq!(c,Capabilities::default());
    let c=merge_capabilities("p","opaque","anthropic-messages","https://example.test/v1/",&json!({}),&json!({}),&rules);assert_eq!(c.web_search,Some(true));
    let c=merge_capabilities("p","model-alpha","openai-chat-completions","",&json!({"limit":{"context":88888}}),&json!({"capabilities":{"vision":true},"supports_tools":false}),&rules);
    assert_eq!(c.input.image,Some(true));assert_eq!(c.tools,Some(false));assert_eq!(c.sources["input.image"],"provider API");assert_eq!(c.sources["context_window"],"ZCode config");
    assert!(capability_lines(&c).contains(&"输入音频：未知".to_string()));
}
#[test]
fn protocol_probes_use_correct_endpoints_and_complete_tool_roundtrip() {
    let t=Temp::new();
    for (kind,suffix) in [("openai","/v1/chat/completions"),("anthropic","/v1/messages"),("openai-responses","/v1/responses")] {
        let server=Server::new(|r|Response::json(200,protocol_response(&r)));let s=t.service(&server.base,kind);
        let r=s.probe_model("p","model",&Operation::probe()); assert_eq!(r.usable,Some(true));assert_eq!(r.tools,Some(true));assert_eq!(r.image,Some(true));assert_eq!(r.pdf,Some(true));assert_eq!(r.requests,5);
        let calls=server.requests();assert_eq!(calls.len(),5);assert!(calls.iter().all(|r|r.path==suffix));
        if kind=="openai-responses" {assert_eq!(calls[0].body["store"],false);assert!(calls[0].body.get("messages").is_none());assert_eq!(calls[2].body["input"][2]["type"],"function_call_output");assert_eq!(calls[3].body["input"][0]["content"][1]["type"],"input_image");}
        else if kind=="anthropic" {assert_eq!(calls[2].body["messages"][2]["content"][0]["type"],"tool_result");assert_eq!(calls[4].body["messages"][0]["content"][1]["source"]["media_type"],"application/pdf");}
        else {assert_eq!(calls[2].body["messages"][2]["role"],"tool");assert_eq!(calls[3].body["messages"][0]["content"][1]["type"],"image_url");}
    }
}
#[test]
fn probe_unknown_is_not_false_and_failed_models_are_not_imported() {
    let server=Server::new(|r| {
        if r.body["model"]=="bad" {return Response::json(503,json!({"error":{"message":KEY}}));}
        Response::json(200,json!({"choices":[{"message":{"content":"OK"}}],"usage":{"image_tokens":0,"pdf_tokens":0}}))
    });
    let t=Temp::new();let s=t.service(&server.base,"openai");let report=s.probe_and_import("p",&rows(&["good","bad"],"p"),&Operation::probe()).unwrap();
    assert_eq!(report.probe.probed,2);assert_eq!(report.probe.rows.len(),1);assert_eq!(report.import.result.added,1);
    let good=&report.probe.outcomes[0];assert_eq!(good.tools,None);assert_eq!(good.image,None);assert_eq!(good.pdf,None);
    assert_eq!(report.probe.outcomes[1].usable,None);assert!(!serde_json::to_string(&report).unwrap().contains(KEY));
    assert!(s.verify_after(&report.import,Duration::ZERO,&Operation::discovery()).retained);
}
#[test]
fn explicit_unsupported_is_false_but_auth_and_rate_limit_are_unknown() {
    let t=Temp::new();
    for status in [401,403,429,500] {let server=Server::queue(vec![Response::json(status,json!({"error":{"message":"model not supported"}}))]);let s=t.service(&server.base,"openai");let r=s.probe_model("p","x",&Operation::probe());assert_eq!(r.usable,None);assert_eq!(server.requests().len(),1);}
    let server=Server::new(|r|{
        if r.body.get("tool_choice").is_some() {return Response::json(400,json!({"error":{"message":"tools unsupported"}}));}
        if r.body.to_string().contains("application/pdf") {return Response::json(400,json!({"error":{"message":"pdf unsupported"}}));}
        Response::json(200,protocol_response(&r))
    });let s=t.service(&server.base,"openai");let r=s.probe_model("p","x",&Operation::probe());assert_eq!(r.tools,Some(false));assert_eq!(r.pdf,Some(false));assert_eq!(r.image,Some(true));
}
#[test]
fn import_preserves_existing_models_and_uses_strict_personal_schema() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let p=s.paths.personal_config.as_ref().unwrap();let original=fs::read(p).unwrap();let account=fs::read(s.paths.v2_config.as_ref().unwrap()).unwrap();
    let mut add=rows(&["existing","new"],"p");add[1].capabilities=Capabilities{input:Modalities{image:Some(true),pdf:Some(false),..Default::default()},output:Modalities{text:Some(true),image:Some(true),..Default::default()},tools:Some(true),context_window:Some(200000),output_limit:Some(32768),..Default::default()};
    let r=s.import_rows(&[("p".into(),add)],&Operation::discovery());assert!(r.result.error.is_empty(),"{}",r.result.error);assert_eq!(r.result.added,1);assert_eq!(r.result.skipped,vec!["p / existing"]);assert_eq!(fs::read(&r.result.backup).unwrap(),original);assert_eq!(fs::read(s.paths.v2_config.as_ref().unwrap()).unwrap(),account);
    let data=read(p);let models=data["config"]["modelConfigRules"]["providerModelRules"].as_array().unwrap();assert_eq!(models[0]["config"]["enabled"],false);assert_eq!(models[0]["config"]["properties"]["contextWindow"],777);
    let added=&models[1]["config"];assert_eq!(added["optionSpecs"]["maxOutputTokens"]["max"],32768);assert!(added["properties"].get("maxOutputTokens").is_none());assert_eq!(added["properties"]["outputFormat"],json!({"supportsText":true}));assert_eq!(added["properties"]["inputFormat"],json!({"supportsImage":true,"supportsPdf":false}));
    let v=s.verify_after(&r,Duration::ZERO,&Operation::discovery());assert!(v.retained);assert_eq!(v.runtime_accepted,None);
}
#[test]
fn malformed_schema_and_backup_failure_never_modify_original() {
    let t=Temp::new();let mut s=t.service("http://127.0.0.1:9","openai");let p=s.paths.personal_config.as_ref().unwrap().clone();
    let bad_dir=t.put("not-a-directory",&json!({}));s.paths.backup_dir=bad_dir;let original=fs::read(&p).unwrap();
    let r=s.import_rows(&[("p".into(),rows(&["new"],"p"))],&Operation::discovery());assert_eq!(r.result.added,0);assert!(!r.result.error.is_empty());assert_eq!(fs::read(&p).unwrap(),original);
    s.paths.backup_dir=t.0.join("backups");let mut doc=read(&p);doc["config"]["modelConfigRules"]["providerModelRules"]=json!({"malformed":true});fs::write(&p,serde_json::to_vec(&doc).unwrap()).unwrap();let original=fs::read(&p).unwrap();
    let r=s.import_rows(&[("p".into(),rows(&["new"],"p"))],&Operation::discovery());assert_eq!(r.result.added,0);assert!(!r.result.error.is_empty());assert_eq!(fs::read(&p).unwrap(),original);
}
#[test]
fn concurrent_changes_are_detected_and_multi_file_partial_is_honest() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let p=s.paths.personal_config.as_ref().unwrap();
    let groups=vec![("p".into(),rows(&["new"],"p"))];
    let r=importing::import_impl(&s,&groups,&Operation::discovery(),&|phase,path|{if phase=="before_commit"{let mut d=read(path);d["concurrent"]=json!(true);fs::write(path,serde_json::to_vec(&d).unwrap()).unwrap();}});
    assert_eq!(r.result.added,0);assert!(!r.result.error.is_empty());assert_eq!(read(p)["concurrent"],true);
    let s=t.service("http://127.0.0.1:9","openai");let account=s.paths.v2_config.as_ref().unwrap();
    let groups=vec![("p".into(),rows(&["new"],"p")),("account:acct".into(),rows(&["new-account"],"account:acct"))];
    let r=importing::import_impl(&s,&groups,&Operation::discovery(),&|phase,path|{if phase=="before_commit"&&path==account{let mut d=read(path);d["concurrent"]=json!(true);fs::write(path,serde_json::to_vec(&d).unwrap()).unwrap();}});
    assert_eq!(r.result.added,1);assert!(r.partial);assert!(r.result.error.contains("部分"));assert_eq!(r.result.added_ids,vec![("p".into(),"new".into())]);assert_eq!(r.files.len(),2);assert!(r.files[0].written);assert!(!r.files[1].written);assert!(r.files.iter().all(|f|Path::new(&f.backup).is_file()));
}
#[test]
fn delayed_verification_detects_client_overwrite_and_checks_all_personal_parts() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let p=s.paths.personal_config.as_ref().unwrap();
    let r=s.import_rows(&[("p".into(),rows(&["new"],"p"))],&Operation::discovery());let mut doc=read(p);
    doc["config"]["providerConfigRules"]["providerRules"][0]["config"]["personalModelIds"]=json!(["existing"]);
    let path=p.clone();let handle=thread::spawn(move||{thread::sleep(Duration::from_millis(30));fs::write(&path,serde_json::to_vec(&doc).unwrap()).unwrap();});
    let v=s.verify_after(&r,Duration::from_millis(90),&Operation::discovery());handle.join().unwrap();assert!(!v.retained);assert_eq!(v.missing,vec!["p / new"]);assert_eq!(v.runtime_accepted,None);
}
#[test]
fn import_deduplicates_repeated_groups_and_preserves_unknown_modalities() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let mut row=rows(&["new"],"account:acct");row[0].capabilities.input.image=Some(true);
    let r=s.import_rows(&[("account:acct".into(),row.clone()),("account:acct".into(),row)],&Operation::discovery());assert_eq!(r.result.added,1);assert!(r.result.error.is_empty());
    assert_eq!(read(s.paths.v2_config.as_ref().unwrap())["provider"]["account:acct"]["models"]["new"],json!({}));
}
#[test]
fn all_provider_probe_is_one_batch_with_one_limit() {
    let server=Server::new(|r|Response::json(200,protocol_response(&r)));
    let t=Temp::new();let s=t.service(&server.base,"openai");
    let path=s.paths.personal_config.as_ref().unwrap();let mut doc=read(path);
    let mut second=doc["config"]["providerConfigRules"]["providerRules"][0].clone();
    second["providerId"]=json!("p2");second["providerName"]=json!("fixture");
    doc["config"]["providerConfigRules"]["providerRules"].as_array_mut().unwrap().push(second);
    fs::write(path,serde_json::to_vec(&doc).unwrap()).unwrap();
    let ids:Vec<_>=(0..8).map(|i|format!("new-{i}")).collect();let refs:Vec<_>=ids.iter().map(String::as_str).collect();
    let report=s.probe_all_and_import(&[("p".into(),rows(&refs,"p")),("p2".into(),rows(&refs,"p2"))],&Operation::probe()).unwrap();
    assert_eq!(report.probe.total,12);assert_eq!(report.probe.probed,12);assert!(report.probe.truncated);
    assert_eq!(report.import.result.added,12,"{}",report.import.result.error);
    assert_eq!(report.probe.outcomes.iter().filter(|r|r.provider_id=="p").count(),8);
    assert_eq!(report.probe.outcomes.iter().filter(|r|r.provider_id=="p2").count(),4);
    assert_eq!(server.requests().len(),60);assert!(s.verify_after(&report.import,Duration::ZERO,&Operation::discovery()).retained);
}
#[test]
fn all_provider_cancel_after_first_probe_commits_nothing() {
    let server=Server::new(|r|Response::json(200,protocol_response(&r)));let t=Temp::new();let s=t.service(&server.base,"openai");
    let path=s.paths.v2_config.as_ref().unwrap();let mut account=read(path);
    account["provider"]["other"]=json!({"kind":"openai","options":{"baseURL":server.base,"apiKey":KEY},"models":{}});
    fs::write(path,serde_json::to_vec(&account).unwrap()).unwrap();
    let before_p=fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap();let before_a=fs::read(path).unwrap();
    let op=Operation::probe();let token=op.cancel.clone();let op=op.with_progress(move|p|{if p.stage=="model"&&p.completed==1{token.cancel();}});
    let report=s.probe_all_and_import(&[("p".into(),rows(&["one"],"p")),("other".into(),rows(&["two"],"other"))],&op).unwrap();
    assert!(report.probe.cancelled);assert_eq!(report.probe.probed,1);assert_eq!(report.import.result.added,0);assert!(report.import.files.is_empty());
    assert_eq!(before_p,fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap());assert_eq!(before_a,fs::read(path).unwrap());assert_eq!(server.requests().len(),5);
}
#[test]
fn identity_changes_during_probe_prevent_entire_batch_import() {
    let server=Server::new(|r|Response::json(200,protocol_response(&r)));let t=Temp::new();let s=t.service(&server.base,"openai");
    let path=s.paths.personal_config.as_ref().unwrap().clone();let op=Operation::probe().with_progress(move|p|{
        if p.stage=="model" {let mut data=read(&path);data["config"]["providerConfigRules"]["providerRules"][0]["config"]["access"]["credential"]=json!("replacement-fixture");fs::write(&path,serde_json::to_vec(&data).unwrap()).unwrap();}
    });
    let result=s.probe_and_import("p",&rows(&["new"],"p"),&op).unwrap();assert_eq!(result.probe.rows.len(),1);
    assert_eq!(result.import.result.added,0);assert!(result.import.result.error.contains("发生变化"));assert!(result.import.files.is_empty());
    assert_eq!(read(s.paths.personal_config.as_ref().unwrap())["config"]["providerConfigRules"]["providerRules"][0]["config"]["personalModelIds"],json!(["existing"]));
}
#[test]
fn second_backup_failure_and_cancel_before_commit_leave_all_originals() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");
    let groups=vec![("p".into(),rows(&["new"],"p")),("account:acct".into(),rows(&["new"],"account:acct"))];
    let personal=s.paths.personal_config.as_ref().unwrap();let account=s.paths.v2_config.as_ref().unwrap();
    let before_p=fs::read(personal).unwrap();let before_a=fs::read(account).unwrap();
    let r=importing::import_impl(&s,&groups,&Operation::discovery(),&|phase,path|{
        if phase=="before_backup"&&path==account {fs::rename(&s.paths.backup_dir,t.0.join("first-backup-safe")).unwrap();fs::write(&s.paths.backup_dir,b"blocked").unwrap();}
    });
    assert_eq!(r.result.added,0);assert!(!r.partial);assert!(r.result.error.contains("备份"));assert_eq!(before_p,fs::read(personal).unwrap());assert_eq!(before_a,fs::read(account).unwrap());
    fs::remove_file(&s.paths.backup_dir).unwrap();let op=Operation::discovery();let cancel=op.cancel.clone();
    let r=importing::import_impl(&s,&groups,&op,&|phase,_|{if phase=="prepared"{cancel.cancel();}});
    assert_eq!(r.result.added,0);assert!(r.result.error.contains("取消"));assert_eq!(before_p,fs::read(personal).unwrap());assert_eq!(before_a,fs::read(account).unwrap());
}
#[test]
fn cancellation_between_files_reports_only_real_commits() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let op=Operation::discovery();let cancel=op.cancel.clone();
    let account=s.paths.v2_config.as_ref().unwrap();let before=fs::read(account).unwrap();
    let groups=vec![("p".into(),rows(&["new"],"p")),("account:acct".into(),rows(&["new"],"account:acct"))];
    let r=importing::import_impl(&s,&groups,&op,&|phase,_|{if phase=="after_commit"{cancel.cancel();}});
    assert!(r.partial);assert_eq!(r.result.added,1);assert_eq!(r.result.added_ids,vec![("p".into(),"new".into())]);assert_eq!(before,fs::read(account).unwrap());assert!(r.result.error.contains("取消"));
}
#[test]
fn repeated_provider_groups_append_all_models_without_losing_earlier_groups() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");
    let r=s.import_rows(&[("p".into(),rows(&["first"],"p")),("p".into(),rows(&["second"],"p")),
        ("account:acct".into(),rows(&["first"],"account:acct")),("account:acct".into(),rows(&["second"],"account:acct"))],&Operation::discovery());
    assert_eq!(r.result.added,4,"{}",r.result.error);assert!(s.verify_after(&r,Duration::ZERO,&Operation::discovery()).retained);
    let again=s.import_rows(&[("p".into(),rows(&["third"],"p"))],&Operation::discovery());assert_eq!(again.result.added,1,"{}",again.result.error);
}
#[test]
fn duplicate_json_fields_are_rejected_without_losing_config() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let p=s.paths.personal_config.as_ref().unwrap();
    let duplicate=br#"{"schemaVersion":1,"config":{},"config":{"secret-fixture":"keep"}}"#;
    fs::write(p,duplicate).unwrap();
    let r=s.import_rows(&[("p".into(),rows(&["new"],"p"))],&Operation::discovery());
    assert_eq!(r.result.added,0);assert!(r.result.error.contains("JSON"));assert!(!r.result.error.contains("secret-fixture"));assert_eq!(fs::read(p).unwrap(),duplicate);
}
#[test]
fn empty_text_and_repeated_tool_calls_do_not_confirm_capability() {
    let t=Temp::new();let empty=Server::queue(vec![Response::json(200,json!({"choices":[{"message":{"content":""}}]}))]);
    let s=t.service(&empty.base,"openai");let r=s.probe_model("p","new",&Operation::probe());assert_eq!(r.usable,None);assert_eq!(empty.requests().len(),1);
    let repeat=Server::new(|request| {
        if request.body.get("tools").is_some() {return Response::json(200,json!({"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"again","type":"function","function":{"name":"zb_probe","arguments":"{}"}}]}}]}));}
        Response::json(200,protocol_response(&request))
    });
    let s=t.service(&repeat.base,"openai");let r=s.probe_model("p","new",&Operation::probe());assert_eq!(r.usable,Some(true));assert_eq!(r.tools,None);assert!(r.errors["tools"].contains("最终文本"));
}
#[test]
fn changed_environment_credential_prevents_import_after_probe() {
    let server=Server::new(|r|Response::json(200,protocol_response(&r)));let t=Temp::new();let s=t.service(&server.base,"openai");
    let key="ZCODE_MODELS_TEST_IDENTITY_619";std::env::set_var(key,KEY);
    let path=s.paths.personal_config.as_ref().unwrap();let mut doc=read(path);
    doc["config"]["providerConfigRules"]["providerRules"][0]["config"]["access"]["credential"]=json!(format!("{{env:{key}}}"));
    fs::write(path,serde_json::to_vec(&doc).unwrap()).unwrap();
    let op=Operation::probe().with_progress(move|p|{if p.stage=="model"{std::env::set_var(key,"new-env-fixture");}});
    let r=s.probe_and_import("p",&rows(&["new"],"p"),&op).unwrap();std::env::remove_var(key);
    assert_eq!(r.probe.probed,1);assert_eq!(r.import.result.added,0);assert!(r.import.result.error.contains("认证发生变化"));
    assert!(server.requests().iter().all(|r|r.headers["authorization"]==format!("Bearer {KEY}")));
}
#[test]
fn failed_or_cancelled_preparation_does_not_count_or_write() {
    let t=Temp::new();let s=t.service("http://127.0.0.1:9","openai");let before=fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap();
    let op=Operation::discovery();op.cancel.cancel();let r=s.import_rows(&[("p".into(),rows(&["new"],"p"))],&op);assert_eq!(r.result.added,0);assert_eq!(before,fs::read(s.paths.personal_config.as_ref().unwrap()).unwrap());
    let wrong=s.import_rows(&[("p".into(),rows(&["new"],"another"))],&Operation::discovery());assert!(!wrong.result.error.is_empty());assert_eq!(wrong.result.added,0);
}
