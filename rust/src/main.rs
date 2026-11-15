//! ZCode Token 悬浮条（Rust 版）——独立进程 + 系统托盘，不碰客户端安装目录。
//!
//! 与 Python 版同源同口径：只读 ~/.zcode/cli/db/db.sqlite，1.5 秒一拍刷新
//! 条面，贴着 ZCode 主窗口；设置面板原生（位置/外观/显示/模型/状态），
//! 壁纸/CDP 暂由 Python 面板桥接承担。
//!
//! 命令行：
//! * 启动（已有实例则退出）/ `--once` / `--quit` / `--debug`
//!
//! 托盘：右键菜单「打开设置 / 恢复自动贴靠 / 退出」。菜单事件走全局
//! channel，由一个 500ms 的 Slint Timer 轮询（tray-icon 的 receiver
//! 是 mpsc，与 winit 事件循环解耦，轮询是最省事的集成方式）。

mod data;
mod models;
mod settings;
mod win;

use settings::Settings;
use slint::ComponentHandle;

slint::include_modules!();

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 拖动状态（条面与面板共用：起点窗口坐标 + 起点光标坐标）。
static DRAG: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

/// 数据根：与 Python 版共用目录（db 只读共享；日志/设置放这里）。
pub fn data_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("ZCODE_TOKEN_BAR_DATA") {
        return d.into();
    }
    "D:\\zcode-data\\token-bar-data".into()
}

fn init_logging() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
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

    if arg == "--quit" {
        let ok = win::signal_quit("Local\\zcode-token-bar-rs");
        println!("{}", if ok { "已通知退出" } else { "没有找到运行中的实例事件" });
        return;
    }

    let si = match win::SingleInstance::acquire("Local\\zcode-token-bar-rs") {
        Some(si) => si,
        None => {
            eprintln!("悬浮条已在运行（--quit 可退出）");
            return;
        }
    };

    init_logging();

    if arg == "--once" {
        match data::connect_readonly() {
            Ok(con) => {
                let cfg = settings::load();
                let s = data::snapshot(&con, cfg.context_window);
                println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
            }
            Err(e) => println!("打不开数据库：{e}"),
        }
        return;
    }

    let debug = arg == "--debug";

    // 全局共享状态：设置（Arc<Mutex>）+ free 模式 + 拖动状态。
    let cfg: Arc<Mutex<Settings>> = Arc::new(Mutex::new(settings::load()));
    let free_mode = Arc::new(AtomicBool::new(cfg.lock().unwrap().anchor == "free"));

    // ── 条面 ──
    let bar = BarWindow::new().expect("Slint 窗口创建失败");
    bar.show().expect("Slint 窗口显示失败");

    // ── 托盘 ──
    // 图标：include_bytes! 直读脚本生成的 32x32 原始 RGBA（零 image 依赖）。
    let tray_icon = {
        use tray_icon::TrayIconBuilder;
        let rgba = include_bytes!("tray.rgba");
        let icon = tray_icon::Icon::from_rgba(rgba.to_vec(), 32, 32).expect("托盘图标无效");
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        let menu = Menu::new();
        for (id, label) in [
            ("open-settings", "打开设置"),
            ("restore-dock", "恢复自动贴靠"),
        ] {
            let item = MenuItem::with_id(id, label, true, None);
            let _ = menu.append(&item);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let quit_item = MenuItem::with_id("quit", "退出", true, None);
        let _ = menu.append(&quit_item);
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("ZCode Token 悬浮条（Rust）")
            .with_icon(icon)
            .build()
            .expect("托盘创建失败")
    };
    // TrayIcon 对象必须存活到进程结束。
    std::mem::forget(tray_icon);

    // ── 设置窗口（懒创建，hide/show 复用） ──
    let panel: Arc<Mutex<Option<slint::Weak<SettingsWindow>>>> = Arc::new(Mutex::new(None));

    fn open_panel(
        panel: &Arc<Mutex<Option<slint::Weak<SettingsWindow>>>>,
        cfg: &Arc<Mutex<Settings>>,
    ) {
        let mut slot = panel.lock().unwrap();
        let win = match slot.as_ref().and_then(|w| w.upgrade()) {
            Some(w) => w,
            None => {
                let w = SettingsWindow::new().expect("设置窗口创建失败");
                *slot = Some(w.as_weak());
                w
            }
        };
        sync_panel(&win, &cfg.lock().unwrap());
        let _ = win.show();
    }

    fn sync_panel(win: &SettingsWindow, cfg: &Settings) {
        let names: Vec<slint::SharedString> = models::configured_providers()
            .into_iter()
            .map(|p| format!("{} · {}", p.name, p.kind).into())
            .collect();
        win.set_provider_names(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(names))));
        win.set_anchor_index(match cfg.anchor.as_str() {
            "bottom-left" => 0, "bottom-center" => 1, "bottom-right" => 2,
            "top-left" => 3, "top-center" => 4, "top-right" => 5, _ => 1,
        });
        win.set_scope_index(if cfg.scope == "screen" { 1 } else { 0 });
        win.set_opacity_pct((cfg.opacity * 100.0).round() as i32);
        win.set_font_px(cfg.font_px);
        win.set_show_speed(cfg.show.speed);
        win.set_show_ctx(cfg.show.ctx);
        win.set_show_turn(cfg.show.turn);
        win.set_show_session(cfg.show.session);
        win.set_show_today(cfg.show.today);
        win.set_show_tools(cfg.show.tools);
        win.set_show_cache(cfg.show.cache);
        win.set_show_labels(cfg.labels);
        win.set_status_lines(format!(
            "版本: 0.1.0-rust\nPID: {}\n自启动: 已注册（HKCU Run）\n数据根: {}\n设置文件: {}\n数据库: ~/.zcode/cli/db/db.sqlite（只读）\n壁纸/CDP: 经 Python 面板（设置按钮桥接）",
            std::process::id(),
            data_dir().display(),
            settings::settings_path().display(),
        ).into());
    }

    // 条面初值：应用设置
    {
        let c = cfg.lock().unwrap();
        bar.set_opacity_level(c.opacity as f32);
        bar.set_font_px(c.font_px);
        bar.set_labels_on(c.labels);
    }

    // ── 条面回调：设置按钮打开原生面板 ──
    {
        let panel = panel.clone();
        let cfg = cfg.clone();
        bar.on_settings_clicked(move || open_panel(&panel, &cfg));
    }

    // ── 条面拖动 ──
    {
        let free = free_mode.clone();
        let cfg = cfg.clone();
        bar.on_drag_start(move || {
            let hwnd = win::own_hwnd("zcode-token-bar-bar");
            let (wx, wy, _, _) = win::window_rect(hwnd);
            let (cx, cy) = win::cursor_pos();
            *DRAG.lock().unwrap() = Some((wx, wy, cx, cy));
        });
        bar.on_drag_move(|| {
            let mut g = DRAG.lock().unwrap();
            let cur = g.take();
            if let Some((wx, wy, sx, sy)) = cur {
                let (cx, cy) = win::cursor_pos();
                let hwnd = win::own_hwnd("zcode-token-bar-bar");
                unsafe { win::move_window(hwnd, wx + cx - sx, wy + cy - sy) };
                *g = Some((wx, wy, cx, cy));
            }
        });
        let free_end = free.clone();
        let cfg_end = cfg.clone();
        bar.on_drag_end(move || {
            let hwnd = win::own_hwnd("zcode-token-bar-bar");
            let (l, t, _, _) = win::window_rect(hwnd);
            {
                let mut c = cfg_end.lock().unwrap();
                c.anchor = "free".into();
                c.free_x = l;
                c.free_y = t;
                settings::save(&c);
            }
            free_end.store(true, Ordering::Relaxed);
            DRAG.lock().unwrap().take();
        });
        bar.on_dock_restore(move || {
            {
                let mut c = cfg.lock().unwrap();
                c.anchor = "bottom-center".into();
                settings::save(&c);
            }
            free.store(false, Ordering::Relaxed);
        });
    }


    // ── 数据泵 ──
    let ui = bar.as_weak();
    let cfg_pump = cfg.clone();
    let pump = std::thread::spawn(move || {
        let con = data::connect_readonly();
        loop {
            let (ok_data, cw) = {
                let c = cfg_pump.lock().unwrap();
                ((), c.context_window)
            };
            let _ = ok_data;
            let snap = match &con {
                Ok(c) => data::snapshot(c, cw),
                Err(e) => data::Snapshot {
                    ok: false,
                    error: e.to_string(),
                    ..Default::default()
                },
            };
            let show = {
                let c = cfg_pump.lock().unwrap();
                c.show
            };
            let labels = {
                let c = cfg_pump.lock().unwrap();
                c.labels
            };
            let ui = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(bar) = ui.upgrade() else { return };
                bar.set_has_data(snap.ok && snap.total > 0);
                bar.set_session_title(display_title(&snap).into());
                bar.set_active(snap.active);
                bar.set_generating(snap.generating);
                apply_snapshot(&bar, &snap, &show, labels);
            });
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    });
    drop(pump);

    // ── 贴靠定时器 + 托盘菜单轮询 ──
    let timer = slint::Timer::default();
    let timer_ui = bar.as_weak();
    let panel_t = panel.clone();
    let cfg_t = cfg.clone();
    let free_t = free_mode.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(500),
        move || {
            // 托盘菜单事件（非阻塞 try_recv）
            while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                match ev.id().as_ref() {
                    "open-settings" => open_panel(&panel_t, &cfg_t),
                    "restore-dock" => {
                        {
                            let mut c = cfg_t.lock().unwrap();
                            c.anchor = "bottom-center".into();
                            settings::save(&c);
                        }
                        free_t.store(false, Ordering::Relaxed);
                    }
                    "quit" => {
                        slint::quit_event_loop();
                        return;
                    }
                    _ => {}
                }
            }

            let Some(bar) = timer_ui.upgrade() else { return };
            if free_t.load(Ordering::Relaxed) || DRAG.lock().unwrap().is_some() {
                return;
            }
            let hwnd = win::own_hwnd("zcode-token-bar-bar");
            if hwnd == 0 {
                return;
            }
            let zcode = win::zcode_hwnd();
            unsafe { win::apply_bar_styles(hwnd, zcode) };
            let (rl, rt, rr, rb) = win::window_rect(hwnd);
            let c = cfg_t.lock().unwrap();
            let (x, y) = settings::dock_position_ex(
                &c.anchor, &c.scope, c.offset_x, c.offset_y, rr - rl, rb - rt, zcode,
            );
            drop(c);
            unsafe { win::move_window(hwnd, x, y) };
        },
    );
    std::mem::forget(timer);

    // 退出信号线程（--quit 命名事件）
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

    // ── 设置窗口回调（open_panel 时已挂一次）──
    bind_panel_callbacks(panel.clone(), cfg.clone(), &bar, free_mode.clone());
    // （panel/cfg/free_mode 在 timer 闭包里已各自 clone，这里再用原变量需重新借——
    //   实际 move 已发生在闭包，因此上面用 clone 的副本）


    tracing::info!("Rust 悬浮条启动（托盘+设置面板+模型导入）");
    bar.run().expect("Slint 事件循环异常退出");
    tracing::info!("Rust 悬浮条退出");
}

/// 设置面板的事件绑定只做一次（窗口复用）。
fn bind_panel_callbacks(
    panel: Arc<Mutex<Option<slint::Weak<SettingsWindow>>>>,
    cfg: Arc<Mutex<Settings>>,
    _bar: &BarWindow,
    _free_mode: Arc<AtomicBool>,
) {
    let Some(win) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) else { return };
    {
        let cfg_close = cfg.clone();
        let panel = panel.clone();
        win.on_close_panel(move || {
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                let _ = w.hide();
            }
            let _ = cfg_close;
        });
    }
    {
        let panel = panel.clone();
        win.on_set_page(move |p| {
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                w.set_page(p);
            }
        });
    }
    {
        let cfg = cfg.clone();
        let free = _free_mode.clone();
        let panel = panel.clone();
        win.on_anchor_set(move |i| {
            let anchor = ["bottom-left", "bottom-center", "bottom-right",
                          "top-left", "top-center", "top-right"][i as usize % 6].to_string();
            {
                let mut c = cfg.lock().unwrap();
                c.anchor = anchor.clone();
                settings::save(&c);
            }
            free.store(false, Ordering::Relaxed);
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                w.set_anchor_index(i);
            }
        });
    }
    {
        let cfg = cfg.clone();
        win.on_scope_set(move |i| {
            let mut c = cfg.lock().unwrap();
            c.scope = if i == 1 { "screen".into() } else { "window".into() };
            settings::save(&c);
        });
    }
    {
        let cfg = cfg.clone();
        let free = _free_mode.clone();
        win.on_restore_dock(move || {
            {
                let mut c = cfg.lock().unwrap();
                c.anchor = "bottom-center".into();
                settings::save(&c);
            }
            free.store(false, Ordering::Relaxed);
        });
    }
    {
        let cfg = cfg.clone();
        let ui = _bar.as_weak();
        win.on_opacity_changed(move |v| {
            let mut c = cfg.lock().unwrap();
            c.opacity = v as f64 / 100.0;
            settings::save(&c);
            if let Some(bar) = ui.upgrade() {
                bar.set_opacity_level(c.opacity as f32);
            }
        });
    }
    {
        let cfg = cfg.clone();
        let ui = _bar.as_weak();
        win.on_font_changed(move |v| {
            let mut c = cfg.lock().unwrap();
            c.font_px = v;
            settings::save(&c);
            if let Some(bar) = ui.upgrade() {
                bar.set_font_px(v);
            }
        });
    }
    {
        let cfg = cfg.clone();
        let ui = _bar.as_weak();
        win.on_show_toggled(move |name, on| {
            let name = name.to_string();
            let mut c = cfg.lock().unwrap();
            match name.as_str() {
                "speed" => c.show.speed = on,
                "ctx" => c.show.ctx = on,
                "turn" => c.show.turn = on,
                "session" => c.show.session = on,
                "today" => c.show.today = on,
                "tools" => c.show.tools = on,
                "cache" => c.show.cache = on,
                _ => {}
            }
            settings::save(&c);
            if let Some(bar) = ui.upgrade() {
                bar.set_labels_on(c.labels);
            }
        });
    }
    {
        let cfg = cfg.clone();
        win.on_labels_toggled(move |on| {
            let mut c = cfg.lock().unwrap();
            c.labels = on;
            settings::save(&c);
        });
    }
    // ── 面板拖动（标题行按住拖动；坐标同条面全走 GetCursorPos）──
    {
        let panel = panel.clone();
        win.on_drag_panel_start(move || {
            let hwnd = win::own_hwnd("zcode-token-bar-settings");
            let (wx, wy, _, _) = win::window_rect(hwnd);
            let (cx, cy) = win::cursor_pos();
            *DRAG.lock().unwrap() = Some((wx, wy, cx, cy));
        });
        win.on_drag_panel_move(move || {
            let mut g = DRAG.lock().unwrap();
            let cur = g.take();
            if let Some((wx, wy, sx, sy)) = cur {
                let (cx, cy) = win::cursor_pos();
                let hwnd = win::own_hwnd("zcode-token-bar-settings");
                unsafe { win::move_window(hwnd, wx + cx - sx, wy + cy - sy) };
                *g = Some((wx, wy, cx, cy));
            }
        });
        win.on_drag_panel_end(move || {
            DRAG.lock().unwrap().take();
        });
    }

    // ── 模型页：状态共享 ──
    let busy = Arc::new(AtomicBool::new(false));
    let provider_index = Arc::new(std::sync::atomic::AtomicI32::new(0));
    {
        let idx = provider_index.clone();
        let panel = panel.clone();
        win.on_provider_changed(move |s| {
            if let Ok(i) = s.to_string().parse::<i32>() {
                idx.store(i, Ordering::Relaxed);
                if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                    w.set_provider_index(i);
                }
            }
        });
    }
    let last_rows: Arc<Mutex<Vec<models::ModelRow>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let panel = panel.clone();
        let busy = busy.clone();
        let idx = provider_index.clone();
        let rows_slot = last_rows.clone();
        win.on_discover_clicked(move || {
            if busy.swap(true, Ordering::Relaxed) {
                return;
            }
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                w.set_models_busy(true);
                w.set_models_status("发现中…（最多 20s/provider）".into());
            }
            let panel = panel.clone();
            let idx = idx.load(Ordering::Relaxed);
            let rows_slot = rows_slot.clone();
            let busy = busy.clone();
            std::thread::spawn(move || {
                let providers = models::configured_providers();
                let result = providers.get(idx as usize)
                    .map(|p| models::discover_provider(&p.id))
                    .unwrap_or_else(|| Err("请先选择 provider。".into()));
                let _ = slint::invoke_from_event_loop(move || {
                    busy.store(false, Ordering::Relaxed);
                    if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                        w.set_models_busy(false);
                        match result {
                            Ok(d) => {
                                w.set_models_status(format!(
                                    "{}：{} 个模型。{}",
                                    d.provider, d.rows.len(),
                                    if d.source_note.is_empty() { "点「导入到 ZCode」写入配置。" } else { &d.source_note }
                                ).into());
                                let ids: Vec<slint::SharedString> = d.rows.iter()
                                    .map(|r| r.id.clone().into()).collect();
                                *rows_slot.lock().unwrap() = d.rows;
                                w.set_model_ids(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(ids))));
                            }
                            Err(e) => {
                                w.set_models_status(format!("发现失败：{e}").into());
                            }
                        }
                    }
                });
            });
        });
    }
    {
        let panel = panel.clone();
        let busy = busy.clone();
        let rows_slot = last_rows.clone();
        win.on_import_clicked(move || {
            if busy.load(Ordering::Relaxed) {
                return;
            }
            let rows = rows_slot.lock().unwrap().clone();
            if rows.is_empty() {
                return;
            }
            // 按 provider 分组
            let mut groups: Vec<(String, Vec<models::ModelRow>)> = Vec::new();
            for r in rows {
                match groups.iter_mut().find(|(p, _)| *p == r.provider_id) {
                    Some((_, v)) => v.push(r),
                    None => groups.push((r.provider_id.clone(), vec![r])),
                }
            }
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                w.set_models_status("正在写入（先备份）…".into());
            }
            let panel = panel.clone();
            let busy = busy.clone();
            std::thread::spawn(move || {
                let result = models::import_groups(&groups);
                let applied = models::verify_import_applied(&result.added_ids);
                let _ = slint::invoke_from_event_loop(move || {
                    busy.store(false, Ordering::Relaxed);
                    if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                        if result.error.is_empty() && applied {
                            w.set_models_status(format!(
                                "已导入 {} 个模型，立即生效（不用重启）。{}",
                                result.added,
                                if result.skipped.is_empty() { String::new() }
                                else { format!("跳过 {} 个已存在。", result.skipped.len()) }
                            ).into());
                        } else if !applied {
                            w.set_models_status("写入被 ZCode 覆盖：请完全退出 ZCode（含托盘）后重试，或稍后在 Python 面板里导入。".into());
                        } else {
                            w.set_models_status(format!("导入失败：{}", result.error).into());
                        }
                    }
                });
            });
        });
    }
    {
        let panel = panel.clone();
        let busy = busy.clone();
        let rows_slot = last_rows.clone();
        win.on_import_id_clicked(move |text| {
            let text = text.to_string();
            if busy.load(Ordering::Relaxed) || text.trim().is_empty() {
                return;
            }
            let ids: Vec<String> = text.split([',', '，', ' ', '\t'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if ids.is_empty() {
                return;
            }
            if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                w.set_models_status("正在写入…".into());
            }
            let panel = panel.clone();
            let busy = busy.clone();
            let rows_slot = rows_slot.clone();
            let idx = provider_index.clone();
            std::thread::spawn(move || {
                // 目标 provider：发现结果里已有该 provider 的行则沿用其归属；
                // 否则取 provider 列表当前选中项。
                let provider_id = {
                    let rows = rows_slot.lock().unwrap();
                    rows.first().map(|r| r.provider_id.clone())
                };
                let provider_id = match provider_id {
                    Some(p) => p,
                    None => {
                        let i = idx.load(Ordering::Relaxed);
                        models::configured_providers().get(i as usize)
                            .map(|p| p.id.clone())
                            .unwrap_or_default()
                    }
                };
                let rows = ids.into_iter().map(|id| models::ModelRow {
                    provider: provider_id.clone(),
                    provider_id: provider_id.clone(),
                    id,
                }).collect();
                let result = models::import_groups(&[(provider_id, rows)]);
                let applied = models::verify_import_applied(&result.added_ids);
                let _ = slint::invoke_from_event_loop(move || {
                    busy.store(false, Ordering::Relaxed);
                    if let Some(w) = panel.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
                        if result.error.is_empty() && applied {
                            w.set_models_status(format!("已导入 {} 个模型，立即生效。", result.added).into());
                        } else if !applied {
                            w.set_models_status("写入被 ZCode 覆盖：请完全退出 ZCode 后重试。".into());
                        } else {
                            w.set_models_status(format!("导入失败：{}", result.error).into());
                        }
                    }
                });
            });
        });
    }
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

/// 快照 → 条面控件；show 开关决定哪些项显示（关了就输出空串，Slint 侧隐藏）。
fn apply_snapshot(bar: &BarWindow, s: &data::Snapshot, show: &settings::ShowItems, labels: bool) {
    bar.set_labels_on(labels);

    let tps = s.last.tps;
    bar.set_speed_text(if show.speed && tps > 0.0 { format!("{tps:.1}").into() } else { "".into() });
    bar.set_speed_level(if tps >= 70.0 { 0 } else if tps >= 40.0 { 1 } else { 2 });

    let cw = s.context_window;
    let ctx = s.ctx;
    if show.ctx && cw > 0 && ctx > 0 {
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

    let lt = &s.last_turn;
    if show.turn && (lt.total > 0 || lt.requests > 0) {
        bar.set_turn_text(data::fmt_tokens(lt.total).into());
        let secs = lt.duration_ms / 1000;
        let tail = if secs > 0 { format!(" {}s", secs) } else { String::new() };
        bar.set_turn_sub(format!("{}次{}", lt.requests, tail).into());
    } else {
        bar.set_turn_text("".into());
        bar.set_turn_sub("".into());
    }

    if show.session && s.total > 0 {
        bar.set_session_text(data::fmt_tokens(s.total).into());
        bar.set_session_sub(format!("{}轮 {}次", s.turns, s.requests).into());
    } else {
        bar.set_session_text("".into());
        bar.set_session_sub("".into());
    }

    bar.set_today_text(if show.today && s.today.total > 0 { s.today_text.clone().into() } else { "".into() });

    if show.cache && s.input > 0 {
        let rate = s.cache_read as f64 / s.input as f64 * 100.0;
        bar.set_cache_text(format!("{rate:.0}%").into());
        bar.set_cache_level(if rate >= 60.0 { 0 } else if rate >= 30.0 { 1 } else { 2 });
    } else {
        bar.set_cache_text("".into());
    }
}

/// 默认上下文窗口由 settings 提供；这里仅留工具函数（泵直接读 settings）。
fn default_context_window() -> i64 {
    settings::load().context_window
}

