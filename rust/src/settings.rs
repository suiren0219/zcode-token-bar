//! 自身设置：settings-rs.json（数据根下），结构与 Python 版 DEFAULTS 对齐，
//! 字段名保持一致便于以后两版互相导入。
//!
//! Rust 惯用法：`serde` 把结构体直接序列化/反序列化，Python 版那一堆
//! 「dict 里挑键、类型校验」的手工代码在 derive 下基本消失。
//! `#[serde(default)]` 让缺字段自动落到默认值——对应 Python 版
//! 「用户值覆盖默认、缺的保留默认」的合并逻辑。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 显示项开关（条面每 1.5s 按 apply_snapshot 组装，关了就输出空串）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowItems {
    #[serde(default = "yes")]
    pub speed: bool,
    #[serde(default = "yes")]
    pub ctx: bool,
    #[serde(default = "yes")]
    pub turn: bool,
    #[serde(default = "yes")]
    pub session: bool,
    #[serde(default = "yes")]
    pub today: bool,
    #[serde(default = "yes")]
    pub tools: bool,
    #[serde(default = "yes")]
    pub cache: bool,
}
fn yes() -> bool {
    true
}
impl Default for ShowItems {
    fn default() -> Self {
        ShowItems { speed: true, ctx: true, turn: true, session: true, today: true, tools: true, cache: true }
    }
}

/// 条面位置锚点（与 Python ANCHORS 同名）。
pub const ANCHORS: [&str; 7] = [
    "bottom-left", "bottom-center", "bottom-right",
    "top-left", "top-center", "top-right", "free",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub anchor: String,
    /// window = 跟随 ZCode 窗口 / screen = 贴整块屏幕
    pub scope: String,
    pub offset_x: i32,
    pub offset_y: i32,
    /// free 模式（拖动过）记住的位置
    pub free_x: i32,
    pub free_y: i32,
    /// 条面整体不透明度 0.55–1.0
    pub opacity: f64,
    /// 条面基准字号（逻辑 px）
    pub font_px: i32,
    pub always_on_top: bool,
    pub show: ShowItems,
    pub labels: bool,
    /// 默认上下文窗口（模型目录缺失时的 fallback）
    pub context_window: i64,
    /// 设置面板透明度
    pub panel_opacity: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            anchor: "bottom-center".into(),
            scope: "window".into(),
            offset_x: 16,
            offset_y: 6,
            free_x: 0,
            free_y: 0,
            opacity: 0.92,
            font_px: 13,
            always_on_top: true,
            show: ShowItems::default(),
            labels: true,
            context_window: 128_000,
            panel_opacity: 1.0,
        }
    }
}

pub fn settings_path() -> PathBuf {
    crate::data_dir().join("settings-rs.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 与 provider_config.json 同款原子写：临时文件 + replace，写一半断电不留坏文件。
    let tmp = path.with_extension("json.tmp");
    if let Ok(body) = serde_json::to_string_pretty(s) {
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// 六向 + free 的贴靠位置计算（scope=window 跟随 ZCode，screen 贴屏幕）。
/// 返回物理像素左上角坐标；bar_w/bar_h 为物理尺寸。
/// 与 Python 版 _place 相比简化了 free 的持久化（由拖动回调配管理）。
pub fn dock_position_ex(
    anchor: &str,
    scope: &str,
    offset_x: i32,
    offset_y: i32,
    bar_w: i32,
    bar_h: i32,
    zcode: isize,
) -> (i32, i32) {
    let (mut zx, have_zcode) = {
        let mut r = windows::Win32::Foundation::RECT::default();
        let ok = zcode != 0
            && unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
                    windows::Win32::Foundation::HWND(zcode as *mut _),
                    &mut r,
                )
            }
            .is_ok();
        (r, ok)
    };
    // scope=screen 或 ZCode 不在：锚点按工作区算。
    let use_window = scope == "window" && have_zcode && anchor != "free";
    let work = monitor_work(zcode);
    let (zone, base) = if use_window { (zx, (zx.left, zx.top, zx.right, zx.bottom)) } else {
        zx = work;
        (work, (work.left, work.top, work.right, work.bottom))
    };
    let _ = zone;
    let (bl, bt, br, bb) = base;
    let x = match anchor {
        "bottom-left" | "top-left" => bl + offset_x,
        "bottom-right" | "top-right" => br - bar_w - offset_x,
        _ => (bl + br) / 2 - bar_w / 2, // *-center
    };
    let y = match anchor {
        "top-left" | "top-center" | "top-right" => bt + offset_y,
        _ => bb - bar_h - offset_y, // bottom-*：贴底（跟随窗口时贴窗口底边内侧）
    };
    // 跟随窗口时 bottom-* 贴窗口下边（比窗口底边高一点点，Python 版贴在输入卡片下方）。
    let y = if use_window && anchor.starts_with("bottom") {
        bb - bar_h - offset_y.max(2)
    } else {
        y
    };
    // 界内 clamp（同 Python「六向全在界内」）。
    let min_x = work.left + 4;
    let max_x = (work.right - bar_w - 4).max(min_x);
    let min_y = work.top + 4;
    let max_y = (work.bottom - bar_h - 4).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

fn monitor_work(hwnd: isize) -> windows::Win32::Foundation::RECT {
    let hmon = unsafe {
        windows::Win32::Graphics::Gdi::MonitorFromWindow(
            windows::Win32::Foundation::HWND(if hwnd != 0 { hwnd as *mut _ } else { std::ptr::null_mut() }),
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
        cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(hmon, &mut mi);
    }
    mi.rcWork
}
