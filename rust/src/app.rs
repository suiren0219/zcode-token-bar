//! UI 状态只在事件循环线程修改；网络、图像和统计通过队列回传。
use crate::{data,model_ui::ModelUi,settings::{self,Settings},system,update,wallpaper,win,BarWindow,DetailsWindow,HoverWindow,SettingsBridge,SettingsWindow,ToastWindow};
use slint::{ComponentHandle,SharedString};
use std::{cell::RefCell,collections::VecDeque,path::{Path,PathBuf},rc::Rc,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering},mpsc::{self,Receiver,Sender}},time::{Duration,Instant}};
use tray_icon::{TrayIcon,TrayIconBuilder,TrayIconEvent,MouseButton,MouseButtonState,menu::{ContextMenu,Menu,MenuItem,PredefinedMenuItem,MenuEvent}};

enum Input { Action(String), Bool(String,bool), Number(String,f32), Text(String,String), Provider(i32), Select(i32), Filter(String), Manual(String), Page(i32), DragStart,DragMove,DragEnd,Hover(bool),WallpaperDrop(PathBuf) }
enum Output {
    Snapshot(data::Snapshot),
    Wallpaper(u64,wallpaper::WallpaperSettings,Result<wallpaper::WallpaperReport,String>),
    Health(u64,Result<wallpaper::WallpaperHealth,String>),
    Update(Result<update::CheckResult,String>), UpdateReady(Result<update::PreparedUpdate,String>),
    Reports(u64,Result<data::UsageReports,String>),
    CompanionCandidates(String,Result<Vec<crate::companion::ModelCandidate>,String>),
    WallpaperPreview(String,Result<(u32,u32,Vec<u8>),String>),
    Recovery(Result<crate::recovery::RecoveryReport,String>),
}
#[derive(Clone,Copy)]struct Drag {origin:(i32,i32),mouse:(i32,i32),moved:bool}
struct App {
    bar:BarWindow,panel:SettingsWindow,details:DetailsWindow,hover:HoverWindow,toast:ToastWindow,
    paths:system::AppPaths,instance:win::SingleInstance,_tray:TrayIcon,context_menu:Menu,
    cfg:Arc<Mutex<Settings>>,committed:Settings,inputs:Rc<RefCell<VecDeque<Input>>>,tx:Sender<Output>,rx:Receiver<Output>,stop:Arc<AtomicBool>,
    snapshot:data::Snapshot,models:ModelUi,drag:Option<Drag>,wp_busy:bool,wp_generation:u64,wp_due:Option<Instant>,wp_checked:Instant,wp_message:String,
    status:String,last_layout:Instant,last_health:Instant,waiting_zcode:bool,positioned:bool,panel_was_visible:bool,panel_place:bool,
    hovering:Option<Instant>,update_info:Option<update::CheckResult>,update_busy:bool,
    companion:Option<crate::companion::Runtime>,companion_draft:String,toast_until:Option<Instant>,
    companion_candidates:Vec<crate::companion::ModelCandidate>,companion_candidates_path:String,
    preview_path:String,preview_due:Option<Instant>,report_generation:u64,showing_report:bool,
    last_panel_opacity:Option<f64>,hover_hide_at:Option<Instant>,host_available:bool,last_bar_position:Option<(i32,i32)>,
    recovery_checked:Instant,recovery_busy:bool,
}
const POSITION_FIELDS:&[&str]=&["anchor","scope","offset_x","offset_y","free_x","free_y","free_position_valid","pinned","pinned_sid","pin_x","pin_y","position_locked","locked_x","locked_y"];
fn queue(q:&Rc<RefCell<VecDeque<Input>>>,e:Input){q.borrow_mut().push_back(e)}

pub fn run(paths:system::AppPaths,instance:win::SingleInstance,page:Option<i32>,wait:bool)->Result<(),String>{
    let (mut cfg,load_error)=settings::load_with_report();
    let had_wallpaper=std::fs::read(settings::settings_path()).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|v|v.get("wallpaper").is_some());
    let mut startup_note=load_error.unwrap_or_default();
    if !had_wallpaper {match wallpaper::migrate(&paths.data_dir){Ok(Some(w))=>cfg.wallpaper=w,Ok(None)=>{},Err(e)=>startup_note=format!("旧壁纸迁移：{e}")}}
    cfg.normalize();
    let policy=legacy_policy(&paths);
    if let Ok(s)=system::autostart_status(&paths,&policy){cfg.autostart=s.enabled;}
    let bar=BarWindow::new().map_err(|e|e.to_string())?;
    let panel=SettingsWindow::new().map_err(|e|e.to_string())?;
    let details=DetailsWindow::new().map_err(|e|e.to_string())?;
    let hover=HoverWindow::new().map_err(|e|e.to_string())?;
    let toast=ToastWindow::new().map_err(|e|e.to_string())?;
    let q=Rc::new(RefCell::new(VecDeque::new()));
    bind(&bar,&panel,&details,&toast,&q);
    let (tx,rx)=mpsc::channel();
    let shared=Arc::new(Mutex::new(cfg.clone()));let stop=Arc::new(AtomicBool::new(false));
    let models=ModelUi::new(&panel);
    let (tray,context_menu)=tray()?;
    let mut app=App{bar,panel,details,hover,toast,paths,instance,_tray:tray,context_menu,cfg:shared.clone(),committed:cfg,
        inputs:q,tx:tx.clone(),rx,stop:stop.clone(),snapshot:data::Snapshot::default(),models,drag:None,
        wp_busy:false,wp_generation:0,wp_due:None,wp_checked:Instant::now()-Duration::from_secs(60),wp_message:String::new(),
        status:startup_note,last_layout:Instant::now()-Duration::from_secs(1),last_health:Instant::now()-Duration::from_secs(10),waiting_zcode:wait,positioned:false,
        panel_was_visible:false,panel_place:false,hovering:None,update_info:None,update_busy:false,companion:None,companion_draft:String::new(),toast_until:None,
        companion_candidates:Vec::new(),companion_candidates_path:String::new(),preview_path:String::new(),preview_due:Some(Instant::now()),report_generation:0,showing_report:false,last_panel_opacity:None,hover_hide_at:None,host_available:!win::minimized(win::zcode_hwnd()),last_bar_position:None,recovery_checked:Instant::now()-Duration::from_secs(30),recovery_busy:false};
    app.sync_settings();app.preview();app.configure_companion();
    app.bar.show().map_err(|e|e.to_string())?;
    if let Some(page)=page {app.open_settings(page);}
    // 空闲时每次最多一个数据查询；失败由 DataReader 下次重新建立只读连接。
    std::thread::spawn(move||{
        let mut reader=data::DataReader::new();
        while !stop.load(Ordering::Acquire){
            let c=shared.lock().unwrap().clone();let sid=if c.pinned{c.pinned_sid.as_str()}else{""};
            let snap=match data::RemoteConfig::from_value(&c.remote){
                Ok(Some(remote))=>reader.snapshot_remote(c.context_window,sid,&remote),
                Ok(None)=>reader.snapshot(c.context_window,sid),
                Err(e)=>data::Snapshot{error:format!("远端配置：{e}"),..Default::default()},
            };
            if tx.send(Output::Snapshot(snap)).is_err(){break}
            let deadline=Instant::now()+Duration::from_millis(c.poll_ms);
            while Instant::now()<deadline&&!stop.load(Ordering::Acquire){std::thread::sleep(Duration::from_millis(100));}
        }
    });
    if app.committed.auto_update {app.check_update();}
    let rc=Rc::new(RefCell::new(app));let timer=slint::Timer::default();let weak=Rc::downgrade(&rc);
    timer.start(slint::TimerMode::Repeated,Duration::from_millis(100),move||{if let Some(app)=weak.upgrade(){app.borrow_mut().tick();}});
    let result=slint::run_event_loop_until_quit().map_err(|e|e.to_string());
    timer.stop();rc.borrow().stop.store(true,Ordering::Release);
    result
}
fn bind(bar:&BarWindow,p:&SettingsWindow,d:&DetailsWindow,t:&ToastWindow,q:&Rc<RefCell<VecDeque<Input>>>){
    let x=q.clone();bar.on_action(move|a|queue(&x,Input::Action(a.to_string())));
    let x=q.clone();bar.on_drag_start(move||queue(&x,Input::DragStart));
    let x=q.clone();bar.on_drag_move(move||queue(&x,Input::DragMove));
    let x=q.clone();bar.on_drag_end(move||queue(&x,Input::DragEnd));
    let x=q.clone();bar.on_hovering(move|v|queue(&x,Input::Hover(v)));
    let g=p.global::<SettingsBridge>();
    let x=q.clone();g.on_action(move|a|queue(&x,Input::Action(a.to_string())));
    let x=q.clone();g.on_bool_change(move|k,v|queue(&x,Input::Bool(k.to_string(),v)));
    let x=q.clone();g.on_number_change(move|k,v|queue(&x,Input::Number(k.to_string(),v)));
    let x=q.clone();g.on_text_change(move|k,v|queue(&x,Input::Text(k.to_string(),v.to_string())));
    let x=q.clone();g.on_provider_change(move|i|queue(&x,Input::Provider(i)));
    let x=q.clone();g.on_model_select(move|i|queue(&x,Input::Select(i)));
    let x=q.clone();g.on_model_filter(move|s|queue(&x,Input::Filter(s.to_string())));
    let x=q.clone();g.on_manual_import(move|s|queue(&x,Input::Manual(s.to_string())));
    let x=q.clone();g.on_page_change(move|i|queue(&x,Input::Page(i)));
    let x=q.clone();p.window().on_close_requested(move||{queue(&x,Input::Action("close-panel".into()));slint::CloseRequestResponse::KeepWindowShown});
    let x=q.clone();d.on_action(move|a|queue(&x,Input::Action(a.to_string())));
    let x=q.clone();d.window().on_close_requested(move||{queue(&x,Input::Action("close-details".into()));slint::CloseRequestResponse::KeepWindowShown});
    let x=q.clone();t.on_clicked(move||queue(&x,Input::Action("open-companion".into())));
    use slint::winit_030::{WinitWindowAccessor,winit::event::WindowEvent,EventResult};
    for window in [bar.window(),p.window()] {
        let x=q.clone();
        window.on_winit_window_event(move|_,e|{
            if let WindowEvent::DroppedFile(path)=e {queue(&x,Input::WallpaperDrop(path.clone()));}
            EventResult::Propagate
        });
    }
}
fn tray()->Result<(TrayIcon,Menu),String>{
    fn menu()->Result<Menu,String>{let m=Menu::new();for(id,label)in [
        ("settings","打开设置"),("wallpaper","壁纸设置"),("pin","钉住 / 解除钉住"),("details","查看用量详情"),("restore-dock","恢复自动贴靠"),("toggle-bar","显示 / 隐藏悬浮条"),("open-companion","小亚陪伴"),("check-update","检查更新")]{m.append(&MenuItem::with_id(id,label,true,None)).map_err(|e|e.to_string())?;}
        m.append(&PredefinedMenuItem::separator()).map_err(|e|e.to_string())?;m.append(&MenuItem::with_id("quit","退出悬浮条",true,None)).map_err(|e|e.to_string())?;Ok(m)}
    let icon=tray_icon::Icon::from_rgba(include_bytes!("tray.rgba").to_vec(),32,32).map_err(|e|e.to_string())?;
    let t=TrayIconBuilder::new().with_tooltip("ZCode Token · 用量与模型管理").with_icon(icon).with_menu_on_left_click(false).with_menu(Box::new(menu()?)).build().map_err(|e|e.to_string())?;
    Ok((t,menu()?))
}
impl App {
    fn tick(&mut self){
        if let Some(a)=self.instance.poll(){self.action(a);}
        while let Ok(e)=MenuEvent::receiver().try_recv(){self.action(e.id.as_ref());}
        while let Ok(e)=TrayIconEvent::receiver().try_recv(){if matches!(e,TrayIconEvent::Click{button:MouseButton::Left,button_state:MouseButtonState::Up,..}|TrayIconEvent::DoubleClick{button:MouseButton::Left,..}){let page=self.cfg.lock().unwrap().last_page;self.open_settings(page);}}
        let commands:Vec<_>=self.inputs.borrow_mut().drain(..).collect();
        for command in commands {match command {
            Input::Action(s)=>self.action(&s),Input::Bool(k,v)=>self.bool_change(&k,v),Input::Number(k,v)=>self.number_change(&k,v),Input::Text(k,v)=>self.text_change(&k,&v),
            Input::Provider(i)=>self.models.provider_change(i),Input::Select(i)=>self.models.select(i),Input::Filter(s)=>self.models.filter(&s),Input::Manual(s)=>self.models.manual_import(&s),
            Input::Page(p)=>{self.cfg.lock().unwrap().last_page=p;if p==6{self.refresh_diagnostics();}},
            Input::DragStart=>self.drag_start(),Input::DragMove=>self.drag_move(),Input::DragEnd=>self.drag_end(),
            Input::Hover(v)=>{self.hovering=if v{Some(Instant::now())}else{None};self.hover_hide_at=if v{None}else{Some(Instant::now()+Duration::from_millis(350))};},
            Input::WallpaperDrop(path)=>{self.open_settings(3);self.select_wallpaper(path);},
        }}
        while let Ok(e)=self.rx.try_recv(){self.output(e);}
        self.models.tick();
        if self.preview_due.is_some_and(|at|Instant::now()>=at){self.preview_due=None;self.load_wallpaper_preview();}
        if self.drag.is_some()&&!win::left_down(){self.drag_end();}
        if self.last_layout.elapsed()>Duration::from_millis(180){self.layout();self.last_layout=Instant::now();}
        if let Some(at)=self.wp_due {if Instant::now()>=at&&!self.wp_busy {self.wp_due=None;self.start_wallpaper("apply");}}
        if self.wp_checked.elapsed()>Duration::from_secs(8)&&!self.wp_busy&&{let c=self.cfg.lock().unwrap();c.wallpaper.enabled||!c.wallpaper.path.is_empty()} {self.start_wallpaper("health");}
        if self.recovery_checked.elapsed()>Duration::from_secs(20)&&!self.recovery_busy&&!self.wp_busy {self.start_recovery(false);}
        self.companion_tick();
        if self.last_health.elapsed()>Duration::from_secs(5){self.publish_state();if self.panel.window().is_visible()&&self.panel.get_page()==6{self.refresh_diagnostics();}self.last_health=Instant::now();}
        if self.hovering.is_some_and(|t|t.elapsed()>Duration::from_millis(650))&&!self.panel.window().is_visible()&&self.drag.is_none(){self.show_hover();}
        if self.hover.window().is_visible(){
            let (x,y)=win::cursor_pos();let over=win::rect(win::slint_handle(self.hover.window())).is_some_and(|r|x>=r.left&&x<r.right&&y>=r.top&&y<r.bottom);
            if over {self.hover_hide_at=None;}else if self.hovering.is_none(){
                if self.hover_hide_at.is_none(){self.hover_hide_at=Some(Instant::now()+Duration::from_millis(300));}
                if self.hover_hide_at.is_some_and(|t|Instant::now()>t){let _=self.hover.hide();self.hover_hide_at=None;}
            }
        }
        if self.toast_until.is_some_and(|d|Instant::now()>d){self.toast_until=None;let _=self.toast.hide();}
        self.panel.global::<SettingsBridge>().set_status(self.status.clone().into());
        self.panel.global::<SettingsBridge>().set_busy(self.update_busy);
    }
    fn output(&mut self,event:Output){match event {
        Output::Snapshot(s)=>{
            if s.pin_lost&&s.error.is_empty(){let mut c=self.cfg.lock().unwrap();c.pinned=false;c.pinned_sid.clear();drop(c);self.save_immediate(&["pinned","pinned_sid"]);self.status="已钉住的会话不再存在，已恢复跟随。".into();self.sync_settings();}
            self.snapshot=s;self.preview();
            if self.hover.window().is_visible(){self.hover.set_content_text(self.detail_text().into());}
            if self.details.window().is_visible()&&!self.showing_report{self.details.set_content_text(self.detail_text().into());}
        },
        Output::Wallpaper(generation,settings,result)=>{
            self.wp_busy=false;self.wp_checked=Instant::now();
            if generation==self.wp_generation {
                self.cfg.lock().unwrap().wallpaper=settings;
                self.save_immediate(&["wallpaper"]);
                self.wp_message=match result{Ok(r)=>r.message,Err(e)=>e};
                self.sync_settings();self.preview();self.preview_due=Some(Instant::now());
            }
            self.panel.set_wp_busy(false);
        },
        Output::Health(generation,result)=>{
            self.wp_busy=false;self.wp_checked=Instant::now();
            if generation==self.wp_generation {match result{Ok(h)=>{self.wp_message=h.message;if h.needs_apply {self.start_wallpaper("apply");}},Err(e)=>self.wp_message=e};}
            self.panel.set_wp_status(self.wp_message.clone().into());self.panel.set_wp_busy(self.wp_busy);
        },
        Output::Update(r)=>{self.update_busy=false;match r{Ok(info)=>{self.panel.set_update_status(info.message.clone().into());self.panel.set_update_available(info.can_update());self.update_info=Some(info);},Err(e)=>self.panel.set_update_status(format!("检查失败：{e}").into())}},
        Output::UpdateReady(r)=>{self.update_busy=false;match r{Ok(prepared)=>match prepared.launch(){Ok(_)=>self.quit(),Err(e)=>self.status=e},Err(e)=>self.status=e}},
        Output::Recovery(result)=>{
            self.recovery_busy=false;self.recovery_checked=Instant::now();
            let message=match result{Ok(r)=>r.message,Err(e)=>e};
            self.panel.set_recovery_status(message.into());self.panel.set_wp_busy(self.wp_busy);
        },
        Output::Reports(g,result)=>{if g==self.report_generation&&self.showing_report{self.details.set_content_text(match result{Ok(report)=>report.detail_text,Err(e)=>format!("报表读取失败：{e}")}.into());}},
        Output::CompanionCandidates(path,result)=>{
            if path==self.cfg.lock().unwrap().companion_config {
                self.companion_candidates=result.unwrap_or_default();self.sync_companion_candidates();
            }
        },
        Output::WallpaperPreview(path,result)=>{
            if path==self.cfg.lock().unwrap().wallpaper.path {
                match result {
                    Ok((w,h,rgba))=>{let buffer=slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba,w,h);self.panel.set_w_preview(slint::Image::from_rgba8(buffer));self.panel.set_wp_preview_ready(true);},
                    Err(e)=>{self.panel.set_wp_preview_ready(false);self.panel.set_wp_status(e.into());},
                }
            }
        },
    }}
    fn persist(&mut self)->bool {
        let mut c=self.cfg.lock().unwrap().clone();c.normalize();
        match settings::save(&c){Ok(())=>{self.committed=c.clone();*self.cfg.lock().unwrap()=c;true},Err(e)=>{self.status=format!("保存失败：{e}");false}}
    }
    fn save_immediate(&mut self,keys:&[&str])->bool {
        let current=self.cfg.lock().unwrap().clone();
        match settings::merge_fields(&self.committed,&current,keys).and_then(|saved|{settings::save(&saved)?;Ok(saved)}) {
            Ok(saved)=>{self.committed=saved;true},Err(e)=>{self.status=format!("保存失败：{e}");false}
        }
    }
    fn save_wallpaper_intent(&mut self){
        self.wp_generation+=1;
        self.save_immediate(&["wallpaper"]);
        self.wp_due=Some(Instant::now()+Duration::from_millis(600));
    }
    fn open_settings(&mut self,page:i32){
        self.panel.set_page(page.clamp(0,8));self.cfg.lock().unwrap().last_page=page.clamp(0,8);
        if !self.panel.window().is_visible(){
            let c=self.cfg.lock().unwrap();
            self.panel.window().set_size(slint::LogicalSize::new(c.panel_width as f32,c.panel_height as f32));
        }
        self.sync_settings();self.sync_companion_candidates();self.models.refresh();let _=self.panel.show();self.panel_place=true;self.panel_was_visible=true;
        self.preview_due=Some(Instant::now());
        self.refresh_diagnostics();self.status="修改会立即预览；点击“应用”保存，关闭可撤销未保存的普通设置。".into();
        let _=self.hover.hide();
    }
    fn close_settings(&mut self,save:bool){
        if save {
            if !self.persist(){return}
            self.status="设置已保存".into();
        } else {
            *self.cfg.lock().unwrap()=self.committed.clone();
            self.positioned=false;
        }
        self.preview();self.configure_companion();
        let size=self.panel.window().size();let scale=self.panel.window().scale_factor().max(0.5);
        if let Some(r)=win::rect(win::slint_handle(self.panel.window())){
            let mut c=self.cfg.lock().unwrap();c.panel_x=r.left;c.panel_y=r.top;c.panel_position_valid=true;
            c.panel_width=(size.width as f32/scale).round() as i32;c.panel_height=(size.height as f32/scale).round() as i32;
        }
        self.save_immediate(&["panel_x","panel_y","panel_position_valid","panel_width","panel_height","last_page"]);
        let _=self.panel.hide();self.panel_was_visible=false;
    }
    fn status_result<T>(&mut self,r:Result<T,String>,success:&str){self.status=match r{Ok(_)=>success.into(),Err(e)=>e};}
    fn action(&mut self,name:&str){
        if self.models.action(name){return}
        match name {
            "settings"=>{let page=self.cfg.lock().unwrap().last_page;self.open_settings(page)},"wallpaper"=>self.open_settings(3),"open-companion"|"companion"=>self.open_settings(8),
            "close-panel"=>self.close_settings(false),"apply"=>self.close_settings(true),"quit"=>self.quit(),
            "restore-defaults"=>{let wp=self.cfg.lock().unwrap().wallpaper.clone();*self.cfg.lock().unwrap()=Settings::default();self.cfg.lock().unwrap().wallpaper=wp;self.sync_settings();self.preview();self.status="已恢复悬浮条默认值；点击应用保存。壁纸保留。".into();},
            "restore-python-settings"=>{match settings::reload_legacy(){Some(mut c)=>{c.wallpaper=self.cfg.lock().unwrap().wallpaper.clone();*self.cfg.lock().unwrap()=c;self.sync_settings();self.preview();self.status="旧版设置已导入预览；点击应用保存。".into()},None=>self.status="未找到可识别的旧版悬浮条设置。".into()}},
            "restore-dock"=>{self.cfg.lock().unwrap().restore_dock();self.positioned=false;self.sync_settings();self.layout();if self.save_immediate(POSITION_FIELDS){self.status="已恢复跟随 ZCode 窗口的左下贴靠。".into();}},
            "restore-free-position"=>{let valid=self.cfg.lock().unwrap().free_position_valid;if valid{self.cfg.lock().unwrap().anchor="free".into();self.positioned=false;self.layout();self.sync_settings();self.save_immediate(POSITION_FIELDS);}else{self.status="尚未保存自由位置；先拖动条面。".into()}},
            "save-free-position"=>{self.cfg.lock().unwrap().anchor="free".into();self.cfg.lock().unwrap().free_position_valid=true;self.positioned=false;self.layout();self.save_immediate(POSITION_FIELDS);self.sync_settings();},
            "pin"=>self.toggle_pin(None),"toggle-bar"=>{let h=self.cfg.lock().unwrap().hidden;self.cfg.lock().unwrap().hidden=!h;self.save_immediate(&["hidden"]);self.sync_settings();self.layout();},
            "details"|"details-current"=>{self.showing_report=false;self.details.set_content_text(self.detail_text().into());let _=self.details.show();},
            "reports-week"=>self.load_report(7),"reports-month"=>self.load_report(30),
            "close-details"=>{let _=self.details.hide();},"copy-details"=>self.status_result(win::copy_text(self.details.get_content_text().as_str()),"详情已复制"),
            "reload"=>{let(n,_)=settings::load_with_report();self.committed=n.clone();*self.cfg.lock().unwrap()=n;self.sync_settings();self.preview();self.status="配置已重新载入。".into();self.configure_companion();}
            "dump"=>self.publish_state(),
            "menu"=>{let h=win::slint_handle(self.bar.window());unsafe{self.context_menu.show_context_menu_for_hwnd(h, None);}},
            "choose-wallpaper"=>self.choose_wallpaper(),"paste-wallpaper-path"=>{match win::clipboard_text(){Ok(text)=>{let path=text.trim().trim_matches('"');if !path.is_empty(){self.text_change("wp-path",path);self.panel.set_wp_path(path.into());self.status="已粘贴图片路径，点击应用壁纸。".into();}},Err(e)=>self.status=e}},
            "apply-wallpaper"=>{if !self.cfg.lock().unwrap().wallpaper.path.trim().is_empty(){self.cfg.lock().unwrap().wallpaper.enabled=true;}self.save_wallpaper_intent();self.start_wallpaper("apply");},
            "refresh-wallpaper"=>{self.save_wallpaper_intent();self.start_wallpaper("apply");},"check-wallpaper-connection"=>self.start_wallpaper("health"),"reset-theme"=>{self.cfg.lock().unwrap().wallpaper.enabled=false;self.save_wallpaper_intent();self.start_wallpaper("reset");self.sync_settings();},
            "open-wallpaper-directory"=>{let dir=self.paths.data_dir.join("wallpapers");let r=std::fs::create_dir_all(&dir).map_err(|e|e.to_string()).and_then(|_|system::open_path(&dir));self.status_result(r,"已打开壁纸目录")},
            "restore-after-update"=>self.start_recovery(true),
            "repair-launchers"=>self.repair_launchers(),"choose-font"=>self.choose_font(),
            "open-logs"=>self.status_result(system::open_logs(&self.paths),"已打开日志目录"),"open-data-directory"=>self.status_result(system::open_data_dir(&self.paths),"已打开数据目录"),
            "refresh-diagnostics"=>self.refresh_diagnostics(),"copy-diagnostics"=>{let cfg=self.cfg.lock().unwrap().clone();let text=diagnostics(&self.paths,&cfg,Some(&self.snapshot));let result=win::copy_text(&text);self.status_result(result,"诊断已复制");},
            "restart-bar"=>{match system::schedule_restart(&[],&self.paths){Ok(_)=>self.quit(),Err(e)=>self.status=e}},
            "create-desktop-shortcut"=>self.status_result(system::create_desktop_shortcut(&self.paths,&system::ShortcutOptions::default()),"桌面快捷方式已创建"),
            "check-update"=>self.check_update(),"install-update"=>self.install_update(),"open-releases"=>self.status_result(system::open_github_page("https://github.com/suiren0219/zcode-token-bar/releases"),"已打开发布页"),
            n if n.starts_with("companion-")||n=="choose-companion-config"=>self.companion_action(n),
            _=>self.status=format!("未识别的操作：{name}"),
        }
    }
    fn toggle_pin(&mut self,want:Option<bool>){let on=want.unwrap_or(!self.cfg.lock().unwrap().pinned);if on&&self.snapshot.sid.is_empty(){self.status="暂无可钉住会话，等待 ZCode 首次请求。".into();self.sync_settings();return}
        let r=win::rect(win::slint_handle(self.bar.window()));let mut c=self.cfg.lock().unwrap();c.pinned=on;if on{c.pinned_sid=self.snapshot.sid.clone();if let Some(r)=r{c.pin_x=r.left;c.pin_y=r.top;}}else{c.pinned_sid.clear();c.pin_x=-1;c.pin_y=-1;}drop(c);if !self.panel.window().is_visible(){self.save_immediate(&["pinned","pinned_sid","pin_x","pin_y"]);}self.sync_settings();self.preview();}
    fn bool_change(&mut self,key:&str,v:bool){
        if key=="pinned"{self.toggle_pin(Some(v));return}
        if key=="lock-position" {
            if let Some(r)=win::rect(win::slint_handle(self.bar.window())){
                let mut c=self.cfg.lock().unwrap();c.position_locked=v;c.locked_x=r.left;c.locked_y=r.top;
            }
            self.sync_settings();return;
        }
        if key=="auto-start"{let p=legacy_policy(&self.paths);let r=system::set_autostart(v,&["--wait-zcode".into()],&self.paths,&p);match r{Ok(ch)=>{self.cfg.lock().unwrap().autostart=ch.status.enabled;self.save_immediate(&["autostart"]);self.status="自启动设置已更新，原注册项已备份。".into()},Err(e)=>self.status=e}self.sync_settings();return}
        let mut c=self.cfg.lock().unwrap();match key {
            "always-on-top"=>c.always_on_top=v,"hide-when-inactive"=>c.hide_when_inactive=v,"auto-update"=>c.auto_update=v,"bar-visible"=>c.hidden=!v,
            "panel-wallpaper"=>c.panel_wallpaper=v,"auto-client-recovery"=>c.auto_client_recovery=v,
            "show-labels"=>c.labels=v,"show-speed"=>c.show.speed=v,"show-ctx"=>c.show.ctx=v,"show-turn"=>c.show.turn=v,"show-session"=>c.show.session=v,"show-today"=>c.show.today=v,"show-tools"=>c.show.tools=v,"show-cache"=>c.show.cache=v,
            "wp-enabled"=>c.wallpaper.enabled=v,"wp-visible"=>c.wallpaper.visible=v,"wp-monet"=>c.wallpaper.monet=v,
            "companion-enabled"=>c.companion_enabled=v,"companion-auto"=>c.companion_auto=v,"companion-paused"=>c.companion_paused=v,
            "companion-stream"=>c.companion_stream=v,"companion-persist-history"=>c.companion_persist_history=v,
            "remote-enabled"=>{if !c.remote.is_object(){c.remote=serde_json::json!({});}c.remote["enabled"]=v.into();},_=>{},
        }drop(c);self.preview();if key.starts_with("wp-"){self.save_wallpaper_intent();}if key.starts_with("companion-"){self.configure_companion();}
    }
    fn number_change(&mut self,key:&str,v:f32){let n=v.round() as i32;
        if key=="companion-model-index" {
            if let Some(model)=self.companion_candidates.get(n.max(0) as usize){let mut c=self.cfg.lock().unwrap();c.companion_provider_id=model.provider_id.clone();c.companion_model_id=model.model_id.clone();}
            self.configure_companion();return;
        }
        let mut c=self.cfg.lock().unwrap();match key {
        "anchor-index"=>{if let Some(a)=settings::ANCHORS.get(n.max(0) as usize){c.anchor=(*a).into();if c.pinned{c.pin_x=-1;c.pin_y=-1;}}self.positioned=false;},"scope-index"=>{c.scope=if n==1{"screen"}else{"window"}.into();if c.pinned{c.pin_x=-1;c.pin_y=-1;}},
        "offset-x"=>{c.offset_x=n;if c.pinned{c.pin_x=-1;c.pin_y=-1;}},"offset-y"=>{c.offset_y=n;if c.pinned{c.pin_x=-1;c.pin_y=-1;}},"free-x"=>{c.free_x=n;c.free_position_valid=true;self.positioned=false;},"free-y"=>{c.free_y=n;c.free_position_valid=true;self.positioned=false;},
        "opacity-pct"=>c.opacity=v as f64/100.0,"font-px"=>c.font_px=n,"panel-opacity"=>c.panel_opacity=v as f64/100.0,"panel-wallpaper-dim"=>c.panel_wallpaper_dim=n.clamp(35,95)as u32,"poll-ms"=>c.poll_ms=n.max(300) as u64,
        "wp-blur"=>c.wallpaper.blur=v as f64,"wp-dim"=>c.wallpaper.dim=v as f64,"wp-fit"=>c.wallpaper.fit=["cover","contain","smart","stretch"].get(n.max(0)as usize).unwrap_or(&"cover").to_string(),"focus-x"=>c.wallpaper.focus_x=v as f64/100.,"focus-y"=>c.wallpaper.focus_y=v as f64/100.,"wp-port"=>c.wallpaper.port=n.clamp(1024,65535)as u16,
        "companion-cooldown-secs"=>c.companion_cooldown_s=n.max(0)as u64,"companion-hourly"=>c.companion_hourly_limit=n.max(0)as u32,
        "remote-port"=>{if !c.remote.is_object(){c.remote=serde_json::json!({});}c.remote["port"]=n.clamp(1,65535).into();c.remote.as_object_mut().unwrap().remove("ssh");},_=>{},
    }c.normalize();drop(c);self.preview();if key.starts_with("wp-")||key.starts_with("focus-"){self.save_wallpaper_intent();}if key.starts_with("companion-"){self.configure_companion();}}
    fn text_change(&mut self,key:&str,v:&str){if key=="companion-input"{self.companion_draft=v.into();return}
        let mut c=self.cfg.lock().unwrap();match key {
            "font-family"=>c.font_family=v.into(),
            "wp-path"=>{c.wallpaper.path=v.into();self.wp_generation+=1;self.preview_due=Some(Instant::now()+Duration::from_millis(350));},
            "companion-config-path"=>{c.companion_config=v.into();c.companion_provider_id.clear();c.companion_model_id.clear();},
            k if k.starts_with("remote-")=>{
                if !c.remote.is_object(){c.remote=serde_json::json!({});}
                let field=k.trim_start_matches("remote-");
                c.remote[field]=v.into();
                if field=="helper" {c.remote.as_object_mut().unwrap().remove("executable");}
                if ["host","user","key"].contains(&field){c.remote.as_object_mut().unwrap().remove("ssh");}
            },_=>{},
        }drop(c);self.preview();
        if key=="companion-config-path"{self.configure_companion();}
    }
    fn drag_start(&mut self){if {let c=self.cfg.lock().unwrap();c.pinned||c.position_locked}{return}if let Some(r)=win::rect(win::slint_handle(self.bar.window())){self.drag=Some(Drag{origin:(r.left,r.top),mouse:win::cursor_pos(),moved:false});let _=self.hover.hide();}}
    fn drag_move(&mut self){if let Some(ref mut d)=self.drag{let p=win::cursor_pos();let(dx,dy)=(p.0-d.mouse.0,p.1-d.mouse.1);if dx.abs()+dy.abs()>3{d.moved=true;}if d.moved{let h=win::slint_handle(self.bar.window());if let Some(r)=win::rect(h){let work=win::work_area_point(p.0,p.1);let(x,y)=work.clamp(d.origin.0+dx,d.origin.1+dy,r.width(),r.height());win::move_window(h,x,y);}}}}
    fn drag_end(&mut self){if let Some(d)=self.drag.take(){if d.moved{if let Some(r)=win::rect(win::slint_handle(self.bar.window())){let mut c=self.cfg.lock().unwrap();c.anchor="free".into();c.free_x=r.left;c.free_y=r.top;c.free_position_valid=true;drop(c);self.positioned=true;self.save_immediate(POSITION_FIELDS);self.sync_settings();}}}}
    fn layout(&mut self){
        let h=win::slint_handle(self.bar.window());if h==0{return}let z=win::zcode_hwnd();let mut c=self.cfg.lock().unwrap().clone();
        win::apply_bar_styles(h,c.always_on_top);
        if z!=0{self.waiting_zcode=false;}
        let available=!win::minimized(z);
        if self.host_available&&!available&&c.hide_when_inactive {
            if self.panel.window().is_visible(){self.close_settings(false);}
            let _=self.details.hide();let _=self.hover.hide();let _=self.toast.hide();
            c=self.cfg.lock().unwrap().clone();
        }
        self.host_available=available;
        let should_show=!c.hidden&&(!c.hide_when_inactive||available)&&(!self.waiting_zcode||z!=0);
        win::show(h,should_show);
        let position=win::rect(h).map(|r|(r.left+r.width()/2,r.top));
        if let (Some(before),Some(after))=(self.last_bar_position,position){
            if ((before.0-after.0).abs()>8||(before.1-after.1).abs()>8)&&self.panel.window().is_visible(){self.panel_place=true;}
        }
        self.last_bar_position=position;
        self.layout_panel(h,c.panel_opacity);
        if !should_show{let _=self.hover.hide();return}
        let Some(r)=win::rect(h)else{return};let work=if c.anchor=="free"{win::work_area_point(r.left,r.top)}else{win::work_area(if z!=0{z}else{h})};
        let sf=self.bar.window().scale_factor().max(0.5);
        self.bar.set_bar_max_width(((work.width()-12).max(200) as f32/sf).into());
        if self.drag.is_none(){
            let pos=if c.pinned&&c.pin_x!=-1&&c.pin_y!=-1{Some(work.clamp(c.pin_x,c.pin_y,r.width(),r.height()))}
            else if c.position_locked{Some(work.clamp(c.locked_x,c.locked_y,r.width(),r.height()))}
            else if c.anchor=="free"{if !self.positioned&&c.free_position_valid{let w=win::work_area_point(c.free_x,c.free_y);Some(w.clamp(c.free_x,c.free_y,r.width(),r.height()))}else{Some(work.clamp(r.left,r.top,r.width(),r.height()))}}
            else{let target=if c.scope=="window"&&!win::minimized(z){win::rect(z).unwrap_or(work)}else{work};Some(settings::anchored_position(&c.anchor,target,work,r.width(),r.height(),(c.offset_x as f32*sf).round()as i32,(c.offset_y as f32*sf).round()as i32))};
            if let Some((x,y))=pos{win::move_window(h,x,y);self.positioned=true;if c.pinned&&(c.pin_x==-1||c.pin_y==-1){let mut c=self.cfg.lock().unwrap();c.pin_x=x;c.pin_y=y;drop(c);if !self.panel.window().is_visible(){self.save_immediate(&["pin_x","pin_y"]);}}}
        }
    }
    fn layout_panel(&mut self,bar:isize,opacity:f64){
        if !self.panel.window().is_visible(){return}
        let ph=win::slint_handle(self.panel.window());if ph==0{return}
        if self.panel_place {
            let work=win::work_area(if bar!=0{bar}else{ph});
            let sf=self.panel.window().scale_factor().max(0.5);
            let current=self.panel.window().size();
            let width=(current.width as i32).min(work.width()-12).max(480);
            let height=(current.height as i32).min(work.height()-48).max(360);
            if current.width as i32!=width||current.height as i32!=height {
                self.panel.window().set_size(slint::LogicalSize::new(width as f32/sf,height as f32/sf));
            }
            win::place_panel(ph,bar);self.panel.window().request_redraw();self.panel_place=false;win::focus(ph);
        }
        // 设置页的背景与透明度由 Slint 图层统一合成，避免修改 WS_EX_LAYERED
        // 后破坏后端交换缓冲，出现静态文字/分区丢失。
        if self.last_panel_opacity!=Some(opacity){self.panel.window().request_redraw();self.last_panel_opacity=Some(opacity);}
    }
    fn preview(&self){let c=self.cfg.lock().unwrap();self.bar.set_font_px(c.font_px);self.bar.set_font_family(c.font_family.clone().into());self.bar.set_opacity_level(c.opacity as f32);self.bar.set_labels_on(c.labels);self.bar.set_pinned(c.pinned);self.bar.set_companion_enabled(c.companion_enabled);let s=&self.snapshot;
        self.bar.set_active(s.active);self.bar.set_has_data(s.ok&&!s.sid.is_empty());self.bar.set_title_text(if !s.error.is_empty(){"读取数据失败"}else if s.title.is_empty(){"等待会话"}else{&s.title}.into());
        let enabled=|on:bool,text:String|->SharedString{if on{text.into()}else{"".into()}};
        self.bar.set_speed(enabled(c.show.speed&&s.last.tps>0.,format!("{:.1}",s.last.tps)));self.bar.set_speed_level(if s.last.tps>=70.{0}else if s.last.tps>=40.{1}else{2});
        let pct=if s.context_window>0{s.ctx as f64/s.context_window as f64*100.}else{0.};
        self.bar.set_context_text(enabled(c.show.ctx&&s.ctx>0,format!("{}{:.1}%",if s.context_inferred{"≈"}else{""},pct)));self.bar.set_context_percent(pct as f32);
        self.bar.set_context_level(if s.context_exceeded||pct>=if s.context_window>=1_000_000{60.}else{85.}{2}else if pct>=if s.context_window>=1_000_000{40.}else{70.}{1}else{0});
        self.bar.set_turn(enabled(c.show.turn&&s.last_turn.requests>0,data::fmt_tokens(s.last_turn.total)));self.bar.set_turn_sub(format!("{}次 {}",s.last_turn.requests,duration(s.last_turn.duration_ms)).into());
        self.bar.set_session(enabled(c.show.session&&s.requests>0,data::fmt_tokens(s.total)));self.bar.set_session_sub(format!("{}轮 {}次",s.turns,s.requests).into());
        self.bar.set_today(enabled(c.show.today&&s.today.requests>0,data::fmt_tokens(s.today.total)));self.bar.set_tools(enabled(c.show.tools&&s.tools_total>0,s.tools_total.to_string()));self.bar.set_tools_errors(if s.tools_errors>0{format!("×{}",s.tools_errors).into()}else{"".into()});
        self.bar.set_cache(enabled(c.show.cache&&s.input>0,format!("{:.0}%",s.cache_rate)));self.bar.set_cache_level(if s.cache_rate>=60.{0}else if s.cache_rate>=30.{1}else{3});
        self.bar.set_warning(if !s.error.is_empty(){"数据断开"}else if c.wallpaper.enabled&&!self.wp_message.is_empty()&&(self.wp_message.contains("失败")||self.wp_message.contains("无法")){"壁纸待恢复"}else if s.remote{"远端"}else{""}.into());
    }
    fn publish_state(&self){let s=&self.snapshot;let state=serde_json::json!({
        "ok":s.ok,"error":s.error,"session":s.title,"sid":s.sid,"pinned":self.cfg.lock().unwrap().pinned,
        "tps":s.last.tps,"ctx":s.ctx,"context_window":s.context_window,"cache_rate":s.cache_rate,
        "today_total":s.today.total,"session_total":s.total,"requests":s.requests,
        "models_busy":self.panel.get_models_busy(),"wallpaper":self.wp_message.clone(),
    });
        if let Err(e)=crate::control::publish_state(&state){tracing::warn!("状态发布失败：{e}");}
    }
    fn detail_text(&self)->String {format!("{}\n\n壁纸：{}\n{}",self.snapshot.detail_text,if self.wp_message.is_empty(){"未启用"}else{&self.wp_message},self.status)}
    fn show_hover(&mut self){
        self.hover.set_content_text(self.detail_text().into());
        if self.hover.window().is_visible(){return}
        let _=self.hover.show();let hh=win::slint_handle(self.hover.window());
        win::place_panel(hh,win::slint_handle(self.bar.window()));win::apply_bar_styles(hh,true);
    }
    fn sync_settings(&self){let c=self.cfg.lock().unwrap();let p=&self.panel;
        p.set_anchor_index(c.anchor_index());p.set_scope_index(if c.scope=="screen"{1}else{0});p.set_offset_x(c.offset_x);p.set_offset_y(c.offset_y);p.set_free_x(c.free_x);p.set_free_y(c.free_y);p.set_free_position_valid(c.free_position_valid);p.set_pinned(c.pinned);p.set_lock_position(c.position_locked);p.set_pinned_label(if c.pinned{format!("已钉住：{}",if self.snapshot.title.is_empty(){&c.pinned_sid}else{&self.snapshot.title}).into()}else{"未钉住，跟随当前会话".into()});
        p.set_opacity_pct((c.opacity*100.).round()as i32);p.set_font_px(c.font_px);p.set_font_family(c.font_family.clone().into());p.set_panel_opacity((c.panel_opacity*100.).round()as i32);p.set_panel_wallpaper(c.panel_wallpaper);p.set_panel_wallpaper_dim(c.panel_wallpaper_dim as i32);p.set_show_labels(c.labels);
        p.set_show_speed(c.show.speed);p.set_show_ctx(c.show.ctx);p.set_show_turn(c.show.turn);p.set_show_session(c.show.session);p.set_show_today(c.show.today);p.set_show_tools(c.show.tools);p.set_show_cache(c.show.cache);
        p.set_bar_always_on_top(c.always_on_top);p.set_hide_when_inactive(c.hide_when_inactive);p.set_poll_ms(c.poll_ms as i32);p.set_auto_start(c.autostart);p.set_auto_update(c.auto_update);p.set_bar_visible(!c.hidden);
        let w=&c.wallpaper;p.set_wp_path(w.path.clone().into());p.set_wp_enabled(w.enabled);p.set_wp_visible(w.visible);p.set_wp_monet(w.monet);p.set_wp_blur(w.blur as i32);p.set_wp_dim(w.dim as i32);p.set_wp_fit(["cover","contain","smart","stretch"].iter().position(|s|*s==w.fit).unwrap_or(0)as i32);p.set_focus_x((w.focus_x*100.)as i32);p.set_focus_y((w.focus_y*100.)as i32);p.set_wp_port(w.port as i32);p.set_wp_status(self.wp_message.clone().into());p.set_wp_busy(self.wp_busy||self.recovery_busy);p.set_auto_client_recovery(c.auto_client_recovery);
        if w.path.is_empty(){p.set_wp_preview_ready(false);}
        p.set_version_info(format!("ZCode Token {} · 原生 Rust",env!("CARGO_PKG_VERSION")).into());
        p.set_remote_enabled(c.remote.get("enabled").and_then(|v|v.as_bool()).unwrap_or(false));p.set_remote_host(c.remote.get("host").and_then(|v|v.as_str()).unwrap_or("").into());p.set_remote_user(c.remote.get("user").and_then(|v|v.as_str()).unwrap_or("").into());p.set_remote_port(c.remote.get("port").and_then(|v|v.as_i64()).unwrap_or(22)as i32);p.set_remote_key(c.remote.get("key").and_then(|v|v.as_str()).unwrap_or("").into());p.set_remote_helper(c.remote.get("executable").or_else(||c.remote.get("helper")).and_then(|v|v.as_str()).unwrap_or(".zcode/zcode-token-usage-statusbar/zcode-token-data").into());
        p.set_companion_enabled(c.companion_enabled);p.set_companion_auto(c.companion_auto);p.set_companion_paused(c.companion_paused);p.set_companion_config_path(c.companion_config.clone().into());p.set_companion_cooldown_secs(c.companion_cooldown_s as i32);p.set_companion_hourly(c.companion_hourly_limit as i32);p.set_companion_stream(c.companion_stream);p.set_companion_persist_history(c.companion_persist_history);
    }
    fn choose_wallpaper(&mut self){
        match system::pick_image(win::slint_handle(self.panel.window()),None){
            Ok(Some(path))=>self.select_wallpaper(path),Ok(None)=>{},Err(e)=>self.status=e,
        }
    }
    fn select_wallpaper(&mut self,path:PathBuf){
        {let mut c=self.cfg.lock().unwrap();c.wallpaper.path=path.to_string_lossy().into_owned();c.wallpaper.enabled=true;}
        self.preview_due=Some(Instant::now());self.save_wallpaper_intent();self.sync_settings();self.start_wallpaper("apply");
    }
    fn load_wallpaper_preview(&mut self){
        let path=self.cfg.lock().unwrap().wallpaper.path.clone();
        if self.preview_path==path{return}self.preview_path=path.clone();self.panel.set_wp_preview_ready(false);
        if path.trim().is_empty(){return}
        let tx=self.tx.clone();
        std::thread::spawn(move||{
            let result=(||{
                let metadata=std::fs::metadata(&path).map_err(|_|"图片文件无法读取".to_string())?;
                if !metadata.is_file()||metadata.len()>64*1024*1024{return Err("图片过大或不是文件".into())}
                let reader=image::ImageReader::open(&path).map_err(|_|"无法读取图片".to_string())?.with_guessed_format().map_err(|_|"无法识别图片格式".to_string())?;
                let decoded=reader.decode().map_err(|_|"图片解码失败".to_string())?;
                let rgba=decoded.thumbnail(1600,1200).into_rgba8();Ok((rgba.width(),rgba.height(),rgba.into_raw()))
            })();
            let _=tx.send(Output::WallpaperPreview(path,result));
        });
    }
    fn load_report(&mut self,days:usize){
        self.showing_report=true;self.report_generation+=1;let generation=self.report_generation;
        self.details.set_content_text("正在读取用量报表…".into());let _=self.details.show();let tx=self.tx.clone();
        std::thread::spawn(move||{let mut reader=data::DataReader::new();let result=reader.reports(days).map_err(|e|e.to_string());let _=tx.send(Output::Reports(generation,result));});
    }
    fn start_wallpaper(&mut self,kind:&str){
        if self.wp_busy {if kind!="health"{self.wp_due=Some(Instant::now()+Duration::from_millis(500));}return}
        let mut settings=self.cfg.lock().unwrap().wallpaper.clone();
        self.wp_busy=true;self.wp_due=None;
        let generation=self.wp_generation;let tx=self.tx.clone();let data_dir=self.paths.data_dir.clone();let op=kind.to_owned();
        self.panel.set_wp_busy(true);
        if kind!="health" {self.wp_message="正在更新 ZCode 壁纸…".into();self.panel.set_wp_status(self.wp_message.clone().into());}
        std::thread::spawn(move||{
            let event=if op=="health" {Output::Health(generation,wallpaper::health(&settings))} else {
                let result=(||{
                    if settings.enabled&&!settings.path.is_empty(){
                        let imported=wallpaper::import_image(Path::new(&settings.path),&data_dir)?;
                        settings.path=imported.to_string_lossy().into_owned();
                    }
                    wallpaper::apply(&settings)
                })();
                Output::Wallpaper(generation,settings,result)
            };
            let _=tx.send(event);
        });
    }
    fn choose_font(&mut self){let c=self.cfg.lock().unwrap().clone();let initial=system::FontChoice{family:c.font_family,size_px:c.font_px,..Default::default()};match system::choose_font(win::slint_handle(self.panel.window()),&initial){Ok(Some(f))=>{let mut c=self.cfg.lock().unwrap();c.font_family=f.family;c.font_px=f.size_px;drop(c);self.sync_settings();self.preview();},Ok(None)=>{},Err(e)=>self.status=e}}
    fn repair_launchers(&mut self){match win::process_path(win::zcode_hwnd()){Ok(exe)=>{let port=self.cfg.lock().unwrap().wallpaper.port;self.status_result(system::repair_zcode_launchers(&exe,port,&self.paths),"已备份并修复 ZCode 启动入口；下次启动时开启壁纸连接。")},Err(e)=>self.status=e}}
    fn start_recovery(&mut self,manual:bool){
        if self.recovery_busy||self.wp_busy {return}
        let c=self.cfg.lock().unwrap().clone();
        if !manual&&!c.auto_client_recovery {self.recovery_checked=Instant::now();return}
        let exe=win::process_path(win::zcode_hwnd()).ok();
        let tx=self.tx.clone();let root=self.paths.data_dir.clone();
        self.recovery_busy=true;self.panel.set_wp_busy(true);
        if manual {self.panel.set_recovery_status("正在恢复保存的自定义外观…".into());}
        std::thread::spawn(move||{
            let manager=crate::recovery::RecoveryManager::new(root.clone());
            let result=if manual {crate::recovery::restore(exe.as_deref(),&c.wallpaper,&root)}else{
                manager.observe(exe.clone(),&c.wallpaper).and_then(|report|{
                    if report.needs_restore {crate::recovery::restore(exe.as_deref(),&c.wallpaper,&root)}else{Ok(report)}
                })
            };
            let _=tx.send(Output::Recovery(result));
        });
    }
    fn check_update(&mut self){if self.update_busy{return}self.update_busy=true;self.panel.set_update_status("正在检查发布版本…".into());let tx=self.tx.clone();std::thread::spawn(move||{let r=(||{let c=update::UpdateClient::new(update::Repository::default())?;let v=update::InstalledVersion::detect(env!("CARGO_PKG_VERSION"),None)?;c.check(&v)})();let _=tx.send(Output::Update(r));});}
    fn install_update(&mut self){if self.update_busy{return}let Some(info)=self.update_info.clone()else{self.check_update();return};if !info.can_update(){self.status="当前没有可安装的 Rust 发布包。".into();return}self.update_busy=true;let tx=self.tx.clone();let paths=self.paths.clone();std::thread::spawn(move||{let r=update::UpdateClient::new(update::Repository::default()).and_then(|c|c.prepare_update(&info,&paths,&[]));let _=tx.send(Output::UpdateReady(r));});}
    fn refresh_diagnostics(&mut self){self.panel.set_status_lines(diagnostics(&self.paths,&self.cfg.lock().unwrap(),Some(&self.snapshot)).into());}
    fn quit(&mut self){self.stop.store(true,Ordering::Release);if let Some(c)=self.companion.as_mut(){c.close();}self.models.cancel();let _=slint::quit_event_loop();}
}
impl App {
    fn configure_companion(&mut self){
        let c=self.cfg.lock().unwrap();
        let settings=crate::companion::CompanionSettings{
            enabled:c.companion_enabled,auto:c.companion_auto,paused:c.companion_paused,
            config_path:c.companion_config.clone(),cooldown_seconds:c.companion_cooldown_s,
            hourly_limit:c.companion_hourly_limit,provider_id:c.companion_provider_id.clone(),
            model_id:c.companion_model_id.clone(),stream:c.companion_stream,persist_history:c.companion_persist_history,
            history_path:self.paths.data_dir.join("companion/replies.json").to_string_lossy().into_owned(),..Default::default()
        };drop(c);
        let path=settings.config_path.clone();
        if let Some(runtime)=self.companion.as_mut(){runtime.configure(settings);}else{self.companion=Some(crate::companion::Runtime::new(settings));}
        if path!=self.companion_candidates_path {
            self.companion_candidates_path=path.clone();self.companion_candidates.clear();self.sync_companion_candidates();
            if !path.trim().is_empty(){let tx=self.tx.clone();std::thread::spawn(move||{
                let result=crate::companion::connection_candidates(&path).map_err(|e|e.to_string());
                let _=tx.send(Output::CompanionCandidates(path,result));
            });}
        }
    }
    fn sync_companion_candidates(&self){
        let c=self.cfg.lock().unwrap();
        let names:Vec<SharedString>=self.companion_candidates.iter().map(|m|format!("{} · {}",m.provider_name,m.model_id).into()).collect();
        let index=self.companion_candidates.iter().position(|m|m.provider_id==c.companion_provider_id&&m.model_id==c.companion_model_id).map(|i|i as i32).unwrap_or(-1);
        self.panel.set_companion_model_names(slint::ModelRc::new(slint::VecModel::from(names)));
        self.panel.set_companion_model_index(index);
    }
    fn companion_action(&mut self,name:&str){
        if name=="choose-companion-config" {
            match system::pick_config(win::slint_handle(self.panel.window()),None){
                Ok(Some(p))=>{self.text_change("companion-config-path",p.to_string_lossy().as_ref());self.sync_settings();},Ok(None)=>{},Err(e)=>self.status=e,
            }return;
        }
        if name=="companion-toggle-pause" {let paused=!self.cfg.lock().unwrap().companion_paused;self.cfg.lock().unwrap().companion_paused=paused;if let Some(r)=self.companion.as_mut(){r.set_paused(paused);}self.sync_settings();return}
        if name=="companion-clear-history" {if let Some(r)=self.companion.as_mut(){r.clear_history();}self.panel.set_companion_history("".into());return}
        if name=="companion-stop" {if let Some(r)=self.companion.as_ref(){r.cancel();}self.panel.set_companion_status("已取消当前陪伴请求".into());return}
        let signal=match name {"companion-greet"=>crate::companion::Signal::Greeting,"companion-say"=>crate::companion::Signal::SaySomething,"companion-send"=>crate::companion::Signal::Message(self.companion_draft.trim().to_string()),_=>return};
        if let Some(runtime)=self.companion.as_ref(){match runtime.request(signal){Ok(())=>{if name=="companion-send"{self.companion_draft.clear();self.panel.set_companion_input("".into());}},Err(e)=>self.panel.set_companion_status(e.to_string().into())}}
    }
    fn companion_tick(&mut self){
        let Some(r)=self.companion.as_mut()else{return};let t=&self.snapshot.turn_state;
        let state=crate::companion::TurnState{available:t.available,is_sub:t.is_sub,session_id:t.session_id.clone(),turn_id:t.turn_id.clone(),status:t.status.clone(),started_at:t.started_at,completed_at:t.completed_at,duration_ms:t.duration_ms,tool_errors:t.tool_errors,cancelled_by_user:t.cancelled_by_user};
        r.observe(&state,chrono::Utc::now().timestamp_millis()as f64/1000.);
        for e in r.drain(){
            self.panel.set_companion_status(e.status.message.clone().into());self.panel.set_companion_busy(e.status.busy);
            if matches!(e.kind,crate::companion::EventKind::Reply|crate::companion::EventKind::Partial) {
                let history=r.history().iter().map(|h|h.message.clone()).collect::<Vec<_>>().join("\n\n");
                self.panel.set_companion_history(if e.kind==crate::companion::EventKind::Partial{format!("{history}\n\n{}",e.message)}else{history}.into());
                if e.kind==crate::companion::EventKind::Reply && win::visible(win::slint_handle(self.bar.window())) {
                    self.toast.set_message(e.message.into());let _=self.toast.show();
                    let hwnd=win::slint_handle(self.toast.window());win::place_panel(hwnd,win::slint_handle(self.bar.window()));win::apply_bar_styles(hwnd,true);
                    self.toast_until=Some(Instant::now()+Duration::from_secs(10));
                }
            }
        }
        let status=r.status();self.panel.set_companion_status(status.message.into());self.panel.set_companion_busy(status.busy);
    }
}

fn duration(ms:i64)->String{if ms<1000{format!("{}ms",ms.max(0))}else if ms<60000{format!("{:.1}s",ms as f64/1000.)}else{format!("{}m {}s",ms/60000,ms/1000%60)}}
pub fn legacy_policy(paths:&system::AppPaths)->system::LegacyPolicy {
    let mut p=system::LegacyPolicy::default();
    if let Some(home)=std::env::var_os("ZCODE_HOME"){p.python_scripts.push(PathBuf::from(home).join("workspace/default/zcode-token-bar/hoverbar.py"));}
    for parent in paths.exe.ancestors().skip(1).take(6){p.python_scripts.extend([parent.join("hoverbar.py"),parent.join("zcode-token-bar/hoverbar.py")]);p.rust_executables.push(parent.join("zcode-token-bar-rs/target/release/zcode-token-bar.exe"));}
    p.python_scripts.retain(|p|p.is_file());p.rust_executables.retain(|p|p.is_file());p
}
pub fn diagnostics(paths:&system::AppPaths,c:&Settings,s:Option<&data::Snapshot>)->String {
    let autostart=match system::autostart_status(paths,&legacy_policy(paths)){Ok(r)=>format!("Rust {}；旧 Python {}",if r.enabled{"已开启"}else{"未开启"},if r.legacy_enabled{"仍在启用"}else{"未启用"}),Err(e)=>format!("读取失败：{e}")};
    let z=win::zcode_hwnd();format!("版本：{}\n进程：{}\n程序：{}\n数据库：{}\n数据链路：{}\nZCode 窗口：{}\n自启动：{}\n设置：{}\n日志：{}\n刷新间隔：{} ms\n位置：{} / {}\n钉住：{}\n壁纸：{}\n远端：{}",env!("CARGO_PKG_VERSION"),std::process::id(),paths.exe.display(),data::db_path().display(),s.map(|v|if v.ok{"正常".into()}else{v.error.clone()}).unwrap_or_else(||"等待首次读取".into()),if z==0{"未运行 / 最小化"}else{"已连接"},autostart,settings::settings_path().display(),paths.logs_dir.display(),c.poll_ms,c.anchor,c.scope,if c.pinned{&c.pinned_sid}else{"未钉住"},if c.wallpaper.enabled{&c.wallpaper.path}else{"未启用"},if c.remote.get("enabled").and_then(|v|v.as_bool())==Some(true){"开启"}else{"关闭"})
}
