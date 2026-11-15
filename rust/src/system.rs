//! 独立系统集成，不依赖 main/settings/UI，也不修改 ZCode 客户端。
//!
//! main 在单实例锁、日志与 UI 初始化前调用 `handle_internal_command()`；返回 true
//! 就退出 main。重启/更新 helper 需要这个入口。普通启动只查询，不修改自启。
//! windows 0.58 额外 features：Win32_System_Registry, Win32_System_Com,
//! Win32_System_Environment, Win32_UI_Shell, Win32_UI_Shell_Common,
//! Win32_UI_Controls_Dialogs, Win32_Storage_FileSystem。无 Python/rfd/PowerShell。

use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Result<T> = std::result::Result<T, String>;
pub const AUTOSTART_VALUE: &str = "zcode-token-bar-rs";
pub const LEGACY_AUTOSTART_VALUE: &str = "ZCodeTokenBar";
pub const GITHUB_URL: &str = "https://github.com/suiren0219/zcode-token-bar";
const RESTART_FLAG: &str = "--token-bar-internal-restart";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub exe: PathBuf,
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub config_file: PathBuf,
    pub backup_dir: PathBuf,
    pub update_dir: PathBuf,
}

impl AppPaths {
    /// 只读发现路径。环境变量 > 有限祖先 data-dir.txt > portable 标记 >
    /// 有应用文件证据的旧目录 > LocalAppData。不会默默写入 target/release。
    pub fn discover() -> Result<Self> {
        let exe = std::env::current_exe().map_err(err)?;
        let override_dir = nonempty_env("ZCODE_TOKEN_BAR_DATA")
            .or_else(|| nonempty_env("TOKEN_BAR_DATA_DIR"));
        let local = nonempty_env("LOCALAPPDATA").or_else(|| {
            nonempty_env("USERPROFILE").map(|p| p.join("AppData").join("Local"))
        });
        let mut legacy = Vec::new();
        if let Some(home) = nonempty_env("ZCODE_HOME") {
            legacy.push(home.join("token-bar-data"));
        }
        if let Some(base) = exe.parent() {
            for ancestor in base.ancestors().take(6).filter(|p| p.parent().is_some()) {
                legacy.push(ancestor.join("token-bar-data"));
                legacy.push(ancestor.join("zcode-token-bar-data"));
                legacy.push(ancestor.join("zcode-token-bar"));
                legacy.push(ancestor.to_path_buf());
            }
            if let Some(root) = base.ancestors().last() {
                legacy.push(root.join("zcode-token-bar-data"));
            }
        }
        Self::discover_with_base(&exe, override_dir.as_deref(), local.as_deref(), &legacy)
    }

    /// 注入路径而非修改进程环境，便于纯临时目录测试。
    pub fn discover_with_base(
        exe: &Path,
        override_dir: Option<&Path>,
        local_app_data: Option<&Path>,
        legacy_dirs: &[PathBuf],
    ) -> Result<Self> {
        if !exe.is_absolute() { return Err("exe 路径必须为绝对路径".into()); }
        let base = exe.parent().ok_or("exe 没有父目录")?;
        if let Some(path) = override_dir.filter(|p| !p.as_os_str().is_empty()) {
            return Ok(Self::from_root(exe, absolute_from(base, path)));
        }
        for ancestor in base.ancestors().take(6).filter(|p| p.parent().is_some()) {
            if let Some(root) = read_data_marker(ancestor)? { return Ok(Self::from_root(exe, root)); }
        }
        if base.join("portable").is_file() || base.join("portable.flag").is_file() {
            return Ok(Self::from_root(exe, base.join("data")));
        }
        for dir in legacy_dirs {
            // Python 的 data-dir.txt 位于脚本目录，可能是 Rust 仓库的兄弟目录。
            if let Some(root) = read_data_marker(dir)? {
                return Ok(Self::from_root(exe, root));
            }
            if ["settings-rs.json", "hoverbar.json", "hoverbar.log", "zusage-hoverbar-dump.json", "zusage-hoverbar.lock"]
                .iter().any(|name| dir.join(name).is_file()) {
                return Ok(Self::from_root(exe, absolute_from(base, dir)));
            }
        }
        let local = local_app_data.ok_or("找不到 LocalAppData；请设置 ZCODE_TOKEN_BAR_DATA")?;
        Ok(Self::from_root(exe, absolute_from(base, local).join("ZCodeTokenBar")))
    }

    pub fn from_root(exe: &Path, root: PathBuf) -> Self {
        Self {
            exe: exe.to_path_buf(), logs_dir: root.join("logs"),
            config_file: root.join("settings-rs.json"),
            backup_dir: root.join("backups"), update_dir: root.join("updates"), data_dir: root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for path in [&self.data_dir, &self.logs_dir, &self.backup_dir, &self.update_dir] {
            fs::create_dir_all(path).map_err(|e| format!("创建 {} 失败：{e}", path.display()))?;
        }
        Ok(())
    }

    /// 可选的通用 JSON 首次迁移。已有目标绝不覆盖，源文件不移动；主 settings
    /// 模块若需字段转换可直接使用自己的迁移，不必调用本函数。
    pub fn initialize(&self, legacy_dirs: &[PathBuf]) -> Result<MigrationReport> {
        self.ensure_dirs()?;
        if self.config_file.exists() { return Ok(MigrationReport::default()); }
        for dir in legacy_dirs {
            for name in ["settings-rs.json", "hoverbar.json"] {
                let source = dir.join(name);
                if !source.is_file() || paths_equal(&source, &self.config_file) { continue; }
                let bytes = read_limited(&source, 4 * 1024 * 1024)?;
                let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(err)?;
                if !json.is_object() { return Err("旧设置不是 JSON 对象，未迁移".into()); }
                let backup = self.backup_dir.join(format!("settings-{}.json", unique_suffix()));
                write_new(&backup, &bytes)?;
                publish_new(&self.config_file, &bytes)?;
                return Ok(MigrationReport { source: Some(source), backup: Some(backup) });
            }
        }
        Ok(MigrationReport::default())
    }
}

#[derive(Debug, Default)]
pub struct MigrationReport { pub source: Option<PathBuf>, pub backup: Option<PathBuf> }

fn nonempty_env(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    match value.to_str() {
        Some(text) => (!text.trim().is_empty()).then(|| PathBuf::from(text.trim())),
        None => (!value.is_empty()).then(|| PathBuf::from(value)),
    }
}
fn read_data_marker(base: &Path) -> Result<Option<PathBuf>> {
    let marker = base.join("data-dir.txt");
    if !marker.is_file() { return Ok(None); }
    let bytes = read_limited(&marker, 8192)?;
    let text = std::str::from_utf8(&bytes).map_err(err)?.trim_start_matches('\u{feff}').trim();
    if text.is_empty() || text.contains(['\n', '\r', '\0']) {
        return Err(format!("数据目录标记无效：{}", marker.display()));
    }
    Ok(Some(absolute_from(base, Path::new(text))))
}
fn absolute_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}
fn err(e: impl std::fmt::Display) -> String { e.to_string() }

pub(crate) fn unique_suffix() -> String {
    format!("{}-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
        SERIAL.fetch_add(1, Ordering::Relaxed))
}
pub(crate) fn new_work_dir(parent: &Path, prefix: &str) -> Result<PathBuf> {
    fs::create_dir_all(parent).map_err(err)?;
    let dir = parent.join(format!("{prefix}-{}", unique_suffix()));
    fs::create_dir(&dir).map_err(err)?;
    fs::canonicalize(dir).map_err(err)
}
pub(crate) fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(err)?;
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data).map_err(err)?;
    if data.len() as u64 > limit { return Err("文件超过大小限制".into()); }
    Ok(data)
}
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(err)?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(err)
}
/// 同卷临时文件完整落盘后，以硬链接原子发布；目标存在时不会覆盖。
fn publish_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", unique_suffix()));
    write_new(&tmp, bytes)?;
    let result = fs::hard_link(&tmp, path).map_err(err);
    let _ = fs::remove_file(tmp);
    result
}
pub(crate) fn paths_equal(left: &Path, right: &Path) -> bool {
    fn norm(path: &Path) -> String {
        let p = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let text = p.to_string_lossy().replace('/', "\\");
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if cfg!(windows) { text.to_lowercase() } else { text.to_owned() }
    }
    norm(left) == norm(right)
}

/// Windows CommandLineToArgvW/CRT 的引号规则；不经过 cmd.exe，特殊字符不会展开。
pub fn quote_windows_arg(value: &str) -> Result<String> {
    if value.contains('\0') { return Err("参数包含 NUL".into()); }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' { backslashes += 1; continue; }
        out.extend(std::iter::repeat_n('\\', backslashes * if ch == '"' { 2 } else { 1 }));
        if ch == '"' { out.push('\\'); }
        out.push(ch);
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    Ok(out)
}
pub fn windows_arguments(args: &[String]) -> Result<String> {
    args.iter().map(|s| quote_windows_arg(s)).collect::<Result<Vec<_>>>().map(|v| v.join(" "))
}
fn path_text(path: &Path) -> Result<&str> { path.to_str().ok_or_else(|| "路径不能转换为 Unicode".into()) }

/// 只用于识别已知应用命令，不运行它。未闭合引号和 NUL 均拒绝。
pub fn split_windows_command(command: &str) -> Result<Vec<String>> {
    if command.contains('\0') { return Err("命令包含 NUL".into()); }
    let chars: Vec<char> = command.chars().collect();
    let (mut i, mut args) = (0, Vec::new());
    while i < chars.len() {
        while i < chars.len() && matches!(chars[i], ' ' | '\t') { i += 1; }
        if i == chars.len() { break; }
        let (mut arg, mut quoted) = (String::new(), false);
        while i < chars.len() {
            if !quoted && matches!(chars[i], ' ' | '\t') { break; }
            let mut slashes = 0;
            while i < chars.len() && chars[i] == '\\' { slashes += 1; i += 1; }
            if i < chars.len() && chars[i] == '"' {
                arg.extend(std::iter::repeat_n('\\', slashes / 2));
                if slashes % 2 != 0 { arg.push('"'); }
                else if quoted && chars.get(i + 1) == Some(&'"') { arg.push('"'); i += 1; }
                else { quoted = !quoted; }
                i += 1;
            } else {
                arg.extend(std::iter::repeat_n('\\', slashes));
                if i == chars.len() || (!quoted && matches!(chars[i], ' ' | '\t')) { break; }
                arg.push(chars[i]); i += 1;
            }
        }
        if quoted { return Err("命令引号未闭合".into()); }
        args.push(arg);
    }
    Ok(args)
}

#[derive(Clone, Debug, Default)]
pub struct LegacyPolicy {
    /// 必须是调用方已确认的 hoverbar.py 绝对路径。默认不猜测 Python 脚本归属。
    pub python_scripts: Vec<PathBuf>,
    /// 应用搬迁后可明确提供旧 Rust exe，按完整路径识别，不按名字抢占 Run 值。
    pub rust_executables: Vec<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutostartKind { Rust, LegacyPython, Unknown }
#[derive(Clone, Debug)]
pub struct AutostartEntry {
    pub name: String, pub command: Option<String>, pub kind: AutostartKind,
    pub target_exists: bool,
}
#[derive(Clone, Debug, Default)]
pub struct AutostartStatus {
    pub enabled: bool, pub legacy_enabled: bool, pub entries: Vec<AutostartEntry>,
}
#[derive(Clone, Debug)]
pub struct AutostartChange { pub status: AutostartStatus, pub backup: Option<PathBuf> }

pub fn classify_autostart(command: &str, exe: &Path, policy: &LegacyPolicy) -> AutostartKind {
    let Ok(args) = split_windows_command(command) else { return AutostartKind::Unknown; };
    let Some(first) = args.first() else { return AutostartKind::Unknown; };
    if args.iter().any(|a| a.starts_with("--token-bar-internal-")) { return AutostartKind::Unknown; }
    let target = Path::new(first);
    if paths_equal(target, exe) || policy.rust_executables.iter().any(|p| paths_equal(p, target)) {
        return AutostartKind::Rust;
    }
    let name = first.rsplit(['/', '\\']).next().unwrap_or("").to_ascii_lowercase();
    if !matches!(name.as_str(), "python.exe" | "pythonw.exe" | "python3.exe" | "pythonw3.exe")
        || !(args.len() == 2 || (args.len() == 3 && args[2] == "--wait-zcode")) {
        return AutostartKind::Unknown;
    }
    let script = Path::new(&args[1]);
    if policy.python_scripts.iter().any(|p| paths_equal(p, script))
        && args[1].rsplit(['/', '\\']).next().unwrap_or("").eq_ignore_ascii_case("hoverbar.py") {
        AutostartKind::LegacyPython
    } else { AutostartKind::Unknown }
}

pub fn autostart_status(paths: &AppPaths, policy: &LegacyPolicy) -> Result<AutostartStatus> {
    platform::autostart_status(paths, policy)
}
/// 只读/写两个明确的本应用 Run 值；未知同名命令直接报错。先备份原始 bytes/type，
/// 再写 Rust 值、移除已识别 Python 值，失败时尝试按比较后的原值回滚。
pub fn set_autostart(enabled: bool, args: &[String], paths: &AppPaths, policy: &LegacyPolicy)
    -> Result<AutostartChange> {
    validate_restart_args(args)?;
    platform::set_autostart(enabled, args, paths, policy)
}

#[derive(Clone, Debug)]
pub struct FontChoice {
    pub family: String,
    /// 逻辑像素，96 DPI 基准；不把 ChooseFont 的十分之一点当像素。
    pub size_px: i32, pub weight: i32, pub italic: bool,
}
impl Default for FontChoice {
    fn default() -> Self { Self { family: "Segoe UI".into(), size_px: 13, weight: 400, italic: false } }
}
/// 取消返回 Ok(None)。在 UI 线程调用；优先当前 STA，已有 MTA 时用消息泵兼容。
pub fn pick_image(owner: isize, initial_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    platform::pick_file(owner, initial_dir, false)
}
/// 原生 JSON 配置选择。这里只返回路径，不读取或记录配置中的凭据。
pub fn pick_config(owner: isize, initial_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    platform::pick_file(owner, initial_dir, true)
}
pub fn choose_font(owner: isize, initial: &FontChoice) -> Result<Option<FontChoice>> {
    platform::choose_font(owner, initial)
}
pub fn desktop_dir() -> Result<PathBuf> { platform::desktop_dir() }

#[derive(Clone, Debug)]
pub struct LauncherInfo { pub path: PathBuf, pub target: PathBuf, pub arguments: String }
#[derive(Debug, Default)]
pub struct LauncherRepairReport {
    pub repaired: Vec<PathBuf>, pub unchanged: Vec<PathBuf>,
    pub backups: Vec<PathBuf>, pub errors: Vec<String>,
}
/// 只枚举系统已知的桌面/开始菜单，不扫描磁盘、不 resolve 快捷方式、不启动 ZCode。
pub fn inspect_zcode_launchers(zcode_exe: &Path) -> Result<Vec<LauncherInfo>> {
    validate_zcode_exe(zcode_exe)?;
    platform::inspect_launchers(zcode_exe)
}
/// 仅用户修复按钮调用。完整路径严格匹配已有 ZCode.exe lnk，先备份，保留其它
/// 参数/图标/工作目录；不新增入口、不改安装目录、不重启 ZCode。
pub fn repair_zcode_launchers(zcode_exe: &Path, port: u16, paths: &AppPaths) -> Result<LauncherRepairReport> {
    validate_zcode_exe(zcode_exe)?;
    if port == 0 { return Err("调试端口必须在 1–65535 之间".into()); }
    platform::repair_launchers(zcode_exe, port, paths)
}
fn validate_zcode_exe(path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.is_file()
        || !path.file_name().and_then(OsStr::to_str).is_some_and(|n| n.eq_ignore_ascii_case("ZCode.exe")) {
        return Err("需要已确认存在的 ZCode.exe 完整路径".into());
    }
    Ok(())
}
/// 纯参数转换；不扩大监听地址、不添加允许远端访问的参数。
pub fn launcher_arguments_with_port(arguments: &str, port: u16) -> Result<String> {
    if port == 0 { return Err("端口不能为 0".into()); }
    let args = split_windows_command(arguments)?;
    let mut result = Vec::new(); let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" { result.extend_from_slice(&args[i..]); break; }
        if arg == "--remote-debugging-port" {
            if args.get(i + 1).is_some_and(|s| !s.starts_with('-')) { i += 1; }
        } else if !arg.starts_with("--remote-debugging-port=") {
            result.push(arg.clone());
        }
        i += 1;
    }
    let insert = result.iter().position(|s| s == "--").unwrap_or(result.len());
    result.insert(insert, format!("--remote-debugging-port={port}"));
    windows_arguments(&result)
}

#[derive(Clone, Debug)]
pub struct ShortcutOptions {
    /// 不含 .lnk 的单个文件名；已有快捷方式不会覆盖。
    pub name: String, pub args: Vec<String>, pub icon: Option<PathBuf>, pub icon_index: i32,
}
impl Default for ShortcutOptions {
    fn default() -> Self {
        Self { name: "ZCode Token 悬浮条".into(), args: Vec::new(), icon: None, icon_index: 0 }
    }
}
pub fn create_desktop_shortcut(paths: &AppPaths, options: &ShortcutOptions) -> Result<PathBuf> {
    validate_component(&options.name)?;
    validate_restart_args(&options.args)?;
    platform::create_shortcut(paths, options)
}
fn validate_component(name: &str) -> Result<()> {
    let reserved = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if name.is_empty() || name.ends_with(['.', ' ']) || name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c))
        || matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || ((reserved.starts_with("COM") || reserved.starts_with("LPT"))
            && matches!(&reserved[3..], "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")) {
        return Err("无效的文件名".into());
    }
    Ok(())
}
pub fn open_data_dir(paths: &AppPaths) -> Result<()> { paths.ensure_dirs()?; open_path(&paths.data_dir) }
pub fn open_logs(paths: &AppPaths) -> Result<()> { paths.ensure_dirs()?; open_path(&paths.logs_dir) }
pub fn open_config(paths: &AppPaths) -> Result<()> {
    if paths.config_file.is_file() { open_path(&paths.config_file) } else { open_data_dir(paths) }
}
pub fn open_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.exists() { return Err("要打开的路径不存在或不是绝对路径".into()); }
    platform::shell_open(path.as_os_str())
}
pub fn open_github() -> Result<()> { open_github_page(GITHUB_URL) }
pub fn open_github_page(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(err)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com")
        || parsed.port().is_some() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("仅允许打开 HTTPS GitHub 页面".into());
    }
    platform::shell_open(OsStr::new(parsed.as_str()))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity { pub pid: u32, pub created: u64 }
#[derive(Serialize, Deserialize)]
struct RestartRequest {
    schema: u32, parent: ProcessIdentity, exe: PathBuf, args: Vec<String>, data_dir: PathBuf,
}
/// 成功后由 main 正常退出。helper 先验证父进程身份并等待退出，避免抢单实例锁。
pub fn schedule_restart(args: &[String], paths: &AppPaths) -> Result<u32> {
    validate_restart_args(args)?;
    let exe = fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?;
    if !paths_equal(&exe, &paths.exe) { return Err("只能重启当前 exe".into()); }
    let dir = new_work_dir(&paths.update_dir, "restart")?;
    let file = dir.join("restart.json");
    let request = RestartRequest { schema: 1, parent: current_process_identity()?, exe: exe.clone(),
        args: args.to_vec(), data_dir: paths.data_dir.clone() };
    write_new(&file, &serde_json::to_vec(&request).map_err(err)?)?;
    spawn_hidden(&exe, &[OsString::from(RESTART_FLAG), file.into_os_string()], &paths.data_dir)
}
/// 必须在 main 最早执行。true 表示内部命令已处理，调用方应 return。
pub fn handle_internal_command() -> Result<bool> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().and_then(|s| s.to_str()) != Some(RESTART_FLAG) { return Ok(false); }
    if args.len() != 2 { return Err("重启 helper 参数错误".into()); }
    let path = PathBuf::from(&args[1]);
    let request: RestartRequest = serde_json::from_slice(&read_limited(&path, 128 * 1024)?).map_err(err)?;
    if request.schema != 1 || !paths_equal(&request.exe, &std::env::current_exe().map_err(err)?)
        || !request.data_dir.is_absolute() { return Err("重启请求与当前 exe 不匹配".into()); }
    validate_restart_args(&request.args)?;
    wait_for_process_exit(&request.parent, &request.exe, Duration::from_secs(120))?;
    spawn_hidden(&request.exe, &request.args.iter().map(OsString::from).collect::<Vec<_>>(), &request.data_dir)?;
    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() { let _ = fs::remove_dir(parent); }
    Ok(true)
}
pub(crate) fn validate_restart_args(args: &[String]) -> Result<()> {
    if args.iter().any(|s| s.contains('\0') || s.starts_with("--token-bar-internal-")) {
        Err("重启参数包含内部命令或 NUL".into())
    } else { Ok(()) }
}
pub(crate) fn spawn_hidden(program: &Path, args: &[OsString], data_dir: &Path) -> Result<u32> {
    let mut cmd = Command::new(program);
    cmd.args(args).env("ZCODE_TOKEN_BAR_DATA", data_dir)
        .current_dir(program.parent().ok_or("exe 没有父目录")?)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW，不与 DETACHED_PROCESS 混用。
    }
    cmd.spawn().map(|child| child.id()).map_err(err)
}
pub(crate) fn current_process_identity() -> Result<ProcessIdentity> { platform::current_process_identity() }
pub(crate) fn wait_for_process_exit(parent: &ProcessIdentity, exe: &Path, timeout: Duration) -> Result<()> {
    platform::wait_for_process_exit(parent, exe, timeout)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::{Interface, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
        ERROR_INVALID_PARAMETER, ERROR_SUCCESS, FILETIME, HANDLE, HWND, WAIT_OBJECT_0};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Registry::*;
    use windows::Win32::UI::Shell::*;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(value: &OsStr) -> Result<Vec<u16>> {
        let mut words: Vec<u16> = value.encode_wide().collect();
        if words.contains(&0) { return Err("字符串包含 NUL".into()); }
        words.push(0); Ok(words)
    }
    fn wstr(value: &str) -> Result<Vec<u16>> { wide(OsStr::new(value)) }
    struct Com;
    impl Com {
        fn new() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(err)?; }
            Ok(Self)
        }
    }
    impl Drop for Com { fn drop(&mut self) { unsafe { CoUninitialize() } } }
    fn sta<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
        // UI 通常已是 STA，直接在 owner 的线程显示，避免 Show -> SendMessage
        // 等待 owner 而 owner 又 join 对话框线程的死锁。
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if initialized.is_ok() { let _com = Com; return f(); }
        if initialized != windows::core::HRESULT(0x80010106u32 as i32) {
            return Err(err(initialized));
        }
        // 已有 MTA 时使用独立 STA，并在 owner 线程泵送消息。
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MsgWaitForMultipleObjectsEx,
            PeekMessageW, PostQuitMessage, TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, WM_QUIT};
        let worker = std::thread::spawn(move || { let _com = Com::new()?; f() });
        let mut quit = None;
        while !worker.is_finished() {
            unsafe {
                let _ = MsgWaitForMultipleObjectsEx(None, 20, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                let mut message = MSG::default();
                while PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT { quit = Some(message.wParam.0 as i32); continue; }
                    let _ = TranslateMessage(&message); DispatchMessageW(&message);
                }
            }
        }
        let result = worker.join().map_err(|_| "COM 线程异常退出".to_string())?;
        if let Some(code) = quit { unsafe { PostQuitMessage(code); } }
        result
    }
    unsafe fn take_com_path(ptr: PWSTR) -> PathBuf {
        let path = PathBuf::from(OsString::from_wide(ptr.as_wide()));
        CoTaskMemFree(Some(ptr.0.cast())); path
    }
    pub(super) fn desktop_dir() -> Result<PathBuf> {
        unsafe {
            let ptr = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, HANDLE::default()).map_err(err)?;
            Ok(take_com_path(ptr))
        }
    }
    pub(super) fn pick_file(owner: isize, initial: Option<&Path>, json: bool) -> Result<Option<PathBuf>> {
        let initial = initial.map(Path::to_path_buf);
        sta(move || unsafe {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(err)?;
            dialog.SetOptions(dialog.GetOptions().map_err(err)? | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST
                | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR | FOS_DONTADDTORECENT).map_err(err)?;
            let label = wstr(if json { "JSON 配置" } else { "图片" })?;
            let pattern = wstr(if json { "*.json" } else { "*.png;*.jpg;*.jpeg;*.webp;*.bmp;*.gif;*.ico" })?;
            dialog.SetFileTypes(&[Common::COMDLG_FILTERSPEC {
                pszName: PCWSTR(label.as_ptr()), pszSpec: PCWSTR(pattern.as_ptr()),
            }]).map_err(err)?;
            let title = wstr(if json { "选择 JSON 配置" } else { "选择图片" })?;
            dialog.SetTitle(PCWSTR(title.as_ptr())).map_err(err)?;
            if let Some(folder) = initial.or_else(|| desktop_dir().ok()).filter(|p| p.is_dir()) {
                let path = wide(folder.as_os_str())?;
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None).map_err(err)?;
                dialog.SetFolder(&item).map_err(err)?;
            }
            if let Err(e) = dialog.Show(HWND(owner as *mut _)) {
                if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) { return Ok(None); }
                return Err(err(e));
            }
            let item = dialog.GetResult().map_err(err)?;
            let path = take_com_path(item.GetDisplayName(SIGDN_FILESYSPATH).map_err(err)?);
            let ext = path.extension().and_then(OsStr::to_str).unwrap_or("").to_ascii_lowercase();
            let accepted = if json { ext == "json" } else {
                matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "ico")
            };
            if !path.is_file() || !accepted {
                return Err("所选文件不存在或格式不受支持".into());
            }
            Ok(Some(path))
        })
    }
    pub(super) fn choose_font(owner: isize, initial: &FontChoice) -> Result<Option<FontChoice>> {
        use windows::Win32::Graphics::Gdi::LOGFONTW;
        use windows::Win32::UI::Controls::Dialogs::*;
        if initial.family.contains('\0') { return Err("字体名称包含 NUL".into()); }
        let mut font = LOGFONTW { lfHeight: -initial.size_px.clamp(6, 200),
            lfWeight: initial.weight.clamp(100, 900), lfItalic: u8::from(initial.italic), ..Default::default() };
        for (to, from) in font.lfFaceName.iter_mut().take(31).zip(initial.family.encode_utf16()) { *to = from; }
        let mut options = CHOOSEFONTW { lStructSize: std::mem::size_of::<CHOOSEFONTW>() as u32,
            hwndOwner: HWND(owner as *mut _), lpLogFont: &mut font,
            Flags: CF_SCREENFONTS | CF_INITTOLOGFONTSTRUCT | CF_FORCEFONTEXIST | CF_LIMITSIZE,
            nSizeMin: 5, nSizeMax: 150, ..Default::default() };
        unsafe {
            if !ChooseFontW(&mut options).as_bool() {
                let code = CommDlgExtendedError();
                return if code.0 == 0 { Ok(None) } else { Err(format!("字体选择失败：{}", code.0)) };
            }
        }
        let n = font.lfFaceName.iter().position(|w| *w == 0).unwrap_or(font.lfFaceName.len());
        Ok(Some(FontChoice { family: String::from_utf16_lossy(&font.lfFaceName[..n]),
            size_px: ((options.iPointSize as f64 / 10.0) * 96.0 / 72.0).round() as i32,
            weight: font.lfWeight, italic: font.lfItalic != 0 }))
    }
    pub(super) fn shell_open(value: &OsStr) -> Result<()> {
        let target = wide(value)?; let verb = wstr("open")?;
        let rc = unsafe { ShellExecuteW(HWND::default(), PCWSTR(verb.as_ptr()), PCWSTR(target.as_ptr()),
            PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) }.0 as isize;
        if rc <= 32 { Err(format!("Windows 打开失败：{rc}")) } else { Ok(()) }
    }
    pub(super) fn create_shortcut(paths: &AppPaths, options: &ShortcutOptions) -> Result<PathBuf> {
        let paths = paths.clone(); let options = options.clone();
        sta(move || unsafe {
            let desktop = desktop_dir()?;
            let destination = desktop.join(format!("{}.lnk", options.name));
            if destination.exists() { return Err("同名桌面快捷方式已存在，未覆盖".into()); }
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
            let target = wide(paths.exe.as_os_str())?;
            let args = wstr(&windows_arguments(&options.args)?)?;
            let cwd = wide(paths.exe.parent().ok_or("exe 没有父目录")?.as_os_str())?;
            let icon = wide(options.icon.as_deref().unwrap_or(&paths.exe).as_os_str())?;
            let desc = wstr("ZCode Token 悬浮条（独立 Rust 程序）")?;
            link.SetPath(PCWSTR(target.as_ptr())).map_err(err)?;
            link.SetArguments(PCWSTR(args.as_ptr())).map_err(err)?;
            link.SetWorkingDirectory(PCWSTR(cwd.as_ptr())).map_err(err)?;
            link.SetIconLocation(PCWSTR(icon.as_ptr()), options.icon_index).map_err(err)?;
            link.SetDescription(PCWSTR(desc.as_ptr())).map_err(err)?;
            let persist: IPersistFile = link.cast().map_err(err)?;
            let tmp = desktop.join(format!(".token-bar-{}.lnk", unique_suffix()));
            let tmp_w = wide(tmp.as_os_str())?;
            persist.Save(PCWSTR(tmp_w.as_ptr()), true).map_err(err)?;
            let result = fs::hard_link(&tmp, &destination).map_err(err);
            let _ = fs::remove_file(tmp);
            result.map(|_| destination)
        })
    }

    fn launcher_roots() -> Vec<PathBuf> {
        [&FOLDERID_Desktop, &FOLDERID_PublicDesktop, &FOLDERID_Programs, &FOLDERID_CommonPrograms]
            .into_iter().filter_map(|id| unsafe {
                SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok().map(|p| take_com_path(p))
            }).collect()
    }
    fn launcher_files(root: &Path, depth: usize, found: &mut Vec<PathBuf>, visited: &mut usize) -> Result<()> {
        if depth > 4 || !root.is_dir() { return Ok(()); }
        for entry in fs::read_dir(root).map_err(err)? {
            let entry = entry.map_err(err)?; *visited += 1;
            if *visited > 8192 { return Err("桌面/开始菜单条目过多，停止有限范围枚举".into()); }
            let meta = entry.path().symlink_metadata().map_err(err)?;
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 { continue; } // 不跟随 junction/reparse。
            if meta.is_dir() { launcher_files(&entry.path(), depth + 1, found, visited)?; }
            else if entry.path().extension().and_then(OsStr::to_str).is_some_and(|s| s.eq_ignore_ascii_case("lnk")) {
                found.push(entry.path());
            }
        }
        Ok(())
    }
    unsafe fn load_link(path: &Path) -> Result<(IShellLinkW, LauncherInfo)> {
        use windows::Win32::System::Com::STGM_READ;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        let persist: IPersistFile = link.cast().map_err(err)?;
        let filename = wide(path.as_os_str())?;
        persist.Load(PCWSTR(filename.as_ptr()), STGM_READ).map_err(err)?;
        let mut target = vec![0u16; 32768]; let mut args = vec![0u16; 32768];
        link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).map_err(err)?;
        link.GetArguments(&mut args).map_err(err)?;
        let tn = target.iter().position(|w| *w == 0).ok_or("快捷方式目标过长")?;
        let an = args.iter().position(|w| *w == 0).ok_or("快捷方式参数过长")?;
        Ok((link, LauncherInfo { path: path.to_path_buf(),
            target: PathBuf::from(OsString::from_wide(&target[..tn])),
            arguments: String::from_utf16(&args[..an]).map_err(err)? }))
    }
    fn inspect_launchers_sta(exe: &Path) -> Result<Vec<LauncherInfo>> {
        let mut files = Vec::new(); let mut visited = 0;
        for root in launcher_roots() { launcher_files(&root, 0, &mut files, &mut visited)?; }
        let mut result = Vec::new();
        for file in files {
            if let Ok((_, info)) = unsafe { load_link(&file) } {
                if info.target.is_absolute() && paths_equal(&info.target, exe) { result.push(info); }
            }
        }
        Ok(result)
    }
    pub(super) fn inspect_launchers(exe: &Path) -> Result<Vec<LauncherInfo>> {
        let exe = exe.to_path_buf(); sta(move || inspect_launchers_sta(&exe))
    }
    pub(super) fn repair_launchers(exe: &Path, port: u16, paths: &AppPaths) -> Result<LauncherRepairReport> {
        let exe = exe.to_path_buf(); let paths = paths.clone();
        sta(move || {
            let launchers = inspect_launchers_sta(&exe)?;
            if launchers.is_empty() { return Err("未找到目标匹配的已有 ZCode 桌面/开始菜单快捷方式".into()); }
            let backup_dir = new_work_dir(&paths.backup_dir, "zcode-launchers")?;
            let mut report = LauncherRepairReport::default();
            for (index, info) in launchers.iter().enumerate() {
                let result = (|| -> Result<Option<PathBuf>> {
                    let before = read_limited(&info.path, 4 * 1024 * 1024)?;
                    let (link, current) = unsafe { load_link(&info.path)? };
                    if !paths_equal(&current.target, &exe) { return Err("快捷方式目标在检查后改变，未修改".into()); }
                    let arguments = launcher_arguments_with_port(&current.arguments, port)?;
                    if split_windows_command(&arguments)? == split_windows_command(&current.arguments)? { return Ok(None); }
                    let backup = backup_dir.join(format!("{index}.lnk"));
                    write_new(&backup, &before)?;
                    write_new(&backup_dir.join(format!("{index}.json")),
                        &serde_json::to_vec_pretty(&serde_json::json!({"original": info.path, "backup": backup})).map_err(err)?)?;
                    let temp = info.path.with_extension(format!("{}.lnk", unique_suffix()));
                    let save = (|| {
                        let args = wstr(&arguments)?; let file = wide(temp.as_os_str())?;
                        unsafe {
                            link.SetArguments(PCWSTR(args.as_ptr())).map_err(err)?;
                            let persist: IPersistFile = link.cast().map_err(err)?;
                            persist.Save(PCWSTR(file.as_ptr()), true).map_err(err)?;
                        }
                        let (_, verified) = unsafe { load_link(&temp)? };
                        if !paths_equal(&verified.target, &exe) || verified.arguments != arguments {
                            return Err("暂存快捷方式验证失败".into());
                        }
                        if read_limited(&info.path, 4 * 1024 * 1024)? != before {
                            return Err("快捷方式已被其它进程改变，保留原件".into());
                        }
                        let original = wide(info.path.as_os_str())?;
                        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
                        unsafe { MoveFileExW(PCWSTR(file.as_ptr()), PCWSTR(original.as_ptr()),
                            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH).map_err(err)?; }
                        let (_, verified) = unsafe { load_link(&info.path)? };
                        if !paths_equal(&verified.target, &exe) || verified.arguments != arguments {
                            return Err("写入后验证失败，请使用已保存的快捷方式备份恢复".into());
                        }
                        Ok(())
                    })();
                    let _ = fs::remove_file(temp);
                    save.map_err(|e: String| format!("{e}；备份：{}", backup.display()))?;
                    Ok(Some(backup))
                })();
                match result {
                    Ok(Some(backup)) => { report.repaired.push(info.path.clone()); report.backups.push(backup); }
                    Ok(None) => report.unchanged.push(info.path.clone()),
                    Err(e) => report.errors.push(format!("{}：{e}", info.path.display())),
                }
            }
            Ok(report)
        })
    }

    struct Key(HKEY);
    impl Drop for Key { fn drop(&mut self) { unsafe { let _ = RegCloseKey(self.0); } } }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct RawValue { kind: u32, bytes: Vec<u8> }
    #[derive(Serialize)]
    struct RunBackup<'a> { schema: u32, hive: &'a str, key: &'a str, values: &'a [(String, Option<RawValue>)] }
    fn open_run(write: bool) -> Result<Option<Key>> {
        let mut key = HKEY::default(); let path = wstr(RUN_KEY)?;
        let access = KEY_QUERY_VALUE | if write { KEY_SET_VALUE } else { REG_SAM_FLAGS(0) };
        let code = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), 0, access, &mut key) };
        if code == ERROR_FILE_NOT_FOUND {
            if !write { return Ok(None); }
            unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), 0, PCWSTR::null(),
                REG_OPTION_NON_VOLATILE, access, None, &mut key, None).ok().map_err(err)?; }
        } else { code.ok().map_err(err)?; }
        Ok(Some(Key(key)))
    }
    fn read_value(key: &Key, name: &str) -> Result<Option<RawValue>> {
        let name = wstr(name)?; let mut kind = REG_VALUE_TYPE(0); let mut size = 0;
        let code = unsafe { RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None,
            Some(&mut kind), None, Some(&mut size)) };
        if code == ERROR_FILE_NOT_FOUND { return Ok(None); }
        code.ok().map_err(err)?;
        if size > 128 * 1024 { return Err("自启值过大，拒绝修改".into()); }
        let mut bytes = vec![0u8; size as usize];
        unsafe { RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, Some(&mut kind),
            Some(bytes.as_mut_ptr()), Some(&mut size)).ok().map_err(err)?; }
        bytes.truncate(size as usize);
        Ok(Some(RawValue { kind: kind.0, bytes }))
    }
    fn command(value: &RawValue) -> Result<String> {
        if !matches!(value.kind, 1 | 2) || value.bytes.len() % 2 != 0 { return Err("Run 值不是字符串".into()); }
        let mut words: Vec<u16> = value.bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        if words.last() == Some(&0) { words.pop(); }
        if words.contains(&0) { return Err("Run 值包含嵌入 NUL".into()); }
        let text = String::from_utf16(&words).map_err(err)?;
        if value.kind == REG_EXPAND_SZ.0 {
            use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
            let source = wstr(&text)?;
            let size = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), None) };
            if size == 0 || size > 65536 { return Err("Run 值环境变量展开失败".into()); }
            let mut dest = vec![0u16; size as usize];
            let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(source.as_ptr()), Some(&mut dest)) };
            if n == 0 || n > size { return Err("Run 值环境变量展开失败".into()); }
            return String::from_utf16(&dest[..n as usize - 1]).map_err(err);
        }
        Ok(text)
    }
    fn snapshot(key: &Key) -> Result<Vec<(String, Option<RawValue>)>> {
        [AUTOSTART_VALUE, LEGACY_AUTOSTART_VALUE].iter()
            .map(|name| read_value(key, name).map(|v| ((*name).to_owned(), v))).collect()
    }
    fn status(values: &[(String, Option<RawValue>)], paths: &AppPaths, policy: &LegacyPolicy) -> AutostartStatus {
        let mut result = AutostartStatus::default();
        for (name, raw) in values {
            let Some(raw) = raw else { continue; };
            let text = command(raw).ok();
            let kind = text.as_deref().map(|c| classify_autostart(c, &paths.exe, policy)).unwrap_or(AutostartKind::Unknown);
            result.enabled |= kind == AutostartKind::Rust;
            result.legacy_enabled |= kind == AutostartKind::LegacyPython;
            let target_exists = text.as_deref().and_then(|s| split_windows_command(s).ok())
                .and_then(|v| v.first().cloned()).is_some_and(|p| Path::new(&p).is_file());
            result.entries.push(AutostartEntry { name: name.clone(), command: text, kind, target_exists });
        }
        result
    }
    pub(super) fn autostart_status(paths: &AppPaths, policy: &LegacyPolicy) -> Result<AutostartStatus> {
        match open_run(false)? { Some(key) => Ok(status(&snapshot(&key)?, paths, policy)), None => Ok(AutostartStatus::default()) }
    }
    fn put(key: &Key, name: &str, value: Option<&RawValue>) -> Result<()> {
        let name = wstr(name)?;
        let code = unsafe { match value {
            Some(v) => RegSetValueExW(key.0, PCWSTR(name.as_ptr()), 0, REG_VALUE_TYPE(v.kind), Some(&v.bytes)),
            None => RegDeleteValueW(key.0, PCWSTR(name.as_ptr())),
        }};
        if code == ERROR_SUCCESS || (value.is_none() && code == ERROR_FILE_NOT_FOUND) { Ok(()) }
        else { code.ok().map_err(err) }
    }
    pub(super) fn set_autostart(enabled: bool, args: &[String], paths: &AppPaths, policy: &LegacyPolicy) -> Result<AutostartChange> {
        let key = match open_run(false)? {
            Some(key) => { drop(key); open_run(true)?.ok_or("无法打开 Run 键")? }
            None if !enabled => return Ok(AutostartChange { status: AutostartStatus::default(), backup: None }),
            None => open_run(true)?.ok_or("无法创建 Run 键")?,
        };
        let before = snapshot(&key)?;
        let old_status = status(&before, paths, policy);
        if old_status.entries.iter().any(|e| e.kind == AutostartKind::Unknown) {
            return Err("同名 Run 值不是已确认的悬浮条命令，未修改；请先确认旧程序完整路径".into());
        }
        let desired = if enabled {
            if !paths.exe.is_file() { return Err("当前 exe 不存在，不能设置自启".into()); }
            let command = format!("{}{}", quote_windows_arg(path_text(&paths.exe)?)?,
                if args.is_empty() { String::new() } else { format!(" {}", windows_arguments(args)?) });
            if command.encode_utf16().count() + 1 > 260 { return Err("Run 自启命令超过 Windows 的 260 字符限制".into()); }
            Some(RawValue { kind: REG_SZ.0,
                bytes: command.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect() })
        } else { None };
        let after = vec![(AUTOSTART_VALUE.into(), desired), (LEGACY_AUTOSTART_VALUE.into(), None)];
        if before == after { return Ok(AutostartChange { status: old_status, backup: None }); }
        fs::create_dir_all(&paths.backup_dir).map_err(err)?;
        let backup = paths.backup_dir.join(format!("autostart-{}.json", unique_suffix()));
        write_new(&backup, &serde_json::to_vec_pretty(&RunBackup { schema: 1, hive: "HKEY_CURRENT_USER",
            key: RUN_KEY, values: &before }).map_err(err)?)?;
        if snapshot(&key)? != before { return Err("自启值刚被其它进程改变，已停止修改".into()); }
        let apply: Result<()> = (|| {
            for (name, value) in &after { put(&key, name, value.as_ref())?; }
            if snapshot(&key)? != after { return Err("自启写入后验证失败".into()); }
            Ok(())
        })();
        if let Err(e) = apply {
            let mut rollback_failed = false;
            for ((name, old), (_, new)) in before.iter().zip(after.iter()) {
                match read_value(&key, name) {
                    Ok(current) if current == *new => { rollback_failed |= put(&key, name, old.as_ref()).is_err(); }
                    Ok(current) if current == *old => {}
                    _ => rollback_failed = true,
                }
            }
            return Err(format!("{e}；{}，备份：{}", if rollback_failed { "回滚未完成" } else { "原值已保留/恢复" }, backup.display()));
        }
        Ok(AutostartChange { status: status(&after, paths, policy), backup: Some(backup) })
    }

    struct Process(HANDLE);
    impl Drop for Process { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } } }
    fn creation(handle: HANDLE) -> Result<u64> {
        use windows::Win32::System::Threading::GetProcessTimes;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u).map_err(err)?; }
        Ok(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
    pub(super) fn current_process_identity() -> Result<ProcessIdentity> {
        use windows::Win32::System::Threading::GetCurrentProcess;
        Ok(ProcessIdentity { pid: std::process::id(), created: creation(unsafe { GetCurrentProcess() })? })
    }
    pub(super) fn wait_for_process_exit(parent: &ProcessIdentity, exe: &Path, timeout: Duration) -> Result<()> {
        use windows::Win32::System::Threading::*;
        if parent.pid == 0 || parent.pid == std::process::id() || parent.created == 0 { return Err("无效的父进程标识".into()); }
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, parent.pid) } {
            Ok(h) => Process(h),
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => return Ok(()),
            Err(e) => return Err(err(e)),
        };
        wait_for_handle(handle.0, parent.created, exe, timeout)
    }
    fn wait_for_handle(handle: HANDLE, expected_creation: u64, exe: &Path, timeout: Duration) -> Result<()> {
        use windows::Win32::System::Threading::*;
        if creation(handle)? != expected_creation { return Ok(()); } // PID 已重用，原进程已经退出。
        let mut buf = vec![0u16; 32768]; let mut len = buf.len() as u32;
        unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).map_err(err)?; }
        let actual = PathBuf::from(OsString::from_wide(&buf[..len as usize]));
        if !paths_equal(&actual, exe) { return Err("父进程不是目标悬浮条，拒绝操作".into()); }
        let result = unsafe { WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128 - 1) as u32) };
        if result != WAIT_OBJECT_0 { return Err("等待旧悬浮条退出超时/失败，未替换程序".into()); }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn wait_checks_identity_and_does_not_signal_live_process() {
            use windows::Win32::System::Threading::GetCurrentProcess;
            let identity = current_process_identity().unwrap();
            let handle = unsafe { GetCurrentProcess() };
            let exe = std::env::current_exe().unwrap();
            assert!(wait_for_handle(handle, identity.created, &exe, Duration::ZERO).unwrap_err().contains("超时"));
            assert!(wait_for_handle(handle, identity.created + 1, &exe, Duration::ZERO).is_ok());
            assert!(wait_for_handle(handle, identity.created, &exe.with_file_name("unrelated.exe"), Duration::ZERO).unwrap_err().contains("不是目标"));
            assert!(wait_for_process_exit(&identity, &exe, Duration::ZERO).unwrap_err().contains("无效"));
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    fn unsupported<T>() -> Result<T> { Err("此系统集成功能仅支持 Windows".into()) }
    pub(super) fn autostart_status(_: &AppPaths, _: &LegacyPolicy) -> Result<AutostartStatus> { unsupported() }
    pub(super) fn set_autostart(_: bool, _: &[String], _: &AppPaths, _: &LegacyPolicy) -> Result<AutostartChange> { unsupported() }
    pub(super) fn pick_file(_: isize, _: Option<&Path>, _: bool) -> Result<Option<PathBuf>> { unsupported() }
    pub(super) fn inspect_launchers(_: &Path) -> Result<Vec<LauncherInfo>> { unsupported() }
    pub(super) fn repair_launchers(_: &Path, _: u16, _: &AppPaths) -> Result<LauncherRepairReport> { unsupported() }
    pub(super) fn choose_font(_: isize, _: &FontChoice) -> Result<Option<FontChoice>> { unsupported() }
    pub(super) fn desktop_dir() -> Result<PathBuf> { unsupported() }
    pub(super) fn shell_open(_: &OsStr) -> Result<()> { unsupported() }
    pub(super) fn create_shortcut(_: &AppPaths, _: &ShortcutOptions) -> Result<PathBuf> { unsupported() }
    pub(super) fn current_process_identity() -> Result<ProcessIdentity> { unsupported() }
    pub(super) fn wait_for_process_exit(_: &ProcessIdentity, _: &Path, _: Duration) -> Result<()> { unsupported() }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self { let p = std::env::temp_dir().join(format!("token-bar-system-test-{}", unique_suffix())); fs::create_dir(&p).unwrap(); Self(p) } }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    #[test]
    fn windows_arguments_round_trip() {
        let args = vec!["".into(), r"C:\路径 有空格\".into(), "a\"b".into(), "a\\\"b".into(), "%PATH% & exit".into(), "普通参数".into()];
        assert_eq!(split_windows_command(&windows_arguments(&args).unwrap()).unwrap(), args);
        assert!(split_windows_command("\"broken").is_err());
        assert!(quote_windows_arg("a\0b").is_err());
    }
    #[test]
    fn only_known_commands_are_owned() {
        let temp = Temp::new(); let exe = temp.0.join("bar.exe"); let script = temp.0.join("hoverbar.py");
        let policy = LegacyPolicy { python_scripts: vec![script.clone()], ..Default::default() };
        let own = windows_arguments(&[path_text(&exe).unwrap().into(), "--wait-zcode".into()]).unwrap();
        assert_eq!(classify_autostart(&own, &exe, &policy), AutostartKind::Rust);
        let py = windows_arguments(&[r"C:\Python\pythonw.exe".into(), path_text(&script).unwrap().into(), "--wait-zcode".into()]).unwrap();
        assert_eq!(classify_autostart(&py, &exe, &policy), AutostartKind::LegacyPython);
        assert_eq!(classify_autostart(&py, &exe, &LegacyPolicy::default()), AutostartKind::Unknown);
        assert_eq!(classify_autostart("cmd.exe /c pythonw.exe hoverbar.py", &exe, &policy), AutostartKind::Unknown);
        assert_eq!(classify_autostart(&format!("{own} --token-bar-internal-apply"), &exe, &policy), AutostartKind::Unknown);
    }
    #[test]
    fn portable_requires_marker_and_env_wins() {
        let temp = Temp::new(); let base = temp.0.join("app"); fs::create_dir(&base).unwrap();
        let exe = base.join("bar.exe"); let local = temp.0.join("local");
        assert_eq!(AppPaths::discover_with_base(&exe, None, Some(&local), &[]).unwrap().data_dir, local.join("ZCodeTokenBar"));
        fs::write(base.join("portable.flag"), "").unwrap();
        assert_eq!(AppPaths::discover_with_base(&exe, None, Some(&local), &[]).unwrap().data_dir, base.join("data"));
        fs::write(base.join("data-dir.txt"), "../saved").unwrap();
        assert_eq!(AppPaths::discover_with_base(&exe, None, Some(&local), &[]).unwrap().data_dir, base.join("../saved"));
        assert_eq!(AppPaths::discover_with_base(&exe, Some(&local), None, &[]).unwrap().data_dir, local);
    }
    #[test]
    fn migration_preserves_source_and_existing_target() {
        let temp = Temp::new(); let old = temp.0.join("old"); fs::create_dir(&old).unwrap();
        let source = old.join("hoverbar.json"); fs::write(&source, br#"{"font_px":17,"extra":true}"#).unwrap();
        let paths = AppPaths::from_root(&temp.0.join("bar.exe"), temp.0.join("new"));
        let result = paths.initialize(std::slice::from_ref(&old)).unwrap();
        assert_eq!(result.source.as_deref(), Some(source.as_path()));
        assert_eq!(fs::read(&source).unwrap(), fs::read(&paths.config_file).unwrap());
        fs::write(&paths.config_file, b"{\"font_px\":24}").unwrap();
        assert!(paths.initialize(&[old]).unwrap().source.is_none());
        assert_eq!(fs::read(&paths.config_file).unwrap(), b"{\"font_px\":24}");
    }
    #[test]
    fn safe_names_and_publish_never_overwrite() {
        for name in ["..", "CON", "NUL.txt", "a/b", "file.", "LPT1", "a:stream"] { assert!(validate_component(name).is_err(), "{name}"); }
        assert!(validate_component("悬浮条").is_ok());
        let temp = Temp::new(); let file = temp.0.join("config.json");
        publish_new(&file, b"first").unwrap(); assert!(publish_new(&file, b"second").is_err());
        assert_eq!(fs::read(file).unwrap(), b"first");
    }
    #[test]
    fn legacy_python_marker_and_verified_root_are_discovered_without_writes() {
        let temp = Temp::new(); let exe = temp.0.join("rust/target/release/bar.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let legacy = temp.0.join("python"); fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("data-dir.txt"), "\u{feff} ../saved-data \r\n").unwrap();
        let paths = AppPaths::discover_with_base(&exe, None, None, std::slice::from_ref(&legacy)).unwrap();
        assert_eq!(paths.data_dir, legacy.join("../saved-data")); assert!(!paths.data_dir.exists());
        fs::remove_file(legacy.join("data-dir.txt")).unwrap(); fs::write(legacy.join("hoverbar.log"), "old").unwrap();
        assert_eq!(AppPaths::discover_with_base(&exe, None, None, &[legacy.clone()]).unwrap().data_dir, legacy);
    }
    #[test]
    fn launcher_port_replacement_preserves_other_arguments_and_is_idempotent() {
        let original = windows_arguments(&["--profile".into(), "目录 有空格".into(),
            "--remote-debugging-port".into(), "9222".into(), "--remote-debugging-port=9333".into(),
            "--".into(), "--remote-debugging-port=literal".into()]).unwrap();
        let updated = launcher_arguments_with_port(&original, 9444).unwrap();
        assert_eq!(split_windows_command(&updated).unwrap(), vec!["--profile", "目录 有空格", "--remote-debugging-port=9444", "--", "--remote-debugging-port=literal"]);
        assert_eq!(launcher_arguments_with_port(&updated, 9444).unwrap(), updated);
        assert!(launcher_arguments_with_port("", 0).is_err());
        assert_eq!(split_windows_command("\"a\"\"b\"").unwrap(), vec!["a\"b"]);
    }
}
