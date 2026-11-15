//! 客户端更新检测与原生视觉恢复调度；同步接口应由后台线程调用。
//!
//! observe 只读官方文件和本机 CDP 页面列表，仅返回恢复建议；自动恢复开关由主程序控制。
//! 旧 token 的 asar 注入若被更新覆盖，当前独立 Rust exe 并不依赖它。
//! 恢复只经 wallpaper::apply 将壁纸 / Monet / resize overlay 应用到运行中的 renderer，
//! 不退出客户端、不修 launcher、不备份或写回 app.asar，也不覆盖任何官方二进制。

use crate::wallpaper::{self, WallpaperSettings};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const STATE_FILE: &str = "native-client-recovery.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
// 所有实例及显式恢复共用锁；observe 每次重读状态，避免旧实例覆盖 restore 的结果。
static OPERATION: Mutex<()> = Mutex::new(());
static STAMPS: OnceLock<Mutex<BTreeMap<PathBuf, ClientStamp>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientStamp {
    pub exe_path: PathBuf,
    pub app_asar_path: PathBuf,
    pub version: Option<String>,
    pub len: u64,
    /// app.asar 的 Unix 纳秒时间戳；无法获取时不复用哈希。
    pub modified: Option<u64>,
    pub hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecoveryState {
    pub schema_version: u32,
    pub client: Option<ClientStamp>,
    /// 仅本模块自己的规范化壁纸配置意图；不保存 auth、页面 URL 或 CDP 响应。
    pub wallpaper_settings: Option<WallpaperSettings>,
    pub pending: bool,
    /// 排序去重后的 target 身份哈希；不持久保存原始调试地址。
    pub target_fingerprints: Vec<String>,
    pub cdp_port: Option<u16>,
    pub cdp_online: bool,
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            client: None,
            wallpaper_settings: None,
            pending: false,
            target_fingerprints: Vec::new(),
            cdp_port: None,
            cdp_online: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryReport {
    pub message: String,
    /// 本次检测到客户端、target 或已有配置意图变化；首次建立基线不算更新。
    pub changed: bool,
    pub needs_restore: bool,
    /// 仅显式恢复且 apply 与随后 health 均确认成功才为 true。
    pub restored: bool,
}

#[derive(Debug)]
pub struct RecoveryManager {
    data_dir: PathBuf,
}

#[derive(Serialize)]
struct Transition<'a> {
    schema_version: u32,
    observed_at: u64,
    before: &'a ClientStamp,
    after: &'a ClientStamp,
    wallpaper_settings: &'a WallpaperSettings,
}

/// 只检查传入 exe 旁的 resources/app.asar，不搜索安装目录或启动任何进程。
/// 首次计算 SHA-256；后续仅在文件路径、长度或修改时间改变时重新计算。
pub fn inspect(exe: &Path) -> Result<ClientStamp, String> {
    inspect_with_previous(exe, None)
}

fn inspect_with_previous(exe: &Path, previous: Option<&ClientStamp>) -> Result<ClientStamp, String> {
    let exe_path = fs::canonicalize(exe).map_err(|_| "无法定位传入的 ZCode 可执行文件。")?;
    if !exe_path.is_file() {
        return Err("传入的 ZCode 可执行文件不是普通文件。".into());
    }
    let root = exe_path.parent().ok_or("ZCode 可执行文件没有安装目录。")?;
    let app_asar_path = fs::canonicalize(root.join("resources/app.asar"))
        .map_err(|_| "无法定位 ZCode 的 resources/app.asar；保留原恢复状态。")?;
    let mut file = File::open(&app_asar_path).map_err(|_| "无法只读打开 ZCode 的 app.asar。")?;
    let metadata = file.metadata().map_err(|_| "无法读取 app.asar 元数据。")?;
    if !metadata.is_file() {
        return Err("ZCode 的 app.asar 不是普通文件。".into());
    }
    let len = metadata.len();
    let modified = modified(&metadata);
    let cache = STAMPS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut cache = cache.lock().map_err(|_| "客户端指纹缓存不可用。")?;
    let cached = cache.get(&app_asar_path).or(previous).filter(|old| {
        old.app_asar_path == app_asar_path
            && old.len == len
            && modified.is_some()
            && old.modified == modified
            && valid_hash(&old.hash)
    });
    let hash = if let Some(old) = cached {
        old.hash.clone()
    } else {
        let mut hasher = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(|_| "读取 app.asar 指纹失败，稍后重试。")?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        // 更新器可能在读取期间替换文件；不将混合内容作为新基线。
        let after = fs::metadata(&app_asar_path).map_err(|_| "app.asar 正在更新，稍后重试。")?;
        let opened_after = file.metadata().map_err(|_| "app.asar 正在更新，稍后重试。")?;
        if after.len() != len || modified_of_pair(&after, &opened_after) != (modified, modified) {
            return Err("app.asar 正在更新，稍后重试。".into());
        }
        format!("{:x}", hasher.finalize())
    };
    let version = package_version(root);
    let stamp = ClientStamp {
        exe_path,
        app_asar_path: app_asar_path.clone(),
        version,
        len,
        modified,
        hash,
    };
    // 缓存只是一项优化，限制数量避免路径长期迁移导致无界增长。
    if cache.len() >= 16 {
        cache.clear();
    }
    cache.insert(app_asar_path, stamp.clone());
    Ok(stamp)
}

fn modified(metadata: &fs::Metadata) -> Option<u64> {
    metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos().try_into().ok()
}

fn modified_of_pair(a: &fs::Metadata, b: &fs::Metadata) -> (Option<u64>, Option<u64>) {
    (modified(a), modified(b))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let file = File::open(path).map_err(|_| "无法读取恢复记录，原文件保留。")?;
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)
        .map_err(|_| "读取恢复记录失败，原文件保留。")?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err("恢复记录过大，原文件保留。".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "恢复记录格式无效，原文件保留。".into())
}

fn package_version(root: &Path) -> Option<String> {
    // 只读已展开的固定清单；若版本只在 asar 内则保持 None，不解包或改写官方包。
    for path in [root.join("resources/app/package.json"), root.join("resources/package.json"), root.join("package.json")] {
        if let Ok(value) = read_json::<serde_json::Value>(&path) {
            if let Some(version) = value.get("version").and_then(serde_json::Value::as_str) {
                if !version.is_empty() && version.len() <= 128 && !version.chars().any(char::is_control) {
                    return Some(version.to_owned());
                }
            }
        }
    }
    None
}

impl RecoveryManager {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into() }
    }

    /// 只读现场，持久记录待恢复；主程序自行按自动恢复开关调用 restore 或 health/apply。
    /// exe=None 表示暂未发现客户端，不清除已有指纹或 pending。
    pub fn observe(&self, exe: Option<PathBuf>, wallpaper_settings: &WallpaperSettings) -> Result<RecoveryReport, String> {
        let _guard = OPERATION.lock().map_err(|_| "恢复操作锁不可用。")?;
        let settings = wallpaper_settings.normalized();
        let current = load_state(&self.data_dir)?;
        let client = exe.as_deref().map(|path| inspect_with_previous(path, current.client.as_ref()));
        let targets = list_target_fingerprints(settings.port);
        observe_inner(&self.data_dir, current, client, &settings, targets)
    }
}

fn load_state(data_dir: &Path) -> Result<RecoveryState, String> {
    let path = data_dir.join(STATE_FILE);
    match fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RecoveryState::default()),
        Err(_) => Err("无法读取恢复记录，原文件保留。".into()),
        Ok(_) => {
            let state: RecoveryState = read_json(&path)?;
            if state.schema_version != SCHEMA_VERSION {
                return Err("恢复记录版本不受支持，原文件保留。".into());
            }
            Ok(state)
        }
    }
}

fn observe_inner(
    data_dir: &Path,
    current: RecoveryState,
    inspected: Option<Result<ClientStamp, String>>,
    settings: &WallpaperSettings,
    targets: Result<Vec<String>, String>,
) -> Result<RecoveryReport, String> {
    let mut next = current.clone();
    let mut changed = false;
    let mut reasons = Vec::new();
    let mut warning = None;
    if let Some(result) = inspected {
        match result {
            Ok(client) => {
                if let Some(before) = &current.client {
                    if before != &client {
                        // 备份成功才推进基线；不保存官方包内容或任何身份凭据。
                        backup_transition(data_dir, before, &client, settings)?;
                        changed = true;
                        reasons.push("检测到客户端包指纹或安装路径变化");
                    }
                }
                next.client = Some(client);
            }
            Err(error) => warning = Some(error),
        }
    }
    let settings_changed = current.wallpaper_settings.as_ref().is_some_and(|old| old != settings);
    if settings_changed {
        changed = true;
        reasons.push("自定义视觉配置已变化");
        // 包含关闭自定义的操作，允许主程序显式恢复默认外观。
        next.pending = true;
    }
    next.wallpaper_settings = Some(settings.clone());
    match targets {
        Ok(mut fingerprints) if !fingerprints.is_empty() => {
            fingerprints.sort();
            fingerprints.dedup();
            if !current.target_fingerprints.is_empty()
                && (current.target_fingerprints != fingerprints || current.cdp_port != Some(settings.port))
            {
                changed = true;
                reasons.push("检测到客户端 renderer target 变化");
            }
            next.target_fingerprints = fingerprints;
            next.cdp_port = Some(settings.port);
            next.cdp_online = true;
        }
        _ => {
            next.cdp_online = false;
            // 离线保留上次 target；重连时仍可识别客户端重启。
            if settings.enabled || current.pending {
                next.pending = true;
            }
            reasons.push("本机 CDP 离线或尚无 ZCode 窗口，等待重试");
        }
    }
    if changed && settings.enabled {
        next.pending = true;
    }
    if next != current {
        crate::storage::json(&data_dir.join(STATE_FILE), &next)?;
    }
    let mut message = if reasons.is_empty() {
        if current.client.is_none() && next.client.is_some() {
            "已建立客户端指纹基线，首次记录不视为更新。".to_string()
        } else if next.pending {
            "自定义视觉恢复仍待完成。".to_string()
        } else {
            "未检测到客户端更新或 renderer target 变化。".to_string()
        }
    } else {
        format!("{}。", reasons.join("；"))
    };
    if let Some(warning) = warning {
        message.push_str(&warning);
    }
    if next.pending && next.cdp_online {
        message.push_str("等待主程序按自动恢复开关重新应用原生视觉 overlay。");
    }
    Ok(RecoveryReport { message, changed, needs_restore: next.pending, restored: false })
}

fn backup_transition(data_dir: &Path, before: &ClientStamp, after: &ClientStamp, settings: &WallpaperSettings) -> Result<(), String> {
    // 内容寻址便于保存失败后重试，避免每个 tick 为相同更新重复产生备份。
    let key = serde_json::to_vec(&(before, after)).map_err(|_| "无法编码客户端指纹。")?;
    let path = data_dir.join("backups").join(format!("native-client-{:x}.json", Sha256::digest(key)));
    if path.try_exists().map_err(|_| "无法检查客户端指纹备份。")? {
        return Ok(());
    }
    let transition = Transition {
        schema_version: SCHEMA_VERSION,
        observed_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        before,
        after,
        wallpaper_settings: settings,
    };
    crate::storage::json(&path, &transition)
}

#[derive(Deserialize)]
struct Target {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    websocket: String,
}

fn list_target_fingerprints(port: u16) -> Result<Vec<String>, String> {
    let client = Client::builder().no_proxy().redirect(Policy::none())
        .connect_timeout(Duration::from_secs(2)).timeout(Duration::from_secs(2))
        .build().map_err(|_| "无法初始化本机 CDP 检查。")?;
    let response = client.get(format!("http://127.0.0.1:{port}/json/list"))
        .send().map_err(|_| "本机 CDP 离线。")?;
    if !response.status().is_success() {
        return Err("本机 CDP 页面列表不可用。".into());
    }
    let mut bytes = Vec::new();
    response.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)
        .map_err(|_| "无法读取本机 CDP 页面列表。")?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err("本机 CDP 页面列表过大。".into());
    }
    target_fingerprints(&bytes, port)
}

fn target_fingerprints(bytes: &[u8], port: u16) -> Result<Vec<String>, String> {
    let targets: Vec<Target> = serde_json::from_slice(bytes).map_err(|_| "本机 CDP 页面列表无效。")?;
    let mut fingerprints = Vec::new();
    for target in targets {
        if !wallpaper::is_zcode_renderer(&target.kind, &target.title, &target.url) {
            continue;
        }
        let url = Url::parse(&target.websocket).map_err(|_| "CDP 页面地址无效。")?;
        let local = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        });
        if url.scheme() != "ws" || !local || url.port_or_known_default() != Some(port)
            || !url.username().is_empty() || url.password().is_some() || url.query().is_some()
            || url.fragment().is_some() || !url.path().starts_with("/devtools/page/")
            || url.path() == "/devtools/page/"
        {
            return Err("CDP 页面地址不属于当前本机调试端口。".into());
        }
        // host 别名不改变 target 身份；排序消除 /json/list 顺序变化的误报。
        fingerprints.push(format!("{:x}", Sha256::digest(format!("{port}:{}", url.path()))));
    }
    fingerprints.sort();
    fingerprints.dedup();
    if fingerprints.is_empty() || fingerprints.len() > 32 {
        return Err("本机 CDP 尚无 ZCode 窗口或窗口数量异常。".into());
    }
    Ok(fingerprints)
}

/// 手动入口或主程序自动恢复入口：只恢复自有视觉，不执行安装目录修复。
/// pending 先原子落盘；CDP 离线、部分失败或健康检查失败均返回待重试报告。
pub fn restore(exe: Option<&Path>, settings: &WallpaperSettings, data_dir: &Path) -> Result<RecoveryReport, String> {
    let _guard = OPERATION.lock().map_err(|_| "恢复操作锁不可用。")?;
    let settings = settings.normalized();
    let current = load_state(data_dir)?;
    let inspected = exe.map(|path| inspect_with_previous(path, current.client.as_ref()));
    let mut report = observe_inner(data_dir, current, inspected, &settings, list_target_fingerprints(settings.port))?;
    let mut state = load_state(data_dir)?;
    state.pending = true;
    crate::storage::json(&data_dir.join(STATE_FILE), &state)?;
    let outcome = wallpaper::apply(&settings).and_then(|applied| {
        if applied.windows == 0 {
            return Err("原生恢复未确认任何 ZCode 窗口。".into());
        }
        let health = wallpaper::health(&settings)?;
        if !health.healthy || health.needs_apply || health.windows == 0 {
            return Err("原生视觉健康检查尚未通过。".into());
        }
        // 应用期间出现新窗口或重启，不把旧 target 上的成功当作全局成功。
        let targets = list_target_fingerprints(settings.port)?;
        if !state.target_fingerprints.is_empty() && targets != state.target_fingerprints {
            return Err("恢复期间 renderer target 已变化，等待再次恢复。".into());
        }
        Ok((applied, targets))
    });
    match outcome {
        Ok((applied, targets)) => {
            state.pending = false;
            state.target_fingerprints = targets;
            state.cdp_port = Some(settings.port);
            state.cdp_online = true;
            crate::storage::json(&data_dir.join(STATE_FILE), &state)?;
            report.changed |= !applied.reused;
            report.needs_restore = false;
            report.restored = true;
            report.message = format!("已确认 {} 个窗口的原生视觉恢复。{}", applied.windows, applied.message);
        }
        Err(_) => {
            // 不持久化底层错误，防止未来 API 将脚本、路径或页面敏感内容混入记录。
            report.needs_restore = true;
            report.restored = false;
            report.message = "原生视觉恢复尚未完成（CDP 离线、图片不可用或窗口状态未通过检查）；已保存待恢复意图，下次重试。".into();
        }
    }
    Ok(report)
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
