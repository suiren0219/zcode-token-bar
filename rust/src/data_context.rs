//! 模型窗口只认当前数据目录的声明；未知时显式标记参考/推断，绝不冒充 128K 官方值。
use super::*;
use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashSet;

pub(super) fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default()
}
fn expand(path: impl AsRef<str>) -> PathBuf {
    let s=path.as_ref();
    if s=="~" {home()} else if let Some(rest)=s.strip_prefix("~/").or_else(||s.strip_prefix("~\\")){home().join(rest)}else{PathBuf::from(s)}
}
fn env_path(name:&str)->Option<PathBuf>{std::env::var(name).ok().filter(|s|!s.trim().is_empty()).map(|s|expand(s.trim()))}
fn unique(paths:Vec<PathBuf>)->Vec<PathBuf>{let mut seen=HashSet::new();paths.into_iter().filter(|p|seen.insert(p.clone())).collect()}
fn read(path:&Path)->Option<Value>{
    // 目录规则应远小于此上限，避免意外读入大型数据文件。
    if std::fs::metadata(path).ok()?.len()>16*1024*1024{return None;}
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
pub(super) fn data_homes()->Vec<PathBuf>{
    let mut bases=Vec::new();
    if let Some(p)=env_path("ZCODE_DATA_BASE_DIR"){bases.push(p.join(".zcode"));}
    if let Some(p)=env_path("ZCODE_HOME"){bases.push(p);}
    for key in ["ZCODE_CLI_HOME","CLI_HOME"]{
        if let Some(p)=env_path(key){
            if p.file_name().is_some_and(|v|v.to_string_lossy().eq_ignore_ascii_case("cli")) {if let Some(parent)=p.parent(){bases.push(parent.to_path_buf());}}
            bases.push(p);
        }
    }
    bases.push(home().join(".zcode"));
    if let Some(p)=env_path("APPDATA"){bases.push(p.join(".zcode"));}
    let bases=unique(bases);let mut active=Vec::new();
    for p in &bases {if let Some(v)=read(&p.join("v2/setting.json")){if let Some(root)=v["dataBaseDir"].as_str().filter(|r|!r.trim().is_empty()){active.push(expand(root).join(".zcode"));}}}
    active.extend(bases);unique(active)
}
fn dirs(path:&Path)->Vec<PathBuf>{
    std::fs::read_dir(path).ok().into_iter().flatten().filter_map(Result::ok).filter(|e|e.file_type().is_ok_and(|t|t.is_dir())).map(|e|e.path()).collect()
}
fn version(path:&Path)->Vec<u32>{
    path.components().rev().filter_map(|c|{
        let s=c.as_os_str().to_string_lossy();
        if !s.contains('.') {return None;}
        s.split('.').map(str::parse).collect::<Result<Vec<u32>,_>>().ok()
    }).next().unwrap_or_default()
}
fn runtime_candidates(base:&Path)->Vec<PathBuf>{
    let mut paths=Vec::new();
    for platform in dirs(&base.join("v2/runtime/provider")){
        for ver in dirs(&platform){for endpoint in dirs(&ver){
            if endpoint.file_name().is_some_and(|x|x.to_string_lossy().starts_with("endpoint-")){
                let p=endpoint.join("zcode-builtin.json");if p.is_file(){paths.push(p);}
            }
        }}
    }
    paths.sort_by(|a,b|version(b).cmp(&version(a)).then_with(||{
        let m=|p:&PathBuf|std::fs::metadata(p).ok().and_then(|v|v.modified().ok());m(b).cmp(&m(a))
    }).then_with(||a.cmp(b)));
    paths
}
#[cfg(windows)]
fn running_installs()->Vec<PathBuf>{
    // 与 Python EnumProcesses 相同的只读探测，避免把安装目录写死。
    #[link(name="kernel32")]
    unsafe extern "system" {
        fn K32EnumProcesses(ids:*mut u32,size:u32,needed:*mut u32)->i32;
        fn OpenProcess(access:u32,inherit:i32,pid:u32)->*mut std::ffi::c_void;
        fn QueryFullProcessImageNameW(process:*mut std::ffi::c_void,flags:u32,path:*mut u16,size:*mut u32)->i32;
        fn CloseHandle(handle:*mut std::ffi::c_void)->i32;
    }
    let mut out=Vec::new();let mut ids=[0u32;4096];let mut bytes=0;
    unsafe{
        if K32EnumProcesses(ids.as_mut_ptr(),std::mem::size_of_val(&ids) as u32,&mut bytes)==0{return out;}
        for pid in ids.iter().take((bytes as usize/4).min(ids.len())){
            let handle=OpenProcess(0x1000,0,*pid);if handle.is_null(){continue;}
            let mut text=[0u16;4096];let mut len=text.len() as u32;
            let ok=QueryFullProcessImageNameW(handle,0,text.as_mut_ptr(),&mut len);
            CloseHandle(handle);
            if ok==0{continue;}
            let path=PathBuf::from(String::from_utf16_lossy(&text[..len as usize]));
            if path.file_name().is_some_and(|n|n.to_string_lossy().eq_ignore_ascii_case("zcode.exe")){
                if let Some(dir)=path.parent(){out.push(dir.to_path_buf());if let Some(parent)=dir.parent(){out.push(parent.to_path_buf());}}
            }
        }
    }
    unique(out)
}
#[cfg(not(windows))]
fn running_installs()->Vec<PathBuf>{Vec::new()}
fn provider_ref(s:&str)->String{
    let s=s.to_lowercase();s.strip_prefix("builtin:").or_else(||s.strip_prefix("account:")).unwrap_or(&s).to_string()
}
fn generic(p:&str)->bool{matches!(p,".*"|".+"|"^.*$"|"^.+$"|"^.*"|"^.+"|".*$"|".+$"|".*?"|".+?"|"^.*?$"|"^.+?$")}
fn fullmatch(pattern:&str,value:&str)->bool{
    if pattern.is_empty()||value.is_empty(){return false;}
    RegexBuilder::new(&format!("\\A(?:{pattern})\\z")).case_insensitive(true).size_limit(2*1024*1024).build().is_ok_and(|r|r.is_match(value))
}
fn positive(v:&Value)->Option<i64>{v.as_i64().filter(|v|*v>0)}
fn window(v:&Value)->Option<i64>{
    positive(&v["properties"]["contextWindow"]).or_else(||positive(&v["limit"]["context"]))
        .or_else(||positive(&v["contextWindow"]))
}

#[derive(Default)]
struct Metadata { runtime:Value, personal:Value, config:Value, legacy:Value, runtime_source:String, personal_source:String, config_source:String, legacy_source:String }
fn load()->Metadata{
    let homes=data_homes();let mut m=Metadata::default();
    if let Some(p)=env_path("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE").or_else(||homes.iter().map(|b|b.join("v2/provider_config.json")).find(|p|p.is_file())){
        m.personal=read(&p).unwrap_or_default();m.personal_source=p.to_string_lossy().into_owned();
    }
    if let Some(p)=homes.iter().map(|b|b.join("v2/config.json")).find(|p|p.is_file()){
        m.config=read(&p).unwrap_or_default();m.config_source=p.to_string_lossy().into_owned();
    }
    let mut candidates=Vec::new();
    if let Some(p)=env_path("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE"){candidates.push(p);}
    // 显式 catalog 兼容 Python 配置，不读取或输出凭证。
    let mut bar_configs=Vec::new();
    if let Some(p)=env_path("ZCODE_TOKEN_BAR_DATA"){bar_configs.extend([p.join("config.json"),p.join("hoverbar.json")]);}
    if let Ok(exe)=std::env::current_exe(){if let Some(p)=exe.parent(){bar_configs.push(p.join("config.json"));}}
    for p in bar_configs{if let Some(c)=read(&p){if let Some(path)=c["catalog"].as_str().filter(|v|!v.trim().is_empty()){candidates.push(expand(path));}}}
    for base in &homes{candidates.extend(runtime_candidates(base));}
    let mut installs=running_installs();
    for env in ["ZCODE_PATH","ZCODE_HOME"]{if let Some(p)=env_path(env){let p=if p.is_file(){p.parent().unwrap_or(&p).to_path_buf()}else{p};installs.push(p.clone());if let Some(parent)=p.parent(){installs.push(parent.to_path_buf());}}}
    if let Ok(exe)=std::env::current_exe(){if let Some(p)=exe.parent(){installs.push(p.to_path_buf());}}
    for base in unique(installs){
        candidates.push(base.join("resources/config/provider/zcode-builtin.json"));
        for dir in [base.join("resources/model-providers"),base.join("model-providers")]{
            if let Ok(items)=std::fs::read_dir(dir){let mut files=items.filter_map(Result::ok).map(|x|x.path()).filter(|p|p.file_name().is_some_and(|n|{let n=n.to_string_lossy();n.starts_with("models_catalog")&&n.ends_with(".json")})).collect::<Vec<_>>();files.sort();candidates.extend(files);}
        }
    }
    for p in unique(candidates){
        let Some(v)=read(&p) else {continue;};
        if v["config"]["modelConfigRules"].is_object()&&m.runtime_source.is_empty(){m.runtime=v.clone();m.runtime_source=p.to_string_lossy().into_owned();}
        if v["providers"].is_array()&&m.legacy_source.is_empty(){m.legacy=v;m.legacy_source=p.to_string_lossy().into_owned();}
        if !m.runtime_source.is_empty()&&!m.legacy_source.is_empty(){break;}
    }
    m
}
fn provider_api(m:&Metadata,provider:&str)->(String,String){
    if let Some(rules)=m.personal["config"]["providerConfigRules"]["providerRules"].as_array(){
        for rule in rules.iter().rev(){if rule["providerId"].as_str().is_some_and(|v|provider_ref(v)==provider_ref(provider)){
            let api=&rule["config"]["api"];
            return (api["type"].as_str().unwrap_or("").into(),api["baseUrl"].as_str().unwrap_or("").into());
        }}
    }
    if let Some(providers)=m.config["provider"].as_object(){for (id,p) in providers{if provider_ref(id)==provider_ref(provider){
        let api=p["apiType"].as_str().or_else(||p["kind"].as_str()).unwrap_or("");
        let base=p["options"]["baseURL"].as_str().or_else(||p["options"]["baseUrl"].as_str()).unwrap_or("");
        return (api.into(),base.into());
    }}}
    (String::new(),String::new())
}
fn rule_matches(rule:&Value,provider:&str,model:&str,api:&str,base:&str,exact:bool)->bool{
    for (key,value) in [("providerId",provider),("modelId",model)]{
        if exact||rule.get(key).is_some(){
            let Some(want)=rule[key].as_str().filter(|v|!v.is_empty()) else{return false;};
            if key=="providerId"{if provider_ref(want)!=provider_ref(value){return false;}}
            else if !want.eq_ignore_ascii_case(value){return false;}
        }
    }
    for (key,value) in [("modelMatch",model),("apiTypeMatch",api),("baseUrlMatch",base)]{
        if let Some(p)=rule.get(key){let Some(p)=p.as_str() else{return false;};if !fullmatch(p,value){return false;}}
    }
    if !exact&&rule.get("modelId").is_none()&&generic(rule["modelMatch"].as_str().unwrap_or(".*")){
        let site=rule["baseUrlMatch"].as_str().is_some_and(|p|!generic(p));
        if !site&&rule.get("providerId").is_none(){return false;}
    }
    true
}
fn rule_window(doc:&Value,provider:&str,model:&str,api:&str,base:&str,personal:bool)->Option<i64>{
    let rules=&doc["config"]["modelConfigRules"];let mut hit=None;
    let groups=if personal{vec!["modelRules","modelApiRules","providerSiteRules","builtinProviderModelRules","providerModelRules","manualProviderModelRules"]}
        else{vec!["modelRules","modelApiRules","providerSiteRules","builtinProviderModelRules"]};
    for group in groups{if let Some(entries)=rules[group].as_array(){
        let mut specificity=0;
        for rule in entries{
            let exact=matches!(group,"builtinProviderModelRules"|"providerModelRules"|"manualProviderModelRules");
            if rule_matches(rule,provider,model,api,base,exact){if let Some(value)=window(&rule["config"]){
                // Python _rules_window_ex：运行期 modelRules 更具体的模型规则优先，同分后者覆盖。
                let score=rule["modelMatch"].as_str().map(str::len).unwrap_or(0);
                if !personal&&group=="modelRules"&&score<specificity{continue;}
                specificity=score;hit=Some(value);
            }}
        }
    }}
    hit
}
fn lookup(m:&Metadata,provider:&str,model:&str)->Option<(i64,String)>{
    if model.trim().is_empty(){return None;}
    let (api,base)=provider_api(m,provider);
    if let Some(v)=rule_window(&m.runtime,provider,model,&api,&base,false){return Some((v,format!("运行期规则 · {}",m.runtime_source)));}
    if let Some(v)=rule_window(&m.personal,provider,model,&api,&base,true){return Some((v,format!("个人模型配置 · {}",m.personal_source)));}
    if let Some(providers)=m.config["provider"].as_object(){
        for (id,p) in providers{
            if provider_ref(id)!=provider_ref(provider){continue;}
            if let Some(models)=p["models"].as_object(){for (id,v) in models{if id.eq_ignore_ascii_case(model){if let Some(cw)=window(v){return Some((cw,format!("ZCode 配置 · {}",m.config_source)));}}}}
        }
    }
    if let Some(providers)=m.legacy["providers"].as_array(){for p in providers{
        if let Some(id)=p["id"].as_str(){if !provider.is_empty()&&provider_ref(id)!=provider_ref(provider){continue;}}
        if let Some(models)=p["models"].as_array(){for v in models{if v["id"].as_str().is_some_and(|id|id.eq_ignore_ascii_case(model)){if let Some(cw)=window(v){return Some((cw,format!("模型目录 · {}",m.legacy_source)));}}}}
    }}
    None
}
struct CachedMetadata { at:Instant, env:String, metadata:Metadata, hits:HashMap<(String,String),Option<(i64,String)>> }
thread_local!{static METADATA:RefCell<Option<CachedMetadata>>=const {RefCell::new(None)};}
pub(super) struct ContextWindow{pub value:i64,pub known:bool,pub inferred:bool,pub source:String}
pub(super) fn resolve(provider:&str,model:&str,used:i64,default_window:i64)->ContextWindow{
    let env=["ZCODE_DATA_BASE_DIR","ZCODE_HOME","ZCODE_CLI_HOME","CLI_HOME","ZCODE_PATH","ZCODE_PERSONAL_PROVIDER_CONFIG_FILE","ZCODE_BUILTIN_PROVIDER_CONFIG_FILE","ZCODE_TOKEN_BAR_DATA"].iter().map(|k|std::env::var(k).unwrap_or_default()).collect::<Vec<_>>().join("\n");
    let hit=METADATA.with(|cell|{
        let mut cache=cell.borrow_mut();
        if cache.as_ref().is_none_or(|c|c.env!=env||c.at.elapsed()>Duration::from_secs(10)){
            *cache=Some(CachedMetadata{at:Instant::now(),env,metadata:load(),hits:HashMap::new()});
        }
        let cache=cache.as_mut().unwrap();let key=(provider.to_lowercase(),model.to_lowercase());
        if let Some(value)=cache.hits.get(&key){return value.clone();}
        let value=lookup(&cache.metadata,provider,model);cache.hits.insert(key,value.clone());value
    });
    from_known(hit,used,default_window)
}
fn from_known(known:Option<(i64,String)>,used:i64,default_window:i64)->ContextWindow{
    let (value,source)=known.clone().unwrap_or_default();
    if used>0&&(value==0||used>value){
        let inferred=[128_000,200_000,256_000,400_000,512_000,1_000_000,2_000_000].into_iter().find(|v|*v>=used)
            .unwrap_or_else(||used.saturating_add(999_999)/1_000_000*1_000_000);
        return ContextWindow{value:inferred,known:false,inferred:true,source:if source.is_empty(){"模型窗口未知；按最近成功输入推断，非官方容量".into()}else{format!("已观测输入超过声明 {value}；按用量推断 · {source}")}};
    }
    if known.is_some(){ContextWindow{value,known:true,inferred:false,source}}
    else{ContextWindow{value:default_window.max(0),known:false,inferred:false,source:if default_window>0{"模型窗口未知；使用设置参考值".into()}else{"模型窗口未知".into()}}}
}

#[cfg(test)]
mod tests{
    use super::*;
    use serde_json::json;
    #[test]fn runtime_personal_config_priority_and_provider_scope(){
        let mut m=Metadata{runtime:json!({"config":{"modelConfigRules":{"modelRules":[{"modelMatch":"^custom$","config":{"properties":{"contextWindow":256000}}},{"modelMatch":".*","config":{"properties":{"contextWindow":128000}}}]}}}),personal:json!({"config":{"modelConfigRules":{"providerModelRules":[{"providerId":"p","modelId":"custom","config":{"properties":{"contextWindow":400000}}}]}}}),config:json!({"provider":{"p":{"models":{"custom":{"limit":{"context":1000000}}}}}}),..Default::default()};
        assert_eq!(lookup(&m,"p","custom").unwrap().0,256000);
        m.runtime=Value::Null;assert_eq!(lookup(&m,"p","custom").unwrap().0,400000);
        m.personal=Value::Null;assert_eq!(lookup(&m,"p","custom").unwrap().0,1000000);
        assert!(lookup(&m,"other","custom").is_none());
    }
    #[test]fn unknown_and_inferred_windows_are_not_declared_known(){
        let s=from_known(None,0,0);assert_eq!(s.value,0);assert!(!s.known);
        let s=from_known(None,0,128000);assert!(!s.known);assert!(s.source.contains("参考"));
        let s=from_known(None,250000,128000);assert_eq!(s.value,256000);assert!(s.inferred);assert!(!s.known);
        let s=from_known(Some((200000,"rule".into())),100000,0);assert_eq!(s.value,200000);assert!(s.known);
    }
    #[test]fn specific_runtime_window_survives_later_broad_model_rule(){
        let doc=json!({"config":{"modelConfigRules":{"modelRules":[{"modelMatch":"^gpt-custom$","config":{"properties":{"contextWindow":400000}}},{"modelMatch":"gpt-.*","config":{"properties":{"contextWindow":128000}}}]}}});
        assert_eq!(rule_window(&doc,"p","gpt-custom","","",false),Some(400000));
    }
    #[test]fn provider_and_site_restrictions_are_respected(){
        let doc=json!({"config":{"modelConfigRules":{"providerSiteRules":[{"modelMatch":".*","baseUrlMatch":"https://example[.]org/.*","config":{"properties":{"contextWindow":200000}}}],"builtinProviderModelRules":[{"providerId":"builtin:p","modelId":"m","config":{"properties":{"contextWindow":400000}}}]}}});
        assert_eq!(rule_window(&doc,"account:p","m","","",false),Some(400000));
        assert_eq!(rule_window(&doc,"other","m","","https://example.org/v1",false),Some(200000));
        assert_eq!(rule_window(&doc,"other","m","","https://elsewhere.org/v1",false),None);
    }
    #[test]fn wildcard_api_rule_does_not_claim_capacity(){
        let doc=json!({"config":{"modelConfigRules":{"modelRules":[{"modelMatch":".*","config":{"properties":{"contextWindow":128000}}}],"modelApiRules":[{"apiTypeMatch":"openai.*","config":{"properties":{"contextWindow":128000}}}]}}});
        assert_eq!(rule_window(&doc,"p","unknown","openai-chat-completions","",false),None);
    }
}
