#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod companion;
mod control;
mod data;
mod model_ui;
mod models;
mod recovery;
mod settings;
mod storage;
mod system;
mod update;
mod wallpaper;
mod win;

slint::include_modules!();
use std::{path::PathBuf,sync::OnceLock};
static DATA_DIR:OnceLock<PathBuf>=OnceLock::new();
pub fn data_dir()->PathBuf {
    DATA_DIR.get_or_init(||system::AppPaths::discover().map(|p|p.data_dir).unwrap_or_else(|_|{
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("ZCodeTokenBar")
    })).clone()
}
fn logging()->tracing_appender::non_blocking::WorkerGuard {
    let dir=data_dir().join("logs");let _=std::fs::create_dir_all(&dir);
    let appender=tracing_appender::rolling::daily(dir,"hoverbar-rs.log");
    let(writer,guard)=tracing_appender::non_blocking(appender);
    let _=tracing_subscriber::fmt().with_writer(writer).with_ansi(false).with_target(false).try_init();guard
}
fn main(){if let Err(e)=run(){eprintln!("{e}");tracing::error!("{e}");std::process::exit(1);}}
fn run()->Result<(),String>{
    if system::handle_internal_command()?||update::handle_internal_command()?{return Ok(())}
    let args:Vec<String>=std::env::args().skip(1).collect();
    if control::handle_cli(&args)?||data::handle_cli(&args){return Ok(())}
    let paths=system::AppPaths::discover()?;paths.ensure_dirs()?;let _=DATA_DIR.set(paths.data_dir.clone());
    let _log_guard=logging();
    let has=|flag:&str|args.iter().any(|s|s==flag);
    if has("--quit"){println!("{}",if win::signal("quit"){"已通知悬浮条退出"}else{"悬浮条未运行"});return Ok(())}
    if has("--help") {println!("ZCode Token\n--settings 设置\n--wallpaper 壁纸\n--companion 陪伴\n--wait-zcode 托盘待命\n--quit 退出\n--once 快照\n--doctor 自检\n--discover <provider-id> 模型发现\n--check-update 检查更新\n--data-json / --data-serve 远端数据服务");return Ok(())}
    if has("--once") {let c=settings::load();let mut r=data::DataReader::new();let s=r.snapshot(c.context_window,if c.pinned{&c.pinned_sid}else{""});println!("{}",serde_json::to_string_pretty(&s).map_err(|e|e.to_string())?);return Ok(())}
    if has("--doctor") {println!("{}",app::diagnostics(&paths,&settings::load(),None));return Ok(())}
    if has("--discover") {
        let i=args.iter().position(|s|s=="--discover").unwrap();let pid=args.get(i+1).ok_or("--discover 后需要 provider ID")?;
        let service=models::ModelsService::from_env()?;let d=service.discover(pid,&models::Operation::discovery())?;
        println!("{}",serde_json::to_string_pretty(&d).map_err(|e|e.to_string())?);return Ok(())
    }
    if has("--check-update") {
        let c=update::UpdateClient::new(update::Repository::default())?;
        let installed=update::InstalledVersion::detect(env!("CARGO_PKG_VERSION"),None)?;
        let info=c.check(&installed)?;println!("{}",info.message);return Ok(())
    }
    let instance=match win::SingleInstance::acquire()? {
        win::Instance::Primary(i)=>i,
        win::Instance::Existing=>{if !has("--control-start"){win::signal(if has("--details"){"details"}else if has("--wallpaper"){"wallpaper"}else if has("--companion"){"companion"}else{"settings"});}return Ok(())},
    };
    let page=if has("--wallpaper"){Some(3)}else if has("--companion"){Some(8)}else if has("--settings"){Some(0)}else{None};
    slint::BackendSelector::new().backend_name("winit".into()).renderer_name("femtovg".into()).select().map_err(|e|format!("图形后端初始化失败：{e}"))?;
    app::run(paths,instance,page,has("--wait-zcode"))
}
