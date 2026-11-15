use super::*;
use serde_json::json;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{atomic::AtomicU64, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SERIAL: AtomicU64 = AtomicU64::new(0);
static IMPORT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
#[derive(Clone, Debug, Default, Serialize)]
pub struct FileImport {
    pub path: String, pub backup: String, pub written: bool,
    pub added_ids: Vec<(String, String)>, pub error: String,
    #[serde(skip)] personal: bool,
    #[serde(skip)] expected: Vec<(String, String, Value)>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    pub result: ImportResult, pub files: Vec<FileImport>, pub partial: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Verification {
    /// 仅磁盘持久性。没有客户端 IPC 确认，绝不声称 runtime 已采用新模型。
    pub retained: bool, pub runtime_accepted: Option<bool>, pub missing: Vec<String>,
    pub error: String, pub cancelled: bool,
}
struct Plan { doc: config::Document, next: Value, report: FileImport, staged: Option<TempFile> }
struct TempFile(PathBuf);
impl Drop for TempFile { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }
fn suffix() -> String {
    format!("{}-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), SERIAL.fetch_add(1, Ordering::Relaxed))
}
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new(); opts.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    opts.open(path)
}
struct SideLock { file: Option<File>, path: PathBuf }
impl SideLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let name = path.file_name().ok_or("配置路径缺少文件名。")?.to_string_lossy();
        let target = path.with_file_name(format!(".{name}.models.lock"));
        let file = create_private(&target).map_err(|_| "另一导入操作正在写入配置，或无法创建导入锁；未修改配置。".to_string())?;
        Ok(Self { file: Some(file), path: target })
    }
}
impl Drop for SideLock {
    fn drop(&mut self) {
        // Windows 删除前必须先释放句柄，否则下一次导入会被遗留锁文件挡住。
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}
fn protect_path(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|_| "配置文件已移除或不可访问。".to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() { return Err("配置目标必须是普通文件，未跟随链接写入。".into()); }
    #[cfg(windows)] { use std::os::windows::fs::MetadataExt; if meta.file_attributes() & 0x400 != 0 { return Err("配置文件是重解析点，未写入。".into()); } }
    Ok(())
}
fn snapshot_matches(doc: &config::Document) -> bool {
    protect_path(&doc.path).is_ok() && fs::read(&doc.path).is_ok_and(|bytes| bytes == doc.raw)
}
fn unique_array(value: Option<&Value>, label: &str) -> Result<Vec<Value>, String> {
    let Some(value) = value else { return Ok(Vec::new()); };
    let arr = value.as_array().ok_or_else(|| format!("{label} 必须是数组，未改写原配置。"))?;
    let mut seen = HashSet::new();
    if arr.iter().any(|v| !v.as_str().is_some_and(|s| config::valid_id(s, &[]) && seen.insert(s))) {
        return Err(format!("{label} 包含无效或重复 ID，未改写原配置。"));
    }
    Ok(arr.clone())
}
fn validate_model_rules(data: &Value) -> Result<Vec<Value>, String> {
    let Some(mcr) = data.pointer("/config/modelConfigRules") else { return Ok(Vec::new()); };
    config::require_object(mcr, "modelConfigRules 必须是对象。")?;
    let Some(pmr) = mcr.get("providerModelRules") else { return Ok(Vec::new()); };
    let arr = pmr.as_array().ok_or("providerModelRules 必须是数组，未丢弃原配置。")?;
    let mut seen = HashSet::new();
    for entry in arr {
        let pid = string(&entry["providerId"]); let mid = string(&entry["modelId"]);
        if !config::valid_id(pid, &[]) || !config::valid_id(mid, &[]) || !seen.insert((pid, mid)) { return Err("providerModelRules 包含无效或重复模型条目。".into()); }
        config::require_object(&entry["config"], "模型规则的 config 必须是对象。")?;
        if let Some(enabled) = entry["config"].get("enabled") { if !enabled.is_boolean() && !enabled.is_null() { return Err("模型 enabled 必须是布尔值。".into()); } }
        if let Some(props) = entry["config"].get("properties").filter(|v| !v.is_null()) {
            let props = config::require_object(props, "模型 properties 必须是对象。")?;
            for (key, value) in props {
                if !matches!(key.as_str(), "requiresMfjsToolSchema" | "contextWindow" | "inputFormat" | "outputFormat" | "supportsToolCall" | "supportsJsonSchemaOutput" | "supportsNativeWebSearch" | "supportsMidConversationSystem") {
                    return Err("现有 properties 含客户端严格 schema 不支持的字段，未写入。".into());
                }
                if value.is_null() { continue; }
                match key.as_str() {
                    "contextWindow" if positive_int(value).is_none() => return Err("contextWindow 必须是有效正整数。".into()),
                    "inputFormat" | "outputFormat" => {
                        for (name, flag) in config::require_object(value, "模型格式声明必须是对象。")? {
                            let allowed = name == "supportsText" || key == "inputFormat" && matches!(name.as_str(), "supportsImage" | "supportsVideo" | "supportsAudio" | "supportsPdf");
                            if !allowed || !flag.is_boolean() && !flag.is_null() { return Err("输入/输出格式不符合客户端严格 schema。".into()); }
                        }
                    }
                    "contextWindow" => {},
                    _ if !value.is_boolean() => return Err("模型能力字段必须是布尔值。".into()),
                    _ => {},
                }
            }
        }
        if let Some(specs) = entry["config"].get("optionSpecs").filter(|v| !v.is_null()) {
            config::require_object(specs, "optionSpecs 必须是对象。")?;
            if let Some(output) = specs.get("maxOutputTokens").filter(|v| !v.is_null()) {
                config::require_object(output, "maxOutputTokens 选项必须是对象。")?;
                for key in ["min", "max", "default"] {
                    if let Some(v) = output.get(key).filter(|v| !v.is_null()) { if positive_int(v).is_none() { return Err("最大输出选项必须是有效正整数。".into()); } }
                }
            }
        }
    }
    Ok(arr.clone())
}
fn personal_plan(doc: config::Document, groups: &[(String, Vec<RichModelRow>)], result: &mut ImportResult) -> Result<Plan, String> {
    let rules = config::personal_rules(&doc.value)?; let mut pmr = validate_model_rules(&doc.value)?;
    let mut next = doc.value.clone(); let mut report = FileImport { path: doc.path.to_string_lossy().into(), personal: true, ..Default::default() };
    for (pid, rows) in groups {
        let Some(idx) = rules.iter().position(|r| string(&r["providerId"]) == pid) else { result.missing.push(pid.clone()); continue; };
        let cfg = &rules[idx]["config"];
        let mut ids = unique_array(cfg.get("personalModelIds"), "personalModelIds")?;
        let mut order = unique_array(cfg.get("modelOrder"), "modelOrder")?;
        for row in rows {
            let mid = &row.row.id;
            if ids.iter().any(|v| v.as_str() == Some(mid)) || pmr.iter().any(|r| string(&r["providerId"]) == pid && string(&r["modelId"]) == mid) {
                result.skipped.push(format!("{pid} / {mid}")); continue;
            }
            let entry = metadata::personal_entry(&row.capabilities);
            ids.push(json!(mid)); if !order.iter().any(|v| v.as_str() == Some(mid)) { order.push(json!(mid)); }
            pmr.push(json!({"providerId":pid,"modelId":mid,"config":entry}));
            report.added_ids.push((pid.clone(), mid.clone())); report.expected.push((pid.clone(), mid.clone(), entry));
        }
        next["config"]["providerConfigRules"]["providerRules"][idx]["config"]["personalModelIds"] = json!(ids);
        next["config"]["providerConfigRules"]["providerRules"][idx]["config"]["modelOrder"] = json!(order);
    }
    if !report.added_ids.is_empty() {
        let cfg = next.get_mut("config").and_then(Value::as_object_mut).ok_or("config 必须是对象。")?;
        let mcr = cfg.entry("modelConfigRules").or_insert_with(|| json!({}));
        let mcr = mcr.as_object_mut().ok_or("modelConfigRules 必须是对象。")?;
        mcr.insert("providerModelRules".into(), json!(pmr));
        validate_model_rules(&next)?;
    }
    Ok(Plan { doc, next, report, staged: None })
}
fn account_plan(doc: config::Document, groups: &[(String, Vec<RichModelRow>)], result: &mut ImportResult) -> Result<Plan, String> {
    let providers = config::require_object(&doc.value["provider"], "provider 必须是对象，未丢弃原配置。")?;
    let mut next = doc.value.clone(); let mut report = FileImport { path: doc.path.to_string_lossy().into(), ..Default::default() };
    for (pid, rows) in groups {
        let Some(provider) = providers.get(pid) else { result.missing.push(pid.clone()); continue; };
        config::require_object(provider, "provider 条目必须是对象。")?;
        if provider.get("enabled") == Some(&Value::Bool(false)) { result.missing.push(pid.clone()); continue; }
        let mut models = match provider.get("models") { Some(m) => config::require_object(m, "provider.models 必须是对象，未丢弃原配置。")?.clone(), None => serde_json::Map::new() };
        for (_, existing) in &models { config::require_object(existing, "已有模型条目必须是对象。")?; }
        for row in rows {
            let mid = &row.row.id;
            if models.contains_key(mid) { result.skipped.push(format!("{pid} / {mid}")); continue; }
            let entry = metadata::account_entry(&row.capabilities);
            models.insert(mid.clone(), entry.clone()); report.added_ids.push((pid.clone(), mid.clone()));
            report.expected.push((pid.clone(), mid.clone(), entry));
        }
        next["provider"][pid]["models"] = Value::Object(models);
    }
    Ok(Plan { doc, next, report, staged: None })
}
fn backup(plan: &mut Plan, dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|_| "配置备份目录创建失败，尚未修改配置。".to_string())?;
    let name = plan.doc.path.file_name().ok_or("配置路径无效。")?.to_string_lossy();
    let path = dir.join(format!("{name}.bak-{}", suffix()));
    let mut file = create_private(&path).map_err(|_| "配置备份创建失败，尚未修改配置。".to_string())?;
    if file.write_all(&plan.doc.raw).and_then(|_| file.sync_all()).is_err() {
        drop(file); let _ = fs::remove_file(&path); return Err("配置备份写入失败，尚未修改配置。".into());
    }
    drop(file);
    if fs::read(&path).ok().as_deref() != Some(plan.doc.raw.as_slice()) { return Err("配置备份读回不一致，尚未修改配置。".into()); }
    plan.report.backup = path.to_string_lossy().into(); Ok(())
}
fn stage(plan: &mut Plan) -> Result<(), String> {
    let name = plan.doc.path.file_name().ok_or("配置路径无效。")?.to_string_lossy();
    let path = plan.doc.path.with_file_name(format!(".{name}.models-{}.tmp", suffix()));
    let mut file = create_private(&path).map_err(|_| "无法创建配置暂存文件。".to_string())?;
    let temp = TempFile(path);
    let bytes = serde_json::to_vec_pretty(&plan.next).map_err(|_| "无法编码模型配置。".to_string())?;
    file.write_all(&bytes).and_then(|_| file.write_all(b"\n")).and_then(|_| file.sync_all()).map_err(|_| "配置暂存文件写入失败。".to_string())?;
    drop(file);
    if config::read_optional(&temp.0)?.is_none_or(|d| d.value != plan.next) { return Err("配置暂存读回校验失败。".into()); }
    // Unix 使用原权限；Windows 新文件继承相同目录 ACL，替换使用 ReplaceFileW 保留原文件 ACL。
    #[cfg(unix)] { let permissions = fs::metadata(&plan.doc.path).map_err(|_| "无法读取配置权限。".to_string())?.permissions(); fs::set_permissions(&temp.0, permissions).map_err(|_| "无法保留配置权限。".to_string())?; }
    plan.staged = Some(temp); Ok(())
}
#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")] extern "system" {
        fn ReplaceFileW(replaced: *const u16, replacement: *const u16, backup: *const u16, flags: u32,
            exclude: *mut std::ffi::c_void, reserved: *mut std::ffi::c_void) -> i32;
    }
    let src: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { ReplaceFileW(dst.as_ptr(), src.as_ptr(), std::ptr::null(), 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}
#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> { fs::rename(from, to) }

pub(super) fn import(service: &ModelsService, groups: &[(String, Vec<RichModelRow>)], op: &Operation) -> ImportReport {
    import_impl(service, groups, op, &|_, _| {})
}
pub(super) fn import_checked(service: &ModelsService, groups: &[(String, Vec<RichModelRow>)], op: &Operation,
    identities: &[(String, Value)]) -> ImportReport {
    import_with_checks(service, groups, op, identities, &|_, _| {})
}
pub(super) fn import_impl(service: &ModelsService, groups: &[(String, Vec<RichModelRow>)], op: &Operation,
    hook: &dyn Fn(&str, &Path)) -> ImportReport {
    import_with_checks(service, groups, op, &[], hook)
}
fn transport_identity(identity: &Value) -> Value {
    let mut value = identity.clone();
    // 首次提交会合法追加这些字段；其余字段（包括已解析环境认证）仍必须逐文件一致。
    for section in ["config", "fallback"] {
        if let Some(config) = value.get_mut(section).and_then(Value::as_object_mut) {
            for key in ["models", "personalModelIds", "modelOrder"] { config.remove(key); }
        }
    }
    value
}
fn check_identities(paths: &ConfigPaths, identities: &[(String, Value)], after_commit: bool) -> Result<(), String> {
    if identities.is_empty() { return Ok(()); }
    let current = config::load_records(paths)?;
    if identities.iter().any(|(pid, expected)| !current.iter().any(|record| record.info.id == *pid &&
        if after_commit { transport_identity(&record.identity) == transport_identity(expected) } else { record.identity == *expected })) {
        return Err("探针期间 provider 配置或认证发生变化，请重新发现后再导入。".into());
    }
    Ok(())
}
fn import_with_checks(service: &ModelsService, groups: &[(String, Vec<RichModelRow>)], op: &Operation,
    identities: &[(String, Value)], hook: &dyn Fn(&str, &Path)) -> ImportReport {
    let mut out = ImportReport::default();
    let mut plans = Vec::new();
    let run = |out: &mut ImportReport, plans: &mut Vec<Plan>| -> Result<(), String> {
        op.check()?;
        let _local_lock = loop {
            op.check()?;
            match IMPORT_LOCK.get_or_init(Default::default).try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => op.wait(Duration::from_millis(25))?,
                Err(std::sync::TryLockError::Poisoned(_)) => return Err("模型导入锁不可用。".into()),
            }
        };
        let mut paths: Vec<_> = [service.paths.personal_config.as_ref(), service.paths.v2_config.as_ref()].into_iter().flatten().filter(|p| p.exists()).cloned().collect();
        paths.sort(); paths.dedup();
        if paths.len() == 1 && service.paths.personal_config == service.paths.v2_config { return Err("个人配置与账号配置不能是同一个文件。".into()); }
        let mut locks = Vec::new();
        for path in &paths { protect_path(path)?; locks.push(SideLock::acquire(path)?); }
        let personal_doc = config::optional_doc(service.paths.personal_config.as_ref())?;
        let account_doc = config::optional_doc(service.paths.v2_config.as_ref())?;
        let personal_ids: HashSet<_> = personal_doc.as_ref().map(|d| config::personal_rules(&d.value)).transpose()?.unwrap_or_default()
            .iter().map(|r| string(&r["providerId"]).to_owned()).collect();
        let records = config::load_records(&service.paths)?;
        if identities.iter().any(|(pid, expected)| !records.iter().any(|record| record.info.id == *pid && record.identity == *expected)) {
            return Err("探针期间 provider 配置或认证发生变化，请重新发现后再导入。".into());
        }
        // 身份读取和写入快照必须来自同一份文件；外部客户端不遵守侧锁，因此逐次比较原始字节。
        let mut watched: Vec<_> = [personal_doc.as_ref(), account_doc.as_ref()].into_iter().flatten()
            .map(|doc| (doc.path.clone(), Some(doc.raw.clone()))).collect();
        for path in [&service.paths.personal_config, &service.paths.v2_config, &service.paths.catalog].into_iter().flatten() {
            if watched.iter().any(|(p, _)| config::path_key(p) == config::path_key(path)) { continue; }
            let raw = match fs::read(path) { Ok(bytes) => Some(bytes), Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, Err(_) => return Err("无法读取模型配置一致性快照。".into()) };
            watched.push((path.clone(), raw));
        }
        let unchanged = |watched: &[(PathBuf, Option<Vec<u8>>)]| watched.iter().all(|(path, raw)| {
            match (fs::read(path), raw) { (Ok(actual), Some(expected)) => actual == *expected,
                (Err(e), None) => e.kind() == std::io::ErrorKind::NotFound, _ => false }
        });
        if !unchanged(&watched) { return Err("准备导入时配置发生并发变化，尚未修改配置。".into()); }
        let mut personal: Vec<(String, Vec<RichModelRow>)> = Vec::new();
        let mut account: Vec<(String, Vec<RichModelRow>)> = Vec::new(); let mut pairs = HashSet::new();
        let mut count = 0;
        for (pid, rows) in groups {
            if rows.is_empty() { continue; }
            let Some(record) = records.iter().find(|r| r.info.id == *pid) else {
                if config::valid_id(pid, &[]) { out.result.missing.push(pid.clone()); }
                continue;
            };
            let secrets = record.secrets(); let mut selected = Vec::new();
            for row in rows {
                if row.row.provider_id != *pid || !config::valid_id(&row.row.id, &secrets) { return Err("模型条目与目标 provider 不匹配或 ID 无效；尚未修改配置。".into()); }
                if !pairs.insert((pid.clone(), row.row.id.clone())) { continue; }
                count += 1; if count > MAX_MODELS { return Err("单次最多导入 200 个模型，尚未修改配置。".into()); }
                selected.push(row.clone());
            }
            let destination = if personal_ids.contains(pid) { &mut personal } else { &mut account };
            match destination.iter_mut().find(|(id, _)| id == pid) {
                Some((_, existing)) => existing.extend(selected),
                None => destination.push((pid.clone(), selected)),
            }
        }
        if !personal.is_empty() { plans.push(personal_plan(personal_doc.ok_or("找不到个人 provider 配置。")?, &personal, &mut out.result)?); }
        if !account.is_empty() { plans.push(account_plan(account_doc.ok_or("找不到当前账号配置。")?, &account, &mut out.result)?); }
        plans.retain(|p| !p.report.added_ids.is_empty());
        // 全部目标先完成 schema 校验、备份和暂存；任一备份失败则所有原文件不动。
        for plan in plans.iter_mut() {
            op.check()?;
            if !snapshot_matches(&plan.doc) { return Err("准备导入时配置发生并发变化，尚未修改配置。".into()); }
            hook("before_backup", &plan.doc.path); backup(plan, &service.paths.backup_dir)?; stage(plan)?;
        }
        hook("prepared", paths.first().map(PathBuf::as_path).unwrap_or(Path::new("")));
        check_identities(&service.paths, identities, false)?;
        if !unchanged(&watched) { return Err("配置在备份后发生并发变化，尚未修改配置。".into()); }
        for plan in plans.iter_mut() {
            op.check()?; hook("before_commit", &plan.doc.path); op.check()?;
            check_identities(&service.paths, identities, out.result.added > 0)?;
            if !unchanged(&watched) || !snapshot_matches(&plan.doc) { plan.report.error = "提交前配置发生并发变化，该文件未写入。".into(); return Err(plan.report.error.clone()); }
            let tmp = plan.staged.as_ref().ok_or("缺少暂存文件。")?;
            if replace_file(&tmp.0, &plan.doc.path).is_err() {
                plan.report.error = "配置原子替换失败；请检查该文件权限或占用。".into(); return Err(plan.report.error.clone());
            }
            plan.report.written = true;
            out.result.added += plan.report.added_ids.len(); out.result.added_ids.extend(plan.report.added_ids.clone());
            // 后续文件校验认可本次已经写入的内容，仍拒绝其它并发变更。
            if let Some((_, raw)) = watched.iter_mut().find(|(path, _)| *path == plan.doc.path) {
                let mut bytes = serde_json::to_vec_pretty(&plan.next).map_err(|_| "已替换文件的一致性编码失败。".to_string())?;
                bytes.push(b'\n'); *raw = Some(bytes);
            }
            #[cfg(unix)] { if let Some(parent) = plan.doc.path.parent() { if let Ok(dir) = File::open(parent) { if dir.sync_all().is_err() { plan.report.error = "文件已替换，但目录持久化同步失败。".into(); return Err(plan.report.error.clone()); } } } }
            hook("after_commit", &plan.doc.path);
            op.emit("", "", "import", out.result.added, count);
        }
        Ok(())
    };
    if let Err(error) = run(&mut out, &mut plans) { out.result.error = error; }
    out.files = plans.into_iter().map(|p| p.report).collect();
    out.result.backup = out.files.iter().find(|f| !f.backup.is_empty()).map(|f| f.backup.clone()).unwrap_or_default();
    out.partial = out.result.added > 0 && !out.result.error.is_empty();
    if out.partial { out.result.error = format!("部分文件已写入（{} 个模型），其余未完成：{}", out.result.added, out.result.error); }
    out
}
fn contains_expected(actual: &Value, expected: &Value) -> bool {
    match expected.as_object() {
        Some(o) => actual.as_object().is_some_and(|a| o.iter().all(|(key, v)| a.get(key).is_some_and(|got| contains_expected(got, v)))),
        None => actual == expected,
    }
}
fn retained(data: &Value, personal: bool, pid: &str, mid: &str, expected: &Value) -> bool {
    if !personal {
        return data["provider"].get(pid).is_some_and(|p| p.get("enabled") != Some(&Value::Bool(false)) &&
            p["models"].get(mid).is_some_and(|m| m.is_object() && m.get("enabled") != Some(&Value::Bool(false)) && contains_expected(m, expected)));
    }
    let Ok(rules) = config::personal_rules(data) else { return false; };
    let Ok(models) = validate_model_rules(data) else { return false; };
    let cfg = rules.iter().find(|r| string(&r["providerId"]) == pid).map(|r| &r["config"]);
    let Some(cfg) = cfg else { return false; };
    let listed = ["personalModelIds", "modelOrder"].iter().all(|key| cfg[*key].as_array().is_some_and(|a| a.iter().any(|v| v.as_str() == Some(mid))));
    listed && models.iter().any(|m| string(&m["providerId"]) == pid && string(&m["modelId"]) == mid && m["config"]["enabled"] == true && contains_expected(&m["config"], expected))
}
pub(super) fn verify_report(report: &ImportReport, delay: Duration, op: &Operation) -> Verification {
    let mut out = Verification { retained: true, ..Default::default() };
    let files: Vec<_> = report.files.iter().filter(|f| f.written && !f.added_ids.is_empty()).collect();
    if files.is_empty() { return out; }
    if let Err(error) = op.wait(delay) { return Verification { error, cancelled: op.cancel.is_cancelled(), ..Default::default() }; }
    for file in files {
        if let Err(error) = op.check() { out.retained = false; out.error = error; out.cancelled = op.cancel.is_cancelled(); break; }
        let doc = config::read_optional(Path::new(&file.path));
        for (pid, mid, expected) in &file.expected {
            if !doc.as_ref().ok().and_then(|v| v.as_ref()).is_some_and(|d| retained(&d.value, file.personal, pid, mid, expected)) {
                out.retained = false; out.missing.push(format!("{pid} / {mid}"));
            }
        }
        if doc.is_err() { out.error = "配置延迟读回失败；没有确认客户端运行态。".into(); }
    }
    if !out.retained && out.error.is_empty() { out.error = "新增配置未完整保留，可能被并发保存覆盖；请刷新配置后重试。".into(); }
    out
}
pub(super) fn verify_ids(paths: &ConfigPaths, pairs: &[(String, String)], op: &Operation) -> bool {
    if op.wait(Duration::from_millis(1200)).is_err() { return false; }
    let Ok(personal) = config::optional_doc(paths.personal_config.as_ref()) else { return false; };
    let Ok(account) = config::optional_doc(paths.v2_config.as_ref()) else { return false; };
    let pids: HashSet<_> = personal.as_ref().and_then(|d| config::personal_rules(&d.value).ok()).unwrap_or_default().iter().map(|r| string(&r["providerId"]).to_owned()).collect();
    pairs.iter().all(|(pid, mid)| {
        if pids.contains(pid) { personal.as_ref().is_some_and(|d| retained(&d.value, true, pid, mid, &json!({"enabled":true}))) }
        else { account.as_ref().is_some_and(|d| retained(&d.value, false, pid, mid, &json!({}))) }
    })
}
