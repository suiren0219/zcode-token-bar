//! ZCode Token 悬浮条（Rust 版）——独立进程，不碰客户端安装目录。
//!
//! 与 Python 版（D:/zcode-data/workspace/default/zcode-token-bar）同源同口径：
//! 只读 ~/.zcode/cli/db/db.sqlite，1.5 秒一拍刷新条面，贴着 ZCode 主窗口。
//!
//! 命令行（对齐 Python 版习惯）：
//! * `zcode-token-bar-rs`            启动条面（已有实例则无操作）
//! * `zcode-token-bar-rs --once`     终端打印一次快照退出（排查用）
//! * `zcode-token-bar-rs --quit`     通知正在运行的实例退出
//! * `zcode-token-bar-rs --debug`    条面 + 终端日志

mod data;
mod win;

use slint::ComponentHandle;

slint::include_modules!();

/// 数据根：与 Python 版共用目录（日志、自身设置放这里；db 是只读共享）。
fn data_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("ZCODE_TOKEN_BAR_DATA") {
        return d.into();
    }
    "D:\\zcode-data\\token-bar-data".into()
}

fn init_logging() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    // tracing-appender 按天轮转；non_blocking 包装成不阻塞 UI 的异步写。
    // guard 持有 writer 生命周期，进程常驻 → forget 掉让它活到退出。
    let appender = tracing_appender::rolling::daily(dir.join("logs"), "hoverbar-rs.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    std::mem::forget(guard);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .init();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = args.get(1).map(|s| s.as_str()).unwrap_or("");

    // --quit：给正在运行的实例发退出信号（命名事件跨进程可见）。
    if arg == "--quit" {
        let ok = win::signal_quit("Local\\zcode-token-bar-rs");
        println!("{}", if ok { "已通知退出" } else { "没有找到运行中的实例事件" });
        return;
    }

    // 单实例：已有实例在跑，直接退出。
    let si = match win::SingleInstance::acquire("Local\\zcode-token-bar-rs") {
        Some(si) => si,
        None => {
            eprintln!("悬浮条已在运行（--quit 可退出）");
            return;
        }
    };

    init_logging();

    // --once：打印一次快照（诊断用）。
    if arg == "--once" {
        match data::connect_readonly() {
            Ok(con) => {
                let s = data::snapshot(&con, default_context_window());
                println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
            }
            Err(e) => println!("打不开数据库：{e}"),
        }
        return;
    }

    let debug = arg == "--debug";

    // ── UI 装配 ──
    let bar = BarWindow::new().expect("Slint 窗口创建失败");
    bar.show().expect("Slint 窗口显示失败");

    // Slint 在 show 时才真正创建 Win32 窗口，但此刻它可能还是 winit 的
    // 14x14 占位尺寸（resize 异步），枚举会拿错。所以启动时不急着定位——
    // 贴靠定时器每拍重新找自己的窗口并幂等应用样式，一秒内自愈，
    // 顺带覆盖 ZCode 重启/条面重建的场景。
    let zcode0 = win::zcode_hwnd();

    // ── 数据泵：专用线程持有 SQLite 连接 ──
    // rusqlite 的 Connection 是 Send 但 !Sync——可以整体搬进一个线程，
    // 不能多线程共享。泵每 1.5s 查一次，通过 invoke_from_event_loop 把
    // 快照排回 UI 线程（Slint 属性只在事件循环线程可写）。
    let ui: slint::Weak<BarWindow> = bar.as_weak();
    let pump = std::thread::spawn(move || {
        let con = data::connect_readonly();
        loop {
            let snap = match &con {
                Ok(c) => data::snapshot(c, default_context_window()),
                Err(e) => data::Snapshot {
                    ok: false,
                    error: e.to_string(),
                    ..Default::default()
                },
            };
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(bar) = ui.upgrade() else { return };
                bar.set_has_data(snap.ok && snap.total > 0);
                bar.set_session_title(display_title(&snap).into());
                bar.set_active(snap.active);
                bar.set_generating(snap.generating);
                apply_snapshot(&bar, &snap);
            });
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    });
    drop(pump);

    // ── 拖动：按住条面拖 → 自由位置（free 模式）；双击条面恢复自动贴靠。
    // 坐标全用 Win32 GetCursorPos 计算，Slint 回调只当「事件发生」信号。
    static FREE_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static DRAG: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

    bar.on_drag_start(move || {
        let hwnd = win::own_hwnd();
        let (wx, wy, _, _) = win::window_rect(hwnd);
        let (cx, cy) = win::cursor_pos();
        *DRAG.lock().unwrap() = Some((wx, wy, cx, cy));
    });
    bar.on_drag_move(|| {
        let mut g = DRAG.lock().unwrap();
        let cur = g.take(); // 先取出来再放回，避免借用冲突
        if let Some((wx, wy, sx, sy)) = cur {
            let (cx, cy) = win::cursor_pos();
            let hwnd = win::own_hwnd();
            unsafe { win::move_window(hwnd, wx + cx - sx, wy + cy - sy) };
            *g = Some((wx, wy, cx, cy));
        }
    });
    bar.on_drag_end(|| {
        *DRAG.lock().unwrap() = None;
        FREE_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    bar.on_dock_restore(|| {
        FREE_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    // ── 贴靠定时器：1 秒一拍，跟住 ZCode 移动/重启 ──
    // 每拍：找自己（面积最大的本进程可见窗口）→ 幂等应用工具窗样式 →
    // 按需重新属主绑定 → 贴靠。全部幂等，窗口被系统折腾了下一拍就回来。
    // scale 每拍从 UI 读：启动瞬间 scale_factor 可能还是 1.0，DPI 感知
    // 稍后才生效（125% 屏上按 1.0 算居中会偏 70 物理像素——实测踩过）。
    let timer = slint::Timer::default();
    let timer_ui = bar.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(1000),
        move || {
            let Some(bar) = timer_ui.upgrade() else { return };
            // 拖动中/自由模式不抢位置。
            if FREE_MODE.load(std::sync::atomic::Ordering::Relaxed)
                || DRAG.lock().unwrap().is_some()
            {
                return;
            }
            let hwnd = win::own_hwnd();
            if hwnd == 0 {
                return;
            }
            let zcode = win::zcode_hwnd();
            unsafe { win::apply_bar_styles(hwnd, zcode) };
            let scale = bar.window().scale_factor() as f64;
            let w = (560.0 * scale) as i32;
            let h = (40.0 * scale) as i32;
            let (x, y) = win::dock_position(w, h, zcode);
            unsafe { win::move_window(hwnd, x, y) };
        },
    );
    // run() 阻塞期间 timer 必须活着——forget 阻止 run 返回后被 drop。
    std::mem::forget(timer);

    // 设置按钮：拉起 Python 版设置面板（壁纸/CDP/模型导入全在）。
    // Rust 原生面板完成前，这是功能继承的桥接——pythonw 无控制台窗口。
    bar.on_settings_clicked(move || {
        let _ = std::process::Command::new("pythonw")
            .arg(r"D:\zcode-data\workspace\default\zcode-token-bar\hoverbar.py")
            .arg("--panel")
            .spawn();
        tracing::info!("已请求打开设置面板（Python 桥接）");
    });

    if debug {
        tracing::info!("调试模式：条面已启动");
    }

    // 退出信号等待放后台线程；收到就关窗、停事件循环。
    {
        let ui = bar.as_weak();
        std::thread::spawn(move || loop {
            if si.wait_quit(60_000) {
                if let Some(bar) = ui.upgrade() {
                    let _ = bar.hide();
                }
                slint::quit_event_loop();
                return;
            }
        });
    }

    tracing::info!("Rust 悬浮条启动（zcode={zcode0}）");
    bar.run().expect("Slint 事件循环异常退出");
    tracing::info!("Rust 悬浮条退出");
}

/// 会话标题显示：无标题用目录名兜底，再兜底「无标题会话」。
fn display_title(s: &data::Snapshot) -> String {
    let t = s.title.trim();
    if !t.is_empty() {
        return t.to_string();
    }
    let d = s.directory.trim();
    if !d.is_empty() {
        return std::path::Path::new(d)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| d.to_string());
    }
    "无标题会话".into()
}

/// 把快照映射到条面控件（口径注释见 data.rs；配色档位照 Python 版）。
/// Slint 的字符串属性是 SharedString，String 一律 .into() 转换。
fn apply_snapshot(bar: &BarWindow, s: &data::Snapshot) {
    // 速度：≥70 绿 / ≥40 橙 / 其余红。
    let tps = s.last.tps;
    bar.set_speed_text(if tps > 0.0 { format!("{tps:.1}").into() } else { "".into() });
    bar.set_speed_level(if tps >= 70.0 { 0 } else if tps >= 40.0 { 1 } else { 2 });

    // 上下文：百分比 + 三档（>1M 窗口用宽松阈值）；超限告警压过普通配色。
    let cw = s.context_window;
    let ctx = s.ctx;
    if cw > 0 && ctx > 0 {
        let pct = (ctx as f64 / cw as f64 * 100.0).min(100.0);
        let big = cw >= 1_000_000;
        bar.set_ctx_pct(pct as i32);
        bar.set_ctx_text(format!("{pct:.1}%").into());
        let level = if pct >= if big { 60.0 } else { 85.0 } {
            2
        } else if pct >= if big { 40.0 } else { 70.0 } {
            1
        } else {
            0
        };
        bar.set_ctx_level(if s.ctx_exc > s.last_at { 2 } else { level });
    } else {
        bar.set_ctx_pct(0);
        bar.set_ctx_text("".into());
        bar.set_ctx_level(0);
    }

    // 本轮：合计 + 次数/耗时。
    let lt = &s.last_turn;
    if lt.total > 0 || lt.requests > 0 {
        bar.set_turn_text(data::fmt_tokens(lt.total).into());
        let secs = lt.duration_ms / 1000;
        let tail = if secs > 0 { format!(" {}s", secs) } else { String::new() };
        bar.set_turn_sub(format!("{}次{}", lt.requests, tail).into());
    } else {
        bar.set_turn_text("".into());
        bar.set_turn_sub("".into());
    }

    // 会话：合计 + 轮次/请求数。
    if s.total > 0 {
        bar.set_session_text(data::fmt_tokens(s.total).into());
        bar.set_session_sub(format!("{}轮 {}次", s.turns, s.requests).into());
    } else {
        bar.set_session_text("".into());
        bar.set_session_sub("".into());
    }

    bar.set_today_text(if s.today.total > 0 { s.today_text.clone().into() } else { "".into() });

    // 缓存率：分母是 input 本身（input_tokens 口径已含 cache_read）。
    if s.input > 0 {
        let rate = s.cache_read as f64 / s.input as f64 * 100.0;
        bar.set_cache_text(format!("{rate:.0}%").into());
        bar.set_cache_level(if rate >= 60.0 { 0 } else if rate >= 30.0 { 1 } else { 2 });
    } else {
        bar.set_cache_text("".into());
    }
}

/// 默认上下文窗口：数据根 config.json 可覆盖（Python load_config 同字段）。
fn default_context_window() -> i64 {
    let path = data_dir().join("config.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("context_window").and_then(|x| x.as_i64()))
        .unwrap_or(128_000)
}
