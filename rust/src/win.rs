//! Win32 层：找 ZCode 窗口、贴靠计算、属主绑定、置顶、单实例。
//!
//! Python 版用 ctypes 直接调 user32；Rust 里对应物是 `windows` crate——
//! 它把每个 API 放在 feature gate 后面（Cargo.toml 里列的那些），调用
//! 模式从「动态查找符号」变成「编译期链接」，写错签名编译期就炸。
//!
//! windows-rs 0.58 的几个约定（踩过的坑记在这）：
//! * 出参/入参大多直接收值，不是 `Option`——`Option<HWND>` 是 0.5x 早期
//!   风格，0.58 的 `SetWindowPos` 第二个参数直接给 `HWND_TOPMOST`。
//! * `CreateMutexW/CreateEventW` 的签名里有 `SECURITY_ATTRIBUTES`，
//!   所以还要开 `Win32_Security` feature。
//! * 显示器 API（MonitorFromWindow/MONITORINFO）在 `Win32::Graphics::Gdi`。
//! * `BOOL` 在 `Win32::Foundation`（不是 windows::core）。

use windows::Win32::Foundation::{ BOOL, CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0 };
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{ CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject };
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, SetWindowLongPtrW,
    SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, GWL_EXSTYLE,
    GWLP_HWNDPARENT, HWND_TOPMOST, SWP_NOZORDER, WINDOW_EX_STYLE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};
use windows::core::PCWSTR;

// ── 找 ZCode 主窗口 ─────────────────────────────────────────────────

/// EnumWindows 的回调签名是裸 C 函数指针（`WNDENUMPROC`），
/// 不能捕获环境——所以「收集结果」要走 lparam 传指针，和 Python 版
/// `found = []` 闭包收集同一个意思，只是写法更底层。
struct EnumCtx {
    best: (isize, i64), // (hwnd, 面积)
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return true.into();
    }
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf) as usize;
    let title = String::from_utf16_lossy(&buf[..n.min(256)]);
    if !title.contains("ZCode") {
        return true.into();
    }
    // 工具窗口（WS_EX_TOOLWINDOW）一般是托盘/辅助小窗，跳过。
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return true.into();
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return true.into();
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w < 400 || h < 300 {
        return true.into();
    }
    let area = w as i64 * h as i64;
    if area > ctx.best.1 {
        ctx.best = (hwnd.0 as isize, area);
    }
    true.into()
}

/// ZCode 主窗口句柄：可见、标题含 ZCode、非工具窗、面积最大。
/// 0 = 没找到（客户端没开）。逻辑同 Python `zcode_hwnd()`。
pub fn zcode_hwnd() -> isize {
    let mut ctx = EnumCtx { best: (0, 0) };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut EnumCtx as isize));
    }
    ctx.best.0
}

/// 按标题精确找本进程的 Slint 窗口（条面/面板各一个标题）。
/// 之前按「面积最大」找，面板打开后面板比条面大，贴靠定时器把面板
/// 当成条面套了 WS_EX_NOACTIVATE 还每秒挪走——面板就此无法交互
/// （2026-11-15 实测）。标题是 Slint Window.title 传下去的，唯一可靠。
pub fn own_hwnd(title: &str) -> isize {
    struct Ctx<'a>(&'a [u16], isize);
    let wanted: Vec<u16> = title.encode_utf16().collect();
    let mut ctx = Ctx(&wanted, 0);
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != std::process::id() {
            return true.into();
        }
        let n = GetWindowTextLengthW(hwnd) as usize;
        if n == ctx.0.len() {
            let mut buf = vec![0u16; n + 1];
            GetWindowTextW(hwnd, &mut buf);
            if buf[..n] == *ctx.0 {
                ctx.1 = hwnd.0 as isize;
                return false.into();
            }
        }
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.1
}

// ── 窗口形态：无边框小胶囊的工具样式 ────────────────────────────────

/// 给 Slint 建好的窗口加：不进任务栏、不抢焦点、置顶。
/// Slint 的 `no-frame` 只去掉边框；「像悬浮条」的扩展样式要自己补。
///
/// # 安全
/// `hwnd` 必须是本进程的合法窗口句柄。
pub unsafe fn apply_bar_styles(hwnd: isize, owner: isize) {
    let hwnd = HWND(hwnd as *mut _);
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    SetWindowLongPtrW(
        hwnd,
        GWL_EXSTYLE,
        (ex | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST).0 as isize,
    );
    if owner != 0 {
        // 属主绑定：条面永远压在 ZCode 上、随它最小化一起让路（Python 同款）。
        SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner);
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

/// 把窗口放到物理坐标 (x, y)，同时确保置顶。
///
/// # 安全
/// `hwnd` 必须是本进程的合法窗口句柄。
pub unsafe fn move_window(hwnd: isize, x: i32, y: i32) {
    let _ = SetWindowPos(
        HWND(hwnd as *mut _),
        HWND_TOPMOST,
        x, y, 0, 0,
        SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// 计算条面贴靠位置：默认贴 ZCode 主窗口正下方居中；
/// ZCode 不在/最大化则贴屏幕底部居中（Python 同策略）。
/// 结果强制 clamp 进工作区——ZCode 被拖到屏幕边缘时条面不许跟出去
/// （Python 版「六向全在界内」同规则，2026-11-15 用户实测越界后补）。
///
/// 返回物理像素坐标（条面左上角）。
pub fn dock_position(bar_w: i32, bar_h: i32, zcode: isize) -> (i32, i32) {
    let mut zx = RECT::default();
    let have_zcode = zcode != 0 && unsafe { GetWindowRect(HWND(zcode as *mut _), &mut zx) }.is_ok();

    let anchor = if have_zcode { zcode } else { 0 };
    let work = monitor_info(anchor).rcWork;
    let (x, y) = if have_zcode {
        // 最大化检测：窗口矩形盖住整个工作区 ≈ 最大化 → 回落屏幕底部。
        let maximized = zx.left <= work.left && zx.right >= work.right && zx.top <= work.top;
        if maximized {
            ((work.left + work.right) / 2 - bar_w / 2, work.bottom - bar_h - 2)
        } else {
            let cx = (zx.left + zx.right) / 2;
            let x = cx - bar_w / 2;
            let y = zx.bottom + 4;
            if y + bar_h <= work.bottom {
                (x, y)
            } else {
                // 底下放不下（任务栏/屏幕边）：贴 ZCode 所在工作区底部。
                ((work.left + work.right) / 2 - bar_w / 2, work.bottom - bar_h - 2)
            }
        }
    } else {
        ((work.left + work.right) / 2 - bar_w / 2, work.bottom - bar_h - 2)
    };

    // 界内 clamp：左右各留 4px。max 兜底处理「屏幕比条面还窄」的极端。
    let min_x = work.left + 4;
    let max_x = (work.right - bar_w - 4).max(min_x);
    let min_y = work.top + 4;
    let max_y = (work.bottom - bar_h - 4).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

fn monitor_info(hwnd: isize) -> MONITORINFO {
    let hmon = unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(hmon, &mut mi);
    }
    mi
}

/// 当前光标物理坐标（拖动计算用）。
pub fn cursor_pos() -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

/// 窗口矩形 (left, top, right, bottom)，物理坐标。失败返回 (0,0,0,0)。
pub fn window_rect(hwnd: isize) -> (i32, i32, i32, i32) {
    let mut r = RECT::default();
    if unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut r) }.is_ok() {
        (r.left, r.top, r.right, r.bottom)
    } else {
        (0, 0, 0, 0)
    }
}

/// 置顶刷新（ZCode 重启/全屏切换后 topmost 可能被系统洗掉，定时补一次）。
pub unsafe fn ensure_topmost(hwnd: isize) {
    let _ = SetWindowPos(
        HWND(hwnd as *mut _),
        HWND_TOPMOST,
        0, 0, 0, 0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
    );
}

// ── 单实例 + 远程退出 ───────────────────────────────────────────────

/// 单实例互斥量 + 命名退出事件。
///
/// Python 版用 lock 文件 + 轮询；Rust 版换成内核对象：
/// `CreateMutexW` 天然跨进程互斥（进程崩了系统自动释放，不会留死锁文件），
/// `CreateEventW` + `SetEvent` 让 `--quit` 立刻唤醒主线程，不用轮询。
pub struct SingleInstance {
    mutex: HANDLE,
    quit_event: HANDLE,
}

// HANDLE 内部是 *mut c_void（!Send），但这里的两个句柄只被
// 「创建它的线程之外的一个等待线程」使用，内核对象本身跨线程安全，
// CloseHandle 也只在这份所有权 drop 时调用一次——手动声明 Send。
unsafe impl Send for SingleInstance {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl SingleInstance {
    /// 已有实例在跑 → 返回 None（`--quit` 由调用方先发退出信号）。
    pub fn acquire(name: &str) -> Option<SingleInstance> {
        unsafe {
            let mutex = CreateMutexW(None, false, PCWSTR(wide(name).as_ptr())).ok()?;
            // 互斥量已存在时句柄照样返回，要用 GetLastError 区分。
            if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
                return None;
            }
            let quit_event = CreateEventW(
                None,
                true,             // 手动复位：多个等待者都能看到
                false,
                PCWSTR(wide(&format!("{name}-quit")).as_ptr()),
            )
            .ok()?;
            Some(SingleInstance { mutex, quit_event })
        }
    }

    /// 另一个实例在等这个信号退出（`--quit` 命令用）。
    pub fn signal_quit(&self) {
        unsafe {
            let _ = SetEvent(self.quit_event);
        }
    }

    /// 阻塞等退出信号（主线程的最后一步）。返回 true = 收到退出。
    pub fn wait_quit(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.quit_event, timeout_ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.quit_event);
            let _ = CloseHandle(self.mutex);
        }
    }
}

/// 给运行中的实例发退出信号（--quit 用）。
/// 不能走 SingleInstance::acquire——它发现 mutex 已存在会返回 None，
/// 而退出信号恰恰要在「已有实例」时发。命名事件单独打开即可。
pub fn signal_quit(name: &str) -> bool {
    unsafe {
        let ev = CreateEventW(None, true, false, PCWSTR(wide(&format!("{name}-quit")).as_ptr())).ok();
        match ev {
            Some(h) => {
                let _ = SetEvent(h);
                let _ = CloseHandle(h);
                true
            }
            None => false,
        }
    }
}
