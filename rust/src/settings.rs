//! 兼容旧版的持久设置。外观、窗口行为与壁纸设置各自保存，不存 API 凭据。
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Path, PathBuf}};
use crate::wallpaper::WallpaperSettings;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShowItems { pub speed:bool, pub ctx:bool, pub turn:bool, pub session:bool, pub today:bool, pub tools:bool, pub cache:bool }
impl Default for ShowItems {
    fn default()->Self { Self {speed:true,ctx:true,turn:true,session:true,today:true,tools:true,cache:true} }
}
pub const ANCHORS: [&str;7] = ["bottom-left","bottom-center","bottom-right","top-left","top-center","top-right","free"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub anchor:String, pub scope:String, pub offset_x:i32, pub offset_y:i32,
    pub free_x:i32, pub free_y:i32, pub free_position_valid:bool,
    pub opacity:f64, pub font_px:i32, pub font_family:String,
    pub always_on_top:bool, pub hide_when_inactive:bool, pub hidden:bool,
    pub show:ShowItems, pub labels:bool, pub poll_ms:u64, pub context_window:i64,
    pub panel_opacity:f64, pub panel_wallpaper:bool, pub panel_wallpaper_dim:u32,
    pub auto_update:bool, pub autostart:bool, pub auto_client_recovery:bool,
    pub pinned:bool, pub pinned_sid:String, pub pin_x:i32, pub pin_y:i32,
    pub position_locked:bool, pub locked_x:i32, pub locked_y:i32,
    pub last_page:i32, pub panel_x:i32, pub panel_y:i32, pub panel_position_valid:bool,
    pub panel_width:i32, pub panel_height:i32,
    pub wallpaper:WallpaperSettings,
    pub remote:serde_json::Value,
    pub companion_enabled:bool, pub companion_auto:bool, pub companion_paused:bool,
    pub companion_config:String, pub companion_cooldown_s:u64, pub companion_hourly_limit:u32,
    pub companion_provider_id:String, pub companion_model_id:String,
    pub companion_stream:bool, pub companion_persist_history:bool,
    #[serde(flatten)] pub extra:BTreeMap<String,serde_json::Value>,
}
impl Default for Settings {
    fn default()->Self { Self {
        schema_version:2,anchor:"bottom-left".into(),scope:"window".into(),offset_x:16,offset_y:6,
        free_x:0,free_y:0,free_position_valid:false,opacity:0.92,font_px:13,font_family:"Microsoft YaHei UI".into(),
        always_on_top:false,hide_when_inactive:false,hidden:false,show:ShowItems::default(),labels:true,poll_ms:700,
        context_window:128_000,panel_opacity:1.0,panel_wallpaper:true,panel_wallpaper_dim:65,auto_update:false,autostart:false,auto_client_recovery:true,
        pinned:false,pinned_sid:String::new(),pin_x:-1,pin_y:-1,position_locked:false,locked_x:0,locked_y:0,last_page:0,
        panel_x:0,panel_y:0,panel_position_valid:false,panel_width:820,panel_height:620,
        wallpaper:WallpaperSettings::default(),remote:serde_json::Value::Null,
        companion_enabled:false,companion_auto:true,companion_paused:false,companion_config:String::new(),
        companion_cooldown_s:300,companion_hourly_limit:6,
        companion_provider_id:String::new(),companion_model_id:String::new(),
        companion_stream:false,companion_persist_history:false,extra:BTreeMap::new(),
    }}
}
impl Settings {
    pub fn normalize(&mut self) {
        if !ANCHORS.contains(&self.anchor.as_str()) {self.anchor="bottom-center".into();}
        if self.scope!="screen" {self.scope="window".into();}
        self.font_px=self.font_px.clamp(10,28);
        self.opacity=if self.opacity.is_finite(){self.opacity.clamp(0.30,1.0)}else{0.92};
        self.panel_opacity=if self.panel_opacity.is_finite(){self.panel_opacity.clamp(0.55,1.0)}else{1.0};
        self.offset_x=self.offset_x.clamp(-4000,4000); self.offset_y=self.offset_y.clamp(-4000,4000);
        self.poll_ms=self.poll_ms.clamp(300,30_000); self.context_window=self.context_window.clamp(1,100_000_000);
        self.panel_width=self.panel_width.clamp(640,1600);self.panel_height=self.panel_height.clamp(480,1200);
        self.last_page=self.last_page.clamp(0,8);
        self.panel_wallpaper_dim=self.panel_wallpaper_dim.clamp(35,95);
        self.companion_cooldown_s=self.companion_cooldown_s.min(86400);
        self.companion_hourly_limit=self.companion_hourly_limit.min(60);
        if self.pinned_sid.is_empty() {self.pinned=false;}
        self.schema_version=2;
    }
    pub fn anchor_index(&self)->i32 {ANCHORS.iter().position(|v|*v==self.anchor).unwrap_or(1) as i32}
    pub fn restore_dock(&mut self) {self.anchor="bottom-left".into();self.scope="window".into();self.pinned=false;self.pinned_sid.clear();self.position_locked=false;}
}
pub fn settings_path()->PathBuf {crate::data_dir().join("settings-rs.json")}

pub fn load()->Settings {load_with_report().0}
pub fn load_with_report()->(Settings, Option<String>) {
    let path=settings_path();
    match std::fs::read(&path) {
        Ok(bytes)=> match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(mut value)=> {
                // 首次升级仅补旧版缺失字段，不覆盖用户已有 Rust 设置。
                let old_version=value.get("schema_version").and_then(|v|v.as_u64()).unwrap_or(0);
                if old_version<2 {
                    if let Some(legacy)=legacy_value() {merge_missing(&mut value,&legacy);}
                    if value.get("anchor").and_then(|v|v.as_str())==Some("free") {value["free_position_valid"]=true.into();}
                }
                match serde_json::from_value::<Settings>(value) {
                    Ok(mut s)=>{s.normalize();(s,None)},
                    Err(e)=>(Settings::default(),Some(format!("设置格式错误，原文件保留：{e}"))),
                }
            }
            Err(e)=>(Settings::default(),Some(format!("设置无法解析，原文件保留：{e}"))),
        },
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=> {
            let mut s=legacy_value().and_then(|v|serde_json::from_value::<Settings>(v).ok()).unwrap_or_default();
            if s.anchor=="free" {s.free_position_valid=true;}
            s.normalize();(s,None)
        },
        Err(e)=>(Settings::default(),Some(format!("设置无法读取：{e}"))),
    }
}
pub fn save(s:&Settings)->Result<(),String> {
    let path=settings_path();
    if let Ok(original)=std::fs::read(&path) {
        let old=serde_json::from_slice::<serde_json::Value>(&original).map_err(|_|"旧设置损坏，已保留；请先在数据目录修复该文件。".to_string())?;
        if old.get("schema_version").and_then(|v|v.as_u64()).unwrap_or(0)<2 {
            let backup=path.with_extension("json.before-native-v2.bak");
            if !backup.exists(){std::fs::write(&backup,&original).map_err(|e|format!("迁移前备份失败：{e}"))?;}
        }
    }
    crate::storage::json(&path,s)
}
/// 即时操作只提交自己的字段，不把尚未应用的面板草稿一并保存。
pub fn merge_fields(saved:&Settings,draft:&Settings,keys:&[&str])->Result<Settings,String>{
    let mut to=serde_json::to_value(saved).map_err(|e|e.to_string())?;
    let from=serde_json::to_value(draft).map_err(|e|e.to_string())?;
    for key in keys {to[*key]=from.get(*key).ok_or_else(||format!("未知设置字段：{key}"))?.clone();}
    serde_json::from_value(to).map_err(|e|e.to_string())
}
pub fn reload_legacy()->Option<Settings> {let mut s:Settings=serde_json::from_value(legacy_value()?).ok()?;s.free_position_valid=s.anchor=="free";s.normalize();Some(s)}

fn legacy_value()->Option<serde_json::Value> {
    let mut candidates=vec![crate::data_dir().join("hoverbar.json"),crate::data_dir().join("hoverbar-settings.json")];
    if let Some(root)=std::env::var_os("ZCODE_HOME") {
        candidates.push(PathBuf::from(root).join("workspace/default/zcode-token-bar/hoverbar.json"));
    }
    if let Ok(exe)=std::env::current_exe() {
        for parent in exe.ancestors().skip(1).take(5) {
            candidates.extend([parent.join("hoverbar.json"),parent.join("hoverbar-settings.json"),parent.join("zcode-token-bar/hoverbar.json"),parent.join("config.json")]);
        }
    }
    if let Some(home)=std::env::var_os("USERPROFILE").or_else(||std::env::var_os("HOME")) {
        let home=PathBuf::from(home);
        candidates.extend([home.join(".zcode/zcode-token-bar/hoverbar.json"),home.join(".zcode/zcode-token-usage-statusbar/hoverbar.json")]);
    }
    for p in candidates {
        if let Ok(b)=std::fs::read(&p) {if let Ok(v)=serde_json::from_slice::<serde_json::Value>(&b) {
            if v.get("anchor").is_some()&&v.get("show").is_some() {
                let mut merged=v;
                // zusage.py 旁 config.json 独立保存数据层配置，按缺项补入。
                if let Some(parent)=p.parent(){if let Ok(bytes)=std::fs::read(parent.join("config.json")){
                    if let Ok(data)=serde_json::from_slice::<serde_json::Value>(&bytes){
                        for key in ["remote","context_window","poll_ms","lang","catalog","archive_days","archive_mode"] {
                            if merged.get(key).is_none(){if let Some(value)=data.get(key){merged[key]=value.clone();}}
                        }
                    }
                }}
                return Some(merged);
            }
        }}
    }
    None
}
fn merge_missing(target:&mut serde_json::Value, from:&serde_json::Value) {
    if let (Some(to),Some(src))=(target.as_object_mut(),from.as_object()) {
        for (k,v) in src {if let Some(existing)=to.get_mut(k) {merge_missing(existing,v)}else{to.insert(k.clone(),v.clone());}}
    }
}

/// 所有窗口边界算法使用同一物理坐标系，包含负坐标副屏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {pub left:i32,pub top:i32,pub right:i32,pub bottom:i32}
impl Bounds {
    pub fn width(self)->i32 {(self.right-self.left).max(1)}
    pub fn height(self)->i32 {(self.bottom-self.top).max(1)}
    pub fn clamp(self,x:i32,y:i32,w:i32,h:i32)->(i32,i32) {
        (x.clamp(self.left,(self.right-w).max(self.left)),y.clamp(self.top,(self.bottom-h).max(self.top)))
    }
}
pub fn anchored_position(anchor:&str, target:Bounds,work:Bounds,w:i32,h:i32,ox:i32,oy:i32)->(i32,i32) {
    let x=if anchor.ends_with("left"){target.left+ox}else if anchor.ends_with("right"){target.right-w-ox}else{target.left+(target.width()-w)/2+ox};
    let y=if anchor.starts_with("top"){target.top+oy}else{target.bottom-h-oy};
    work.clamp(x,y,w,h)
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn all_six_anchors_and_negative_monitor_stay_visible() {
        let work=Bounds{left:-1920,top:0,right:0,bottom:1040};
        let target=Bounds{left:-2100,top:-60,right:-20,bottom:1100};
        for a in &ANCHORS[..6] {let(x,y)=anchored_position(a,target,work,720,50,16,6);assert!(x>=work.left&&x+720<=work.right);assert!(y>=0&&y+50<=1040);}
    }
    #[test] fn unknown_keys_and_partial_show_survive_migration() {
        let s:Settings=serde_json::from_value(serde_json::json!({"anchor":"free","show":{"cache":false},"future_option":7})).unwrap();
        assert!(s.show.speed);assert!(!s.show.cache);assert_eq!(s.extra["future_option"],7);
    }
    #[test] fn defaults_are_normalized_without_losing_user_settings() {
        let mut s=Settings{font_px:200,opacity:f64::NAN,poll_ms:1,..Default::default()};s.normalize();assert_eq!(s.font_px,28);assert_eq!(s.opacity,0.92);assert_eq!(s.poll_ms,300);
    }
}
