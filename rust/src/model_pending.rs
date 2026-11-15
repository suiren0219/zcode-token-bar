//! 模型页的待办文件边界。只存确认过的模型/能力，不存 provider 配置或凭据。
use super::{group_rows, row_key, sanitize_caps, valid_identifier, Groups};
use crate::models::{self, Capabilities, ImportReport, ModelsService, Operation, RichModelRow, Verification};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(super) enum Intent {
    Import(Groups),
    Probe { provider_id: String, rows: Vec<RichModelRow>, discover: bool },
}

#[derive(Clone)]
pub(super) struct Snapshot {
    // 不实现 Debug，未知旧文件也不能把原始内容送入日志/UI。
    raw: Vec<u8>,
    stamp: Stamp,
    pub fingerprint: [u8; 32],
    pub intent: Result<Intent, String>,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)] dev: u64,
    #[cfg(unix)] ino: u64,
}

pub(super) struct Inspection {
    pub snapshot: Option<Snapshot>,
    pub running: Option<bool>,
    pub note: String,
}

fn unique() -> String {
    format!("{}-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), SERIAL.fetch_add(1, Ordering::Relaxed))
}

fn stamp(meta: &fs::Metadata) -> Stamp {
    #[cfg(unix)] use std::os::unix::fs::MetadataExt;
    Stamp {
        len: meta.len(), modified: meta.modified().ok(), created: meta.created().ok(),
        #[cfg(unix)] dev: meta.dev(),
        #[cfg(unix)] ino: meta.ino(),
    }
}

fn ordinary(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() { return false; }
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 { return false; }
    }
    true
}

fn bounded(file: &mut File, limit: u64) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    file.take(limit + 1).read_to_end(&mut raw).map_err(|_| "无法读取模型待办，原文件保留。".to_string())?;
    if raw.len() as u64 > limit { return Err("模型待办超过大小限制，原文件保留。".into()); }
    Ok(raw)
}

pub(super) fn read(path: &Path) -> Result<Option<Snapshot>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法检查模型待办文件。".into()),
    };
    if !ordinary(&meta) { return Err("模型待办必须是普通文件，未跟随链接。".into()); }
    let mut file = File::open(path).map_err(|_| "模型待办暂不可读，原文件保留。".to_string())?;
    let before = stamp(&file.metadata().map_err(|_| "无法检查模型待办。".to_string())?);
    let raw = bounded(&mut file, MAX_BYTES)?;
    let after = stamp(&file.metadata().map_err(|_| "无法检查模型待办。".to_string())?);
    if before != after || before != stamp(&meta) { return Err("模型待办正在被其它程序修改，稍后重试。".into()); }
    let fingerprint = Sha256::digest(&raw).into();
    let intent = decode(&raw);
    Ok(Some(Snapshot { raw, stamp: before, fingerprint, intent }))
}

pub(super) fn inspect(path: &Path, op: &Operation) -> Result<Inspection, String> {
    op.check()?;
    let snapshot = read(path)?;
    let mut running = None;
    let note = match &snapshot {
        None => String::new(),
        Some(s) => match &s.intent {
            Ok(Intent::Import(groups)) => {
                running = client_running().ok();
                let count: usize = groups.iter().map(|(_, r)| r.len()).sum();
                match running {
                    Some(true) => format!("有 {count} 个已确认模型待续办；完全退出 ZCode（含托盘）后自动导入，不再发探针。可点击“取消待办”备份后移除。"),
                    Some(false) => format!("检测到 {count} 个已确认模型待续办，正在确认 ZCode 已完全退出。"),
                    None => "检测到模型待办，但无法确认客户端进程状态；暂不自动写入，可稍后继续。".into(),
                }
            }
            Ok(Intent::Probe { .. }) => "检测到旧版探针待办；不会自动重发可能计费的请求。点击“继续待办”预览，再次点击确认同批实测；也可取消待办。".into(),
            Err(error) => format!("{} 可点击“取消待办”备份后移除；不会自动覆盖。", error),
        },
    };
    op.check()?;
    Ok(Inspection { snapshot, running, note })
}

fn positive(value: &Value) -> Option<u64> { value.as_u64().filter(|n| *n > 0 && *n < 100_000_000) }

pub(super) fn normalize_row(pid: &str, fallback_id: Option<&str>, value: &Value) -> Result<RichModelRow, String> {
    let id = value.get("id").and_then(Value::as_str).or_else(|| value.as_str()).or(fallback_id).unwrap_or("");
    if !valid_identifier(pid) || !valid_identifier(id) { return Err("待办包含无效的提供方或模型 ID，未写入。".into()); }
    if value.get("provider_id").and_then(Value::as_str).is_some_and(|p| p != pid) { return Err("待办模型与目标提供方不匹配，未写入。".into()); }
    let caps = value.get("capabilities").filter(|v| v.is_object()).unwrap_or(value);
    let mut out = Capabilities::default();
    for (name, flags) in [("input", &mut out.input), ("output", &mut out.output)] {
        for modality in models::MODALITIES {
            if let Some(flag) = caps[name][modality].as_bool() { flags.set(modality, flag); }
        }
    }
    out.context_window = positive(&caps["context_window"]).or_else(|| positive(&caps["context"]))
        .or_else(|| positive(&caps["contextWindow"])).or_else(|| positive(&caps["limit"]["context"]))
        .or_else(|| positive(&value["context"]));
    out.output_limit = positive(&caps["output_limit"]).or_else(|| positive(&caps["max_output_tokens"]))
        .or_else(|| positive(&caps["limit"]["output"]));
    out.tools = caps["tools"].as_bool();
    out.reasoning = caps["reasoning"].as_bool();
    out.structured_output = caps["structured_output"].as_bool();
    out.web_search = caps["web_search"].as_bool();
    if let Some(sources) = caps["sources"].as_object() {
        for (key, source) in sources {
            if let Some(source) = source.as_str() {
                out.sources.insert(if key == "context" { "context_window".into() } else { key.clone() }, source.into());
            }
        }
    }
    let mut row = RichModelRow::from(models::ModelRow { provider_id: pid.into(), provider: pid.into(), id: id.into() });
    row.capabilities = out;
    sanitize_caps(&mut row);
    Ok(row)
}

fn decode(raw: &[u8]) -> Result<Intent, String> {
    let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(raw);
    let value: Value = serde_json::from_slice(raw).map_err(|_| "旧模型待办不是有效 JSON，原文件保留。".to_string())?;
    match value["kind"].as_str() {
        Some("import") => {
            let map = value.pointer("/payload/groups").and_then(Value::as_object).ok_or("导入待办缺少有效 groups，原文件保留。")?;
            let mut groups = Vec::new();
            let mut seen = HashSet::new();
            for (pid, rows) in map {
                let rows = rows.as_array().ok_or("待办模型清单不是数组，原文件保留。")?;
                let mut normalized = Vec::new();
                for row in rows {
                    let row = normalize_row(pid, None, row)?;
                    if seen.insert(row_key(&row)) { normalized.push(row); }
                    if seen.len() > models::MAX_MODELS { return Err("待办模型超过单次 200 个上限，原文件保留。".into()); }
                }
                if !normalized.is_empty() { groups.push((pid.clone(), normalized)); }
            }
            if groups.is_empty() { return Err("导入待办没有模型，原文件保留。".into()); }
            Ok(Intent::Import(groups))
        }
        Some("probe") => {
            let payload = &value["payload"];
            let pid = payload["provider_id"].as_str().filter(|s| valid_identifier(s)).ok_or("旧探针待办缺少有效提供方，原文件保留。")?;
            let discover = payload["auto"] == true;
            let mut rows = Vec::new();
            if let Some(raw_rows) = payload["rows"].as_array() {
                if raw_rows.len() > models::MAX_MODELS { return Err("旧探针待办模型过多，原文件保留。".into()); }
                for row in raw_rows { rows.push(normalize_row(pid, None, row)?); }
            }
            if rows.is_empty() && !discover { return Err("旧探针待办没有模型，原文件保留。".into()); }
            Ok(Intent::Probe { provider_id: pid.into(), rows, discover })
        }
        _ => Err("检测到未知类型的旧模型待办，原文件保留。".into()),
    }
}

pub(super) fn validate_groups(service: &ModelsService, groups: &Groups, op: &Operation) -> Result<Groups, String> {
    let providers = service.providers()?;
    let mut valid = Vec::new();
    for (pid, rows) in groups {
        op.check()?;
        let provider = providers.iter().find(|p| p.id == *pid).ok_or("待办目标提供方已移除或停用；保留待办，未写入。")?;
        let input = rows.iter().map(|r| r.row.id.as_str()).collect::<Vec<_>>().join("\n");
        let checked = service.manual_rows(pid, &input)?;
        let allowed: HashSet<_> = checked.iter().map(|r| r.row.id.as_str()).collect();
        if rows.iter().any(|r| r.row.provider_id != *pid || !allowed.contains(r.row.id.as_str())) {
            return Err("待办 ID 无法通过当前配置校验；保留待办，未写入。".into());
        }
        let mut rows = rows.clone();
        for row in &mut rows { row.row.provider = provider.name.clone(); sanitize_caps(row); }
        valid.push((pid.clone(), rows));
    }
    Ok(valid)
}

pub(super) fn prepare_probe(service: &ModelsService, snapshot: &Snapshot, op: &Operation) -> Result<Groups, String> {
    let Intent::Probe { provider_id, rows, discover } = snapshot.intent.as_ref().map_err(Clone::clone)? else { return Err("该待办不是探针任务。".into()); };
    let rows = if *discover && rows.is_empty() { service.discover(provider_id, op)?.rows } else { rows.clone() };
    let mut seen = HashSet::new();
    let rows: Vec<_> = rows.into_iter().filter(|r| seen.insert(row_key(r))).take(models::MAX_PROBE_MODELS).collect();
    if rows.is_empty() { return Err("旧探针待办未取得可实测模型，未发送收费请求。".into()); }
    validate_groups(service, &vec![(provider_id.clone(), rows)], op)
}

fn encoded_import(groups: &Groups) -> Result<Vec<u8>, String> {
    let mut map = BTreeMap::new();
    for (pid, rows) in groups {
        let entries: Vec<_> = rows.iter().map(|row| {
            let mut row = row.clone(); sanitize_caps(&mut row);
            json!({"id":row.row.id,"capabilities":row.capabilities})
        }).collect();
        if !entries.is_empty() { map.insert(pid.clone(), entries); }
    }
    if map.is_empty() { return Err("没有可持久化的待办模型。".into()); }
    let raw = serde_json::to_vec_pretty(&json!({"version":1,"kind":"import","payload":{"groups":map}}))
        .map_err(|_| "无法编码模型待办。".to_string())?;
    if raw.len() as u64 > MAX_BYTES { return Err("模型待办超过大小限制。".into()); }
    Ok(raw)
}

fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new(); options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    options.open(path)
}

fn stage(path: &Path, raw: &[u8], purpose: &str) -> Result<PathBuf, String> {
    let dir = path.parent().ok_or("待办目录无效。")?;
    fs::create_dir_all(dir).map_err(|_| "无法创建待办目录。".to_string())?;
    let staged = dir.join(format!(".pending-import-{purpose}-{}.json", unique()));
    let mut file = create_private(&staged).map_err(|_| "无法创建待办备份/暂存文件。".to_string())?;
    if file.write_all(raw).and_then(|_| file.sync_all()).is_err() {
        drop(file); let _ = fs::remove_file(&staged);
        return Err("待办备份/暂存写入失败，原待办保留。".into());
    }
    drop(file);
    if fs::read(&staged).ok().as_deref() != Some(raw) { return Err("待办备份读回不一致，原待办保留。".into()); }
    Ok(staged)
}

fn publish_new(path: &Path, raw: &[u8]) -> Result<(), String> {
    let staged = stage(path, raw, "new")?;
    // hard_link 是同卷的 create-if-absent 发布：文件完整可见，绝不替换已有待办。
    let result = fs::hard_link(&staged, path).map_err(|_| "已有其它待办或无法发布待办，未覆盖原文件。".to_string());
    let _ = fs::remove_file(staged);
    result
}

pub(super) fn save_failed(path: &Path, groups: &Groups, report: &ImportReport, verification: &Verification, op: &Operation) -> String {
    if verification.retained || verification.cancelled || op.cancel.is_cancelled() || report.result.added == 0 { return String::new(); }
    // 仅续办确实提交过、读回未确认的行；不把 skipped、缺失 provider 或未实测行塞入待办。
    let committed: HashSet<_> = report.result.added_ids.iter().cloned().collect();
    let rows = group_rows(groups.iter().flat_map(|(_, rows)| rows.iter()).filter(|r| committed.contains(&row_key(r))));
    if rows.is_empty() { return String::new(); }
    let raw = match encoded_import(&rows) { Ok(raw) => raw, Err(e) => return e };
    if op.cancel.is_cancelled() { return String::new(); }
    match publish_new(path, &raw) {
        Ok(()) => "读回未确认，已保存导入待办；完全退出 ZCode 后自动续办，只重写已确认模型，不重发探针。".into(),
        Err(_) => {
            // 旧待办不被覆盖；新的确认意图另存恢复副本，避免静默丢失。
            match stage(path, &raw, "deferred") {
                Ok(_) => "已有旧待办，未覆盖；本次导入意图已另存 .pending-import-deferred-*.json 恢复副本。先处理当前待办，再手动重新导入本批。".into(),
                Err(_) => "读回未确认且待办保存失败；已有文件未覆盖，请保留本批列表后手动重试。".into(),
            }
        }
    }
}

pub(super) fn matches(path: &Path, expected: &Snapshot) -> Result<bool, String> {
    Ok(read(path)?.is_some_and(|now| now.stamp == expected.stamp && now.raw == expected.raw))
}

// Windows 打开句柄时拒绝其它写入/删除，再按句柄删除，不能在检查和 unlink 之间误删新文件。
#[cfg(windows)]
fn open_guarded(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new().read(true).access_mode(0x8000_0000 | 0x0001_0000).share_mode(1).open(path)
}
#[cfg(not(windows))]
fn open_guarded(path: &Path) -> std::io::Result<File> { File::open(path) }

#[cfg(windows)]
fn delete_open_file(file: &File, _path: &Path, _expected: &Snapshot) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" { fn SetFileInformationByHandle(handle: *mut std::ffi::c_void, class: i32, info: *const std::ffi::c_void, size: u32) -> i32; }
    let delete: u8 = 1;
    let result = unsafe { SetFileInformationByHandle(file.as_raw_handle(), 4, &delete as *const u8 as *const _, 1) };
    if result == 0 { Err("无法移除已备份待办，文件保留。".into()) } else { Ok(()) }
}
#[cfg(not(windows))]
fn delete_open_file(_file: &File, path: &Path, expected: &Snapshot) -> Result<(), String> {
    if !matches(path, expected)? { return Err("待办已被其它程序更新，未移除新文件。".into()); }
    fs::remove_file(path).map_err(|_| "无法移除已备份待办。".into())
}

pub(super) fn retire(path: &Path, expected: &Snapshot, op: &Operation) -> Result<(), String> {
    op.check()?;
    let meta = fs::symlink_metadata(path).map_err(|_| "待办已不存在或不可访问。".to_string())?;
    if !ordinary(&meta) { return Err("待办路径类型已变化，未移除。".into()); }
    let mut file = open_guarded(path).map_err(|_| "待办正被其它程序使用，未移除。".to_string())?;
    let current = stamp(&file.metadata().map_err(|_| "无法检查待办状态。".to_string())?);
    let raw = bounded(&mut file, MAX_BYTES)?;
    if current != expected.stamp || raw != expected.raw { return Err("待办已被其它程序更新，未移除新文件。".into()); }
    // 取消与成功消费都先备份本次读取快照，不备份一个稍后读取的不同文件。
    stage(path, &expected.raw, "backup")?;
    op.check()?;
    delete_open_file(&file, path, expected)
}

fn config_value(path: Option<&PathBuf>) -> Result<Option<Value>, String> {
    let Some(path) = path else { return Ok(None); };
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("待办读回配置失败。".into()),
    };
    let raw = bounded(&mut file, 16 * 1024 * 1024)?;
    let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw);
    serde_json::from_slice(raw).map(Some).map_err(|_| "待办读回配置不是有效 JSON。".into())
}

pub(super) fn all_present(service: &ModelsService, groups: &Groups, op: &Operation) -> Result<bool, String> {
    op.check()?;
    let personal = config_value(service.paths.personal_config.as_ref())?.unwrap_or(Value::Null);
    let account = config_value(service.paths.v2_config.as_ref())?.unwrap_or(Value::Null);
    let providers = personal.pointer("/config/providerConfigRules/providerRules").and_then(Value::as_array);
    let rules = personal.pointer("/config/modelConfigRules/providerModelRules").and_then(Value::as_array);
    for (pid, rows) in groups {
        for row in rows {
            let mid = row.row.id.as_str();
            let pcfg = providers.and_then(|ps| ps.iter().find(|p| p["providerId"].as_str() == Some(pid))).map(|p| &p["config"]);
            let found = if let Some(cfg) = pcfg {
                cfg["enabled"] != false && ["personalModelIds", "modelOrder"].iter().all(|key| cfg[*key].as_array().is_some_and(|a| a.iter().any(|v| v.as_str() == Some(mid))))
                    && rules.is_some_and(|rs| rs.iter().any(|r| r["providerId"].as_str() == Some(pid) && r["modelId"].as_str() == Some(mid) && r["config"]["enabled"] == true))
            } else {
                account["provider"].get(pid).is_some_and(|p| p["enabled"] != false && p["models"].get(mid).is_some_and(|m| m.is_object() && m["enabled"] != false))
            };
            if !found { return Ok(false); }
        }
    }
    op.check()?;
    Ok(true)
}

pub(super) fn resume_import(path: &Path, snapshot: &Snapshot, service: &ModelsService, op: &Operation) -> Result<(ImportReport, Verification, String), String> {
    let Intent::Import(groups) = snapshot.intent.as_ref().map_err(Clone::clone)? else { return Err("探针待办必须显式预览并再次确认，不能自动续办。".into()); };
    if client_running()? { return Err("ZCode 仍在运行，待办保留；完全退出后自动续办。".into()); }
    let groups = validate_groups(service, groups, op)?;
    if !matches(path, snapshot)? { return Err("待办已变更，本次未导入；请重新读取。".into()); }
    op.check()?;
    let report = service.import_rows(&groups, op);
    let verification = super::verify_import(service, &report, op);
    let confirmed = verification.retained && !verification.cancelled && verification.error.is_empty()
        && report.result.error.is_empty() && report.result.missing.is_empty() && all_present(service, &groups, op)?;
    let note = if confirmed {
        match retire(path, snapshot, op) {
            Ok(()) => "待办已写入并读回确认，已备份并移除本次待办；尚未取得客户端运行态接受确认。".into(),
            Err(e) => format!("待办模型已读回确认；{e}"),
        }
    } else { "待办尚未完整读回确认，文件保留；本次不自动循环重试，可点击“继续待办”。".into() };
    Ok((report, verification, note))
}

pub(super) fn finish_legacy_probe(path: &Path, snapshot: &Snapshot, rows: &[RichModelRow], report: &ImportReport, verification: &Verification, op: &Operation) -> String {
    if op.cancel.is_cancelled() { return "旧探针待办仍保留；不会自动重新发起请求。".into(); }
    if !verification.retained && report.result.added > 0 {
        let committed: HashSet<_> = report.result.added_ids.iter().cloned().collect();
        let groups = group_rows(rows.iter().filter(|r| committed.contains(&row_key(r))));
        let raw = match encoded_import(&groups) { Ok(raw) => raw, Err(e) => return e };
        // 先保留可恢复的导入意图，再消耗旧 probe；并发新文件永不覆盖。
        let recovery = match stage(path, &raw, "deferred") { Ok(p) => p, Err(e) => return e };
        if let Err(e) = retire(path, snapshot, op) { return format!("{e} 已另外保存本轮可用模型的导入恢复副本，不会自动重探。"); }
        if let Err(e) = fs::hard_link(&recovery, path) { let _ = e; return "出现新的待办，未覆盖；本轮可用模型的导入恢复副本已保留。".into(); }
        let _ = fs::remove_file(recovery);
        return "旧探针待办已转为纯导入待办，保留本轮可用模型和能力；退出客户端后只续办写入，不再探测。".into();
    }
    if report.result.error.is_empty() && verification.retained && !verification.cancelled {
        match retire(path, snapshot, op) {
            Ok(()) => "本次显式探针待办已消费并备份；未确认可用或超过上限的模型不会自动重试。".into(),
            Err(e) => e,
        }
    } else { "旧探针待办保留；再次实测仍需显式二次确认，绝不自动重试。".into() }
}

#[cfg(windows)]
pub(super) fn client_running() -> Result<bool, String> {
    // Toolhelp 按可执行文件名检查，涵盖托盘/无可见窗口；不读取进程凭据。
    use std::ffi::c_void;
    #[repr(C)]
    struct Entry { size: u32, usage: u32, pid: u32, heap: usize, module: u32, threads: u32, parent: u32, priority: i32, flags: u32, exe: [u16; 260] }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    unsafe {
        let handle = CreateToolhelp32Snapshot(2, 0);
        if handle as isize == -1 { return Err("无法确认客户端是否完全退出，未续办。".into()); }
        struct Handle(*mut c_void);
        impl Drop for Handle { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }
        let _guard = Handle(handle);
        let mut entry: Entry = std::mem::zeroed(); entry.size = std::mem::size_of::<Entry>() as u32;
        if Process32FirstW(handle, &mut entry) == 0 { return Err("无法枚举客户端进程，未续办。".into()); }
        loop {
            let len = entry.exe.iter().position(|c| *c == 0).unwrap_or(entry.exe.len());
            if String::from_utf16_lossy(&entry.exe[..len]).eq_ignore_ascii_case("zcode.exe") { return Ok(true); }
            if Process32NextW(handle, &mut entry) == 0 {
                return if GetLastError() == 18 { Ok(false) } else { Err("客户端进程检查未完成，未续办。".into()) };
            }
        }
    }
}
#[cfg(not(windows))]
pub(super) fn client_running() -> Result<bool, String> { Err("当前平台不能确认 ZCode 已完全退出，待办保留。".into()) }

#[cfg(test)]
mod tests {
    use super::*;
    fn groups(id: &str) -> Groups {
        vec![("p".into(), vec![normalize_row("p", None, &json!({"id":id,"capabilities":{"context":128000,"tools":true}})).unwrap()])]
    }
    #[test]
    fn legacy_import_is_confirmed_intent_with_safe_capabilities() {
        let bytes = br#"{"kind":"import","payload":{"groups":{"p":[{"id":"m","apiKey":"never","capabilities":{"context":64000,"tools":false,"input":{"image":true,"pdf":"true"},"sources":{"tools":"provider API","secret":"never"}}}]}}}"#;
        let Intent::Import(groups) = decode(bytes).unwrap() else { panic!("wrong kind") };
        let row = &groups[0].1[0];
        assert_eq!(row.row.provider_id, "p"); assert_eq!(row.capabilities.context_window, Some(64000));
        assert_eq!(row.capabilities.tools, Some(false)); assert_eq!(row.capabilities.input.pdf, None);
        let written = String::from_utf8(encoded_import(&groups).unwrap()).unwrap();
        assert!(!written.contains("never")); assert!(!written.contains("apiKey"));
    }
    #[test]
    fn legacy_probe_never_decodes_to_import() {
        assert!(matches!(decode(br#"{"kind":"probe","payload":{"provider_id":"p","auto":true}}"#).unwrap(), Intent::Probe { discover: true, .. }));
    }
    #[test]
    fn invalid_legacy_limits_stay_unknown_and_false_stays_false() {
        let row = normalize_row("p", None, &json!({"id":"m","capabilities":{"context":true,"output_limit":-1,"tools":false}})).unwrap();
        assert_eq!(row.capabilities.context_window, None); assert_eq!(row.capabilities.output_limit, None); assert_eq!(row.capabilities.tools, Some(false));
    }
    #[test]
    fn new_pending_never_overwrites_unknown_or_concurrent_file() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("pending-import.json");
        fs::write(&path, b"unknown original").unwrap();
        assert!(publish_new(&path, &encoded_import(&groups("m")).unwrap()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"unknown original");
    }
    #[test]
    fn cancel_backs_up_exact_snapshot_before_removing() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("pending-import.json");
        let raw = encoded_import(&groups("m")).unwrap(); publish_new(&path, &raw).unwrap();
        let snapshot = read(&path).unwrap().unwrap();
        retire(&path, &snapshot, &Operation::new(std::time::Duration::from_secs(10))).unwrap();
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path()).unwrap().filter_map(Result::ok).any(|e| fs::read(e.path()).ok() == Some(raw.clone())));
    }
    #[test]
    fn changed_pending_survives_cancel_and_success_cleanup() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("pending-import.json");
        publish_new(&path, &encoded_import(&groups("old")).unwrap()).unwrap();
        let snapshot = read(&path).unwrap().unwrap();
        let newer = encoded_import(&groups("new")).unwrap(); fs::write(&path, &newer).unwrap();
        assert!(retire(&path, &snapshot, &Operation::new(std::time::Duration::from_secs(10))).is_err());
        assert_eq!(fs::read(path).unwrap(), newer);
    }
    #[test]
    fn failed_readback_queues_only_committed_confirmed_rows() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("pending-import.json");
        let mut input = groups("new"); input[0].1.push(normalize_row("p", None, &json!({"id":"skip"})).unwrap());
        let mut report = ImportReport::default(); report.result.added = 1; report.result.added_ids.push(("p".into(), "new".into()));
        let note = save_failed(&path, &input, &report, &Verification::default(), &Operation::new(std::time::Duration::from_secs(10)));
        assert!(note.contains("已保存导入待办"));
        let snapshot = read(&path).unwrap().unwrap(); let Intent::Import(groups) = snapshot.intent.unwrap() else { panic!("wrong kind") };
        assert_eq!(groups[0].1.len(), 1); assert_eq!(groups[0].1[0].row.id, "new");
    }
    #[test]
    fn cancelled_operation_never_creates_or_removes_pending() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("pending-import.json");
        let op = Operation::new(std::time::Duration::from_secs(10)); op.cancel.cancel();
        let mut report = ImportReport::default(); report.result.added = 1; report.result.added_ids.push(("p".into(), "m".into()));
        assert!(save_failed(&path, &groups("m"), &report, &Verification::default(), &op).is_empty()); assert!(!path.exists());
        publish_new(&path, &encoded_import(&groups("m")).unwrap()).unwrap(); let snapshot = read(&path).unwrap().unwrap();
        assert!(retire(&path, &snapshot, &op).is_err()); assert!(path.exists());
    }
}
