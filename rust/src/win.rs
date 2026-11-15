//! Win32 边界：窗口句柄、物理坐标、单实例信号。设置窗口始终保留正常输入焦点。
use std::path::Path;
use windows::{core::PCWSTR, Win32::{Foundation::*, Graphics::Gdi::*, System::Threading::*, UI::WindowsAndMessaging::*}};
use crate::settings::Bounds;

fn wide(s:&str)->Vec<u16>{s.encode_utf16().chain(Some(0)).collect()}
fn handle(h:isize)->HWND {HWND(h as *mut _)}

pub fn slint_handle(window:&slint::Window)->isize {
    use raw_window_handle::{HasWindowHandle,RawWindowHandle};
    let owned=window.window_handle();
    match owned.window_handle().map(|h|h.as_raw()) {Ok(RawWindowHandle::Win32(w))=>w.hwnd.get(),_=>0}
}
pub fn own_hwnd(title:&str)->isize {
    struct Find<'a>{title:&'a str,result:isize}
    unsafe extern "system" fn visit(hwnd:HWND,p:LPARAM)->BOOL {
        let f=&mut *(p.0 as *mut Find);
        let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
        if pid==std::process::id()&&window_title(hwnd)==f.title {f.result=hwnd.0 as isize;return false.into()}
        true.into()
    }
    let mut f=Find{title,result:0};unsafe{let _=EnumWindows(Some(visit),LPARAM(&mut f as *mut _ as isize));}f.result
}
unsafe fn window_title(hwnd:HWND)->String {
    let n=GetWindowTextLengthW(hwnd).clamp(0,4096) as usize;
    let mut buf=vec![0u16;n+1];let got=GetWindowTextW(hwnd,&mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..got])
}
pub fn zcode_hwnd()->isize {
    struct Find {best:isize,area:i64}
    unsafe extern "system" fn visit(hwnd:HWND,p:LPARAM)->BOOL {
        let f=&mut *(p.0 as *mut Find);let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
        if pid==std::process::id()||!IsWindowVisible(hwnd).as_bool(){return true.into()}
        if (GetWindowLongPtrW(hwnd,GWL_EXSTYLE) as u32)&WS_EX_TOOLWINDOW.0!=0{return true.into()}
        let title=window_title(hwnd);
        if !title.to_ascii_lowercase().contains("zcode"){return true.into()}
        let mut r=RECT::default();
        if GetWindowRect(hwnd,&mut r).is_ok() {
            let(w,h)=(r.right-r.left,r.bottom-r.top);
            if w>=400&&h>=300&&i64::from(w)*i64::from(h)>f.area {f.best=hwnd.0 as isize;f.area=i64::from(w)*i64::from(h);}
        }
        true.into()
    }
    let mut f=Find{best:0,area:0};unsafe{let _=EnumWindows(Some(visit),LPARAM(&mut f as *mut _ as isize));}f.best
}
pub fn rect(hwnd:isize)->Option<Bounds> {
    if hwnd==0{return None}let mut r=RECT::default();
    unsafe{GetWindowRect(handle(hwnd),&mut r)}.ok()?;
    Some(Bounds{left:r.left,top:r.top,right:r.right,bottom:r.bottom})
}
pub fn window_rect(hwnd:isize)->(i32,i32,i32,i32){rect(hwnd).map(|r|(r.left,r.top,r.right,r.bottom)).unwrap_or_default()}
pub fn cursor_pos()->(i32,i32){let mut p=POINT::default();unsafe{let _=GetCursorPos(&mut p);} (p.x,p.y)}
pub fn left_down()->bool {unsafe{windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(1)<0}}
pub fn minimized(hwnd:isize)->bool {hwnd==0||unsafe{IsIconic(handle(hwnd)).as_bool()}}
pub fn foreground_is(hwnd:isize)->bool {hwnd!=0&&unsafe{GetForegroundWindow()==handle(hwnd)}}

pub fn work_area(hwnd:isize)->Bounds {
    let m=unsafe{MonitorFromWindow(handle(hwnd),MONITOR_DEFAULTTONEAREST)};monitor_bounds(m)
}
pub fn work_area_point(x:i32,y:i32)->Bounds {
    let m=unsafe{MonitorFromPoint(POINT{x,y},MONITOR_DEFAULTTONEAREST)};monitor_bounds(m)
}
fn monitor_bounds(m:HMONITOR)->Bounds {
    let mut mi=MONITORINFO{cbSize:std::mem::size_of::<MONITORINFO>() as u32,..Default::default()};
    if unsafe{GetMonitorInfoW(m,&mut mi)}.as_bool(){let r=mi.rcWork;Bounds{left:r.left,top:r.top,right:r.right,bottom:r.bottom}}
    else{Bounds{left:0,top:0,right:unsafe{GetSystemMetrics(SM_CXSCREEN)}.max(640),bottom:unsafe{GetSystemMetrics(SM_CYSCREEN)}.max(480)}}
}
pub fn apply_bar_styles(hwnd:isize,topmost:bool) {
    if hwnd==0{return}unsafe{
        let hwnd=handle(hwnd);let old=GetWindowLongPtrW(hwnd,GWL_EXSTYLE) as u32;
        let new=(old|WS_EX_NOACTIVATE.0|WS_EX_TOOLWINDOW.0)&!WS_EX_APPWINDOW.0;
        if old!=new {SetWindowLongPtrW(hwnd,GWL_EXSTYLE,new as isize);}
        // 不绑定 ZCode 属主：面板/拖动/置顶状态独立，最小化由调度器处理。
        if GetWindowLongPtrW(hwnd,GWLP_HWNDPARENT)!=0 {SetWindowLongPtrW(hwnd,GWLP_HWNDPARENT,0);}
        if (old&WS_EX_TOPMOST.0!=0)!=topmost {
            let _=SetWindowPos(hwnd,if topmost{HWND_TOPMOST}else{HWND_NOTOPMOST},0,0,0,0,SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE);
        }
    }
}
pub fn move_window(hwnd:isize,x:i32,y:i32) {
    if hwnd==0{return}if let Some(r)=rect(hwnd){if(r.left,r.top)==(x,y){return}}
    unsafe{let _=SetWindowPos(handle(hwnd),HWND::default(),x,y,0,0,SWP_NOSIZE|SWP_NOACTIVATE|SWP_NOZORDER);}
}
pub fn visible(hwnd:isize)->bool {hwnd!=0&&unsafe{IsWindowVisible(handle(hwnd)).as_bool()}}
pub fn show(hwnd:isize,yes:bool){if hwnd!=0&&visible(hwnd)!=yes{unsafe{let _=ShowWindow(handle(hwnd),if yes{SW_SHOWNOACTIVATE}else{SW_HIDE});}}}
pub fn focus(hwnd:isize){if hwnd!=0{unsafe{let _=ShowWindow(handle(hwnd),SW_SHOWNORMAL);let _=SetForegroundWindow(handle(hwnd));}}}
pub fn set_opacity(hwnd:isize,value:f64) {
    if hwnd==0{return}unsafe{
        let h=handle(hwnd);let ex=GetWindowLongPtrW(h,GWL_EXSTYLE) as u32;
        if value>=0.999 {
            if ex&WS_EX_LAYERED.0!=0 {SetWindowLongPtrW(h,GWL_EXSTYLE,(ex&!WS_EX_LAYERED.0) as isize);}
        }else{
            SetWindowLongPtrW(h,GWL_EXSTYLE,(ex|WS_EX_LAYERED.0) as isize);
            let _=SetLayeredWindowAttributes(h,COLORREF(0),(value.clamp(0.3,1.0)*255.0).round() as u8,LWA_ALPHA);
        }
    }
}
pub fn place_panel(panel:isize,bar:isize) {
    let Some(p)=rect(panel)else{return};let work=work_area(if bar==0{panel}else{bar});
    let(x,y)=if let Some(b)=rect(bar){
        let y=if b.top-p.height()-10>=work.top{b.top-p.height()-10}else{b.bottom+10};
        (b.left+(b.width()-p.width())/2,y)
    }else{(work.left+(work.width()-p.width())/2,work.top+(work.height()-p.height())/2)};
    let(x,y)=work.clamp(x,y,p.width(),p.height());move_window(panel,x,y);
}
pub fn clamp_window(hwnd:isize) {
    if let Some(r)=rect(hwnd){let w=work_area(hwnd);let(x,y)=w.clamp(r.left,r.top,r.width(),r.height());move_window(hwnd,x,y)}
}

const INSTANCE:&str="Local\\zcode-token-bar-rs";
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {fn drop(&mut self){unsafe{let _=CloseHandle(self.0);}}}
/// 内核事件可跨线程等待；OwnedHandle 保证只关闭一次。
unsafe impl Send for OwnedHandle {}
pub struct SingleInstance { _mutex:OwnedHandle,quit:OwnedHandle,settings:OwnedHandle,details:OwnedHandle,reload:OwnedHandle,dump:OwnedHandle,wallpaper:OwnedHandle,companion:OwnedHandle }
pub enum Instance {Primary(SingleInstance),Existing}
impl SingleInstance {
    pub fn acquire()->Result<Instance,String>{unsafe{
        let m=CreateMutexW(None,false,PCWSTR(wide(INSTANCE).as_ptr())).map_err(|e|e.to_string())?;
        let already=GetLastError()==ERROR_ALREADY_EXISTS;
        let owned=OwnedHandle(m);if already{return Ok(Instance::Existing)}
        fn event(suffix:&str)->Result<OwnedHandle,String>{unsafe{
            CreateEventW(None,false,false,PCWSTR(wide(&format!("{INSTANCE}-{suffix}")).as_ptr())).map(OwnedHandle).map_err(|e|e.to_string())
        }}
        Ok(Instance::Primary(Self{_mutex:owned,quit:event("quit")?,settings:event("settings")?,details:event("details")?,reload:event("reload")?,dump:event("dump")?,wallpaper:event("wallpaper")?,companion:event("companion")?}))
    }}
    pub fn poll(&self)->Option<&'static str>{unsafe{
        for (h,name) in [(&self.quit,"quit"),(&self.settings,"settings"),(&self.details,"details"),(&self.reload,"reload"),(&self.dump,"dump"),(&self.wallpaper,"wallpaper"),(&self.companion,"companion")] {
            if WaitForSingleObject(h.0,0)==WAIT_OBJECT_0 {return Some(name)}
        }
        None
    }}
}
pub fn signal(action:&str)->bool {unsafe{
    let name=wide(&format!("{INSTANCE}-{action}"));
    let Ok(h)=OpenEventW(EVENT_MODIFY_STATE,false,PCWSTR(name.as_ptr()))else{return false};
    let owned=OwnedHandle(h);SetEvent(owned.0).is_ok()
}}

pub fn copy_text(text:&str)->Result<(),String>{
    use windows::Win32::System::{DataExchange::*,Memory::*};
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e|e.to_string())?;
        struct Clipboard;impl Drop for Clipboard{fn drop(&mut self){unsafe{let _=CloseClipboard();}}}
        let _guard=Clipboard;
        let data=wide(text);let mem=GlobalAlloc(GMEM_MOVEABLE,data.len()*2).map_err(|e|e.to_string())?;
        let ptr=GlobalLock(mem);
        if ptr.is_null(){let _=GlobalFree(mem);return Err("无法分配剪贴板内存".into())}
        std::ptr::copy_nonoverlapping(data.as_ptr(),ptr as *mut u16,data.len());let _=GlobalUnlock(mem);
        if let Err(e)=EmptyClipboard(){let _=GlobalFree(mem);return Err(e.to_string())}
        match SetClipboardData(13,HANDLE(mem.0)) {Ok(_)=>Ok(()),Err(e)=>{let _=GlobalFree(mem);Err(e.to_string())}}
    }
}

pub fn clipboard_text()->Result<String,String>{
    use windows::Win32::System::{DataExchange::*,Memory::*};
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e|e.to_string())?;
        struct Clipboard;impl Drop for Clipboard{fn drop(&mut self){unsafe{let _=CloseClipboard();}}}
        let _guard=Clipboard;
        let handle=GetClipboardData(13).map_err(|_|"剪贴板里没有文本".to_string())?;
        let memory=HGLOBAL(handle.0);let count=(GlobalSize(memory)/2).min(32_768);
        let ptr=GlobalLock(memory) as *const u16;
        if ptr.is_null(){return Err("无法读取剪贴板文本".into())}
        let data=std::slice::from_raw_parts(ptr,count);let len=data.iter().position(|c|*c==0).unwrap_or(count);
        let text=String::from_utf16_lossy(&data[..len]);let _=GlobalUnlock(memory);Ok(text)
    }
}

pub fn process_path(hwnd:isize)->Result<std::path::PathBuf,String>{
    if hwnd==0{return Err("ZCode 尚未运行，无法确认需要修复的启动入口".into())}
    unsafe{let mut pid=0;GetWindowThreadProcessId(handle(hwnd),Some(&mut pid));
        let p=OwnedHandle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid).map_err(|e|e.to_string())?);
        let mut b=vec![0u16;32768];let mut n=b.len()as u32;
        QueryFullProcessImageNameW(p.0,PROCESS_NAME_WIN32,windows::core::PWSTR(b.as_mut_ptr()),&mut n).map_err(|e|e.to_string())?;
        Ok(std::path::PathBuf::from(String::from_utf16_lossy(&b[..n as usize])))
    }
}

pub fn open_path(path:&Path)->Result<(),String>{
    use std::os::windows::ffi::OsStrExt;
    let p:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();
    let r=unsafe{windows::Win32::UI::Shell::ShellExecuteW(HWND::default(),PCWSTR(wide("open").as_ptr()),PCWSTR(p.as_ptr()),PCWSTR::null(),PCWSTR::null(),SW_SHOWNORMAL)};
    if r.0 as isize>32{Ok(())}else{Err("无法打开指定路径".into())}
}
