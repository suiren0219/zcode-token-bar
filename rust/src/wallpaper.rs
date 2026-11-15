//! 原生壁纸：同步 API 应在后台线程调用；不启动外部进程、不访问非本机服务。
//! 首次迁移后由调用方保存设置；tick 用 health().needs_apply 决定是否 apply()。
//! reset() 只清理现场，调用方须同步保存 enabled=false，避免下次 tick 恢复。

use base64::{engine::general_purpose::STANDARD, Engine};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use material_colors::{
    color::Rgb,
    quantize::{Quantizer, QuantizerCelebi},
    score::Score,
    theme::{Theme, ThemeBuilder},
};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tungstenite::{protocol::WebSocketConfig, Message, WebSocket};

const RUNTIME: &str = include_str!("wallpaper_runtime.js");
const VERSION: u32 = 1;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CDP_BYTES: usize = 4 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const EVAL_TIMEOUT: Duration = Duration::from_secs(10);
// 与手动刷新、定时恢复串行；仅缓存一张图，复用判据是内容哈希而非路径/mtime。
static OPERATION: Mutex<()> = Mutex::new(());
static ASSETS: OnceLock<Mutex<Option<Arc<Assets>>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WallpaperSettings {
    pub enabled: bool,
    #[serde(alias = "wallpaperPath")]
    pub path: String,
    /// 像素，0..80。
    pub blur: f64,
    /// 黑色遮罩百分比，0..100。
    pub dim: f64,
    /// 壁纸本身的不透明度，0..1，不影响文字与控件。
    pub opacity: f64,
    pub monet: bool,
    #[serde(alias = "wallpaperVisible")]
    pub visible: bool,
    /// cover / contain / smart；兼容 Python 的 stretch。
    pub fit: String,
    /// 图片焦点，0..1；smart 的 0.5 表示使用自动焦点。
    #[serde(alias = "focusX")]
    pub focus_x: f64,
    #[serde(alias = "focusY")]
    pub focus_y: f64,
    pub port: u16,
}

impl Default for WallpaperSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            blur: 0.0,
            dim: 25.0,
            opacity: 1.0,
            monet: true,
            visible: true,
            fit: "cover".into(),
            focus_x: 0.5,
            focus_y: 0.5,
            port: 9222,
        }
    }
}

impl WallpaperSettings {
    pub fn normalized(&self) -> Self {
        let mut s = self.clone();
        s.blur = bounded(s.blur, 0.0, 80.0, 0.0);
        s.dim = bounded(s.dim, 0.0, 100.0, 25.0);
        s.opacity = bounded(s.opacity, 0.0, 1.0, 1.0);
        s.focus_x = bounded(s.focus_x, 0.0, 1.0, 0.5);
        s.focus_y = bounded(s.focus_y, 0.0, 1.0, 0.5);
        s.fit = match s.fit.trim().to_ascii_lowercase().as_str() {
            "contain" => "contain",
            "smart" => "smart",
            "stretch" => "stretch",
            _ => "cover",
        }.into();
        if s.port == 0 { s.port = 9222; }
        s
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WallpaperReport {
    pub windows: usize,
    /// 所有窗口均已使用相同内容和设置，无需重新注入。
    pub reused: bool,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WallpaperHealth {
    pub healthy: bool,
    pub needs_apply: bool,
    pub windows: usize,
    pub message: String,
}

fn bounded(value: f64, min: f64, max: f64, fallback: f64) -> f64 {
    if value.is_finite() { value.clamp(min, max) } else { fallback }
}

fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_limited(path: &Path, limit: u64, message: &str) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| message.to_string())?;
    if !file.metadata().map_err(|_| message.to_string())?.is_file() {
        return Err(message.into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).map_err(|_| message.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("文件过大，无法作为壁纸配置或图像读取。".into());
    }
    Ok(bytes)
}

fn image_format(bytes: &[u8]) -> Result<(ImageFormat, &'static str), String> {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => Ok((ImageFormat::Png, "png")),
        Ok(ImageFormat::Jpeg) => Ok((ImageFormat::Jpeg, "jpg")),
        Ok(ImageFormat::WebP) => Ok((ImageFormat::WebP, "webp")),
        Ok(ImageFormat::Gif) => Ok((ImageFormat::Gif, "gif")),
        Ok(ImageFormat::Bmp) => Ok((ImageFormat::Bmp, "bmp")),
        _ => Err("请选择 PNG、JPEG、WebP、GIF 或 BMP 图片。".into()),
    }
}

fn decode_image(bytes: &[u8]) -> Result<DynamicImage, String> {
    let (format, _) = image_format(bytes)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(24_000);
    limits.max_image_height = Some(24_000);
    limits.max_alloc = Some(384 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "图片无法解码或尺寸过大。")?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 60_000_000 {
        return Err("图片尺寸不受支持（最多 6000 万像素）。".into());
    }
    let orientation = decoder.orientation().map_err(|_| "图片方向信息损坏。")?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "图片数据损坏或解码失败。")?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// 校验并复制到传入持久根，按实际格式与 SHA-256 命名；不返回路径/解码器原始错误。
/// GIF/WebP 动画显示第一帧，与静态壁纸一致；原始文件完整保留。
pub fn import_image(source: &Path, data_dir: &Path) -> Result<PathBuf, String> {
    let _guard = OPERATION.lock().map_err(|_| "壁纸操作锁不可用。")?;
    import_image_inner(source, data_dir)
}

fn import_image_inner(source: &Path, data_dir: &Path) -> Result<PathBuf, String> {
    let bytes = read_limited(source, MAX_IMAGE_BYTES, "无法读取所选壁纸，请检查文件是否存在。")?;
    let (_, extension) = image_format(&bytes)?;
    decode_image(&bytes)?;
    let hash = content_hash(&bytes);
    let root = data_dir.join("wallpapers");
    fs::create_dir_all(&root).map_err(|_| "无法创建持久壁纸目录。")?;
    // canonicalize 确保即使调用方传相对路径，返回值仍可直接用于预览/跨重启读取。
    let root = fs::canonicalize(root).map_err(|_| "无法定位持久壁纸目录。")?;
    let destination = root.join(format!("{hash}.{extension}"));
    if destination.exists() {
        let old = read_limited(&destination, MAX_IMAGE_BYTES, "无法读取已导入的壁纸。")?;
        if content_hash(&old) == hash { return Ok(destination); }
        return Err("持久壁纸内容与校验值不一致，请移走损坏文件后重新导入。".into());
    }
    // create_new 防止并发覆盖；只清理由本次创建但尚未写完的文件。
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&destination) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let old = read_limited(&destination, MAX_IMAGE_BYTES, "壁纸正由另一个操作导入，请重试。")?;
            if content_hash(&old) == hash { return Ok(destination); }
            return Err("壁纸正在导入或持久文件损坏，请重试。".into());
        }
        Err(_) => return Err("无法保存到持久壁纸目录。".into()),
    };
    if file.write_all(&bytes).and_then(|_| file.sync_all()).is_err() {
        drop(file);
        let _ = fs::remove_file(&destination);
        return Err("壁纸复制失败，请检查剩余空间和目录权限。".into());
    }
    Ok(destination)
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|p| p.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy())) {
        paths.push(path);
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn legacy_configs() -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Some(root) = env_path("ZCODE_BEAUTIFY_DATA_DIR") {
        push_unique(&mut result, root.join("config.json"));
    }
    let home = env_path("USERPROFILE").or_else(|| env_path("HOME"));
    // Python _wp_shared_dir 与插件 dataDir 的实际首选位置。
    if let Some(home) = &home {
        push_unique(&mut result, home.join(".zcode/cli/plugins/data/zcode-beautify@zcode-beautify/config.json"));
    }
    let mut homes = Vec::new();
    for key in ["ZCODE_HOME", "ZCODE_CLI_HOME"] {
        if let Some(root) = env_path(key) { push_unique(&mut homes, root); }
    }
    if let Some(root) = env_path("APPDATA") { push_unique(&mut homes, root.join(".zcode")); }
    if let Some(home) = home { push_unique(&mut homes, home.join(".zcode")); }
    for root in homes {
        for name in ["zcode-beautify@zcode-beautify", "zcode-beautify"] {
            push_unique(&mut result, root.join("cli/plugins/data").join(name).join("config.json"));
        }
    }
    result
}

/// 只读旧配置；调用方仅在尚无 Rust 壁纸设置时迁移一次并保存返回值。
/// 不修改插件配置，不扫描图片目录，不读取 CDP 中的图片内容。
pub fn migrate(data_dir: &Path) -> Result<Option<WallpaperSettings>, String> {
    let _guard = OPERATION.lock().map_err(|_| "壁纸操作锁不可用。")?;
    for config in legacy_configs() {
        if !config.is_file() { continue; }
        let bytes = read_limited(&config, MAX_CONFIG_BYTES, "无法读取旧壁纸配置。")?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "旧壁纸配置编码无效。")?;
        let mut settings = parse_legacy_config(text)?;
        if !settings.path.is_empty() {
            let source = PathBuf::from(&settings.path);
            let source = if source.is_absolute() { source } else {
                config.parent().ok_or("旧壁纸配置路径无效。")?.join(source)
            };
            settings.path = import_image_inner(&source, data_dir)?.to_string_lossy().into_owned();
        }
        return Ok(Some(settings));
    }
    Ok(None)
}

/// 可单测的旧插件配置转换；忽略其它插件字段。
pub fn parse_legacy_config(text: &str) -> Result<WallpaperSettings, String> {
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|_| "旧壁纸配置不是有效 JSON。")?;
    let object = value.as_object().ok_or("旧壁纸配置必须是 JSON 对象。")?;
    let get = |key: &str, old: &str| object.get(key).or_else(|| object.get(old));
    let mut s = WallpaperSettings::default();
    s.path = get("path", "wallpaperPath").and_then(Value::as_str).unwrap_or("").into();
    s.enabled = object.get("enabled").and_then(Value::as_bool).unwrap_or(!s.path.is_empty());
    s.visible = get("visible", "wallpaperVisible").and_then(Value::as_bool).unwrap_or(s.visible);
    s.monet = object.get("monet").and_then(Value::as_bool).unwrap_or(s.monet);
    s.fit = object.get("fit").and_then(Value::as_str).unwrap_or(&s.fit).into();
    for (slot, key, old) in [
        (&mut s.blur, "blur", "blur"), (&mut s.dim, "dim", "dim"),
        (&mut s.opacity, "opacity", "opacity"),
        (&mut s.focus_x, "focus_x", "focusX"), (&mut s.focus_y, "focus_y", "focusY"),
    ] {
        if let Some(value) = get(key, old).and_then(Value::as_f64) { *slot = value; }
    }
    if let Some(port) = object.get("port").and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok()) {
        s.port = port;
    }
    Ok(s.normalized())
}

struct Assets {
    hash: String,
    data_uri: String,
    width: u32,
    height: u32,
    focus: (f64, f64),
    theme: Theme,
}

/// 根据颜色偏离均值的权重寻找显著区域；透明像素不参与。
fn analyze_focus(image: &RgbaImage) -> (f64, f64) {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 { return (0.5, 0.5); }
    let stride = ((f64::from(width) * f64::from(height) / 24_000.0).sqrt().ceil() as usize).max(1);
    let mut samples = Vec::new();
    let mut sums = [0.0; 3];
    for y in (0..height).step_by(stride) {
        for x in (0..width).step_by(stride) {
            let p = image.get_pixel(x, y);
            if p[3] < 128 { continue; }
            for i in 0..3 { sums[i] += f64::from(p[i]); }
            samples.push((x, y, *p));
        }
    }
    if samples.is_empty() { return (0.5, 0.5); }
    let count = samples.len() as f64;
    let means = sums.map(|v| v / count);
    let (mut wx, mut wy, mut total) = (0.0, 0.0, 0.0);
    for (x, y, p) in samples {
        let weight = 1.0 + (0..3).map(|i| (f64::from(p[i]) - means[i]).powi(2)).sum::<f64>();
        wx += (f64::from(x) + 0.5) * weight;
        wy += (f64::from(y) + 0.5) * weight;
        total += weight;
    }
    ((wx / total / f64::from(width)).clamp(0.0, 1.0),
     (wy / total / f64::from(height)).clamp(0.0, 1.0))
}

fn source_color(image: &RgbaImage) -> Rgb {
    let (width, height) = image.dimensions();
    let stride = ((f64::from(width) * f64::from(height) / 24_000.0).sqrt().ceil() as usize).max(1);
    let mut pixels = Vec::new();
    for y in (0..height).step_by(stride) {
        for x in (0..width).step_by(stride) {
            let p = image.get_pixel(x, y);
            if p[3] >= 128 { pixels.push(Rgb::new(p[0], p[1], p[2])); }
        }
    }
    let fallback = Rgb::new(103, 80, 164);
    if pixels.is_empty() { return fallback; }
    let quantized = QuantizerCelebi::quantize(&pixels, 64);
    Score::score(&quantized.color_to_count, Some(1), Some(fallback), Some(true))
        .into_iter().next().unwrap_or(fallback)
}

fn load_assets(path: &Path) -> Result<Arc<Assets>, String> {
    let bytes = read_limited(path, MAX_IMAGE_BYTES, "壁纸文件不可读或已丢失，请重新选择图片。")?;
    let hash = content_hash(&bytes);
    let cache = ASSETS.get_or_init(|| Mutex::new(None));
    if let Some(assets) = cache.lock().map_err(|_| "壁纸缓存不可用。")?.as_ref() {
        if assets.hash == hash { return Ok(Arc::clone(assets)); }
    }
    let image = decode_image(&bytes)?.thumbnail(2560, 2560).to_rgba8();
    let focus = analyze_focus(&image);
    let theme = ThemeBuilder::with_source(source_color(&image)).build();
    // 注入重新编码的像素，不传源文件路径、EXIF 或其它元数据。
    let rgb = image::RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y);
        let a = u32::from(p[3]);
        image::Rgb(std::array::from_fn(|i| {
            ((u32::from(p[i]) * a + [18, 18, 22][i] * (255 - a) + 127) / 255) as u8
        }))
    });
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 85)
        .encode_image(&rgb).map_err(|_| "壁纸预处理失败。")?;
    let assets = Arc::new(Assets {
        hash,
        data_uri: format!("data:image/jpeg;base64,{}", STANDARD.encode(encoded)),
        width: image.width(), height: image.height(), focus, theme,
    });
    *cache.lock().map_err(|_| "壁纸缓存不可用。")? = Some(Arc::clone(&assets));
    Ok(assets)
}

/// smart 以当前视口覆盖比例决定 contain/cover，与 JS resize 路径一致。
pub fn resolved_fit(fit: &str, image: (u32, u32), viewport: (u32, u32)) -> &'static str {
    match fit {
        "contain" => "contain",
        "stretch" => "100% 100%",
        "smart" if image.0 > 0 && image.1 > 0 && viewport.0 > 0 && viewport.1 > 0 => {
            let aspect = f64::from(image.0) / f64::from(image.1);
            let view = f64::from(viewport.0) / f64::from(viewport.1);
            if (aspect / view).min(view / aspect) < 0.60 { "contain" } else { "cover" }
        }
        _ => "cover",
    }
}

/// 将图中焦点转换为 CSS background-position 百分比，尽量保持焦点位于视口中央。
pub fn cover_position(image: (u32, u32), viewport: (u32, u32), focus: (f64, f64)) -> (f64, f64) {
    if image.0 == 0 || image.1 == 0 || viewport.0 == 0 || viewport.1 == 0 { return (0.5, 0.5); }
    let (iw, ih) = (f64::from(image.0), f64::from(image.1));
    let (w, h) = (f64::from(viewport.0), f64::from(viewport.1));
    let scale = (w / iw).max(h / ih);
    let axis = |size: f64, view: f64, f: f64| {
        let excess = size * scale - view;
        if excess > 0.0001 { ((size * scale * bounded(f, 0.0, 1.0, 0.5) - view / 2.0) / excess).clamp(0.0, 1.0) }
        else { 0.5 }
    };
    (axis(iw, w, focus.0), axis(ih, h, focus.1))
}

fn color_css(color: Rgb, alpha: f64) -> String {
    if alpha >= 1.0 { format!("#{:02x}{:02x}{:02x}", color.red, color.green, color.blue) }
    else { format!("rgba({},{},{},{:.3})", color.red, color.green, color.blue, alpha) }
}

fn theme_rows(theme: &Theme, dark: bool, visible: bool) -> String {
    let scheme = if dark { &theme.schemes.dark } else { &theme.schemes.light };
    // 保留原插件 CorePalette 的中性色调，避免迁移后面板底色突变。
    #[allow(deprecated)]
    let neutral = &theme.palettes.neutral;
    let (low, container, high, highest, surface) = if dark { (10, 12, 17, 22, 6) } else { (96, 94, 92, 90, 98) };
    let (base, panel, input, popover) = if visible { (0.72, 0.62, 0.50, 0.92) } else { (1.0, 1.0, 1.0, 1.0) };
    let mut result = format!("--color-background:{};--zcode-token-bar-base:{};",
        if visible { "transparent".into() } else { color_css(neutral.tone(surface), 1.0) },
        color_css(neutral.tone(surface), 1.0));
    let values = [
        ("background-alt", neutral.tone(high), panel),
        ("background-win-alt", neutral.tone(low), panel),
        ("panel", neutral.tone(low), panel), ("sidebar", neutral.tone(low), panel),
        ("surface", neutral.tone(surface), base), ("surface-hover", neutral.tone(high), base),
        ("card", neutral.tone(container), base), ("card-selected", neutral.tone(highest), (base + 0.15_f64).min(1.0)),
        ("card-border", scheme.outline_variant, 0.5), ("popover", neutral.tone(container), popover),
        ("input", neutral.tone(highest), input), ("input-focused", neutral.tone(highest), (input + 0.2_f64).min(1.0)),
        ("input-border", scheme.outline_variant, 0.6), ("input-border-hover", scheme.outline, 0.7),
        ("input-border-focused", scheme.primary, 0.9),
        ("foreground", scheme.on_surface, 1.0), ("foreground-subtle", scheme.on_surface_variant, 1.0),
        ("foreground-subtlest", scheme.on_surface_variant, 0.72), ("foreground-inverse", scheme.inverse_on_surface, 1.0),
        ("primary", scheme.primary, 1.0), ("primary-foreground", scheme.on_primary, 1.0),
        ("secondary", scheme.secondary_container, 1.0), ("accent", scheme.tertiary, 1.0), ("brand", scheme.primary, 1.0),
        ("border", scheme.outline_variant, 0.55), ("border-hover", scheme.outline_variant, 0.9),
        ("border-color-interactive", scheme.outline_variant, 0.7), ("border-color-interactive-hover", scheme.outline_variant, 1.0),
        ("border-color-interactive-active", scheme.primary, 0.9),
        ("find-highlight", scheme.tertiary_container, 1.0), ("find-highlight-active", scheme.secondary_container, 1.0),
    ];
    for (name, color, alpha) in values {
        result.push_str(&format!("--color-{name}:{};", color_css(color, alpha)));
    }
    result.push_str(&format!("--divider-color:{};", color_css(scheme.outline_variant, 0.4)));
    result
}

fn transparency_rows(dark: bool) -> String {
    let rgb = if dark { Rgb::new(18, 18, 22) } else { Rgb::new(255, 255, 255) };
    let mut result = format!("--color-background:transparent;--zcode-token-bar-base:{};", color_css(rgb, 1.0));
    for (name, alpha) in [
        ("background-alt", 0.62), ("background-win-alt", 0.62), ("panel", 0.62), ("sidebar", 0.62),
        ("surface", 0.72), ("surface-hover", 0.72), ("card", 0.72), ("card-selected", 0.87),
        ("popover", 0.92), ("input", 0.50), ("input-focused", 0.70),
    ] {
        result.push_str(&format!("--color-{name}:{};", color_css(rgb, alpha)));
    }
    result
}

fn build_css(s: &WallpaperSettings, assets: &Assets) -> String {
    let mut css = String::new();
    if s.monet {
        css.push_str(&format!(":root,:host{{{}}}.dark{{{}}}",
            theme_rows(&assets.theme, false, s.visible), theme_rows(&assets.theme, true, s.visible)));
    } else if s.visible {
        css.push_str(&format!(":root,:host{{{}}}.dark{{{}}}", transparency_rows(false), transparency_rows(true)));
    }
    if s.visible {
        css.push_str(&format!(r#"
html {{ background:var(--zcode-token-bar-base) !important; }}
body {{ background:transparent !important; }}
#zcode-beautify-wallpaper,#zcode-beautify-backdrop {{
 position:fixed; inset:0; pointer-events:none; background-repeat:no-repeat;
 opacity:{opacity}; visibility:visible; display:block;
}}
#zcode-beautify-wallpaper {{ z-index:-2147483646; filter:blur({blur}px) brightness({brightness}); }}
#zcode-beautify-backdrop {{
 z-index:-2147483647; background-size:cover; display:none;
 filter:blur(28px) saturate(1.15) brightness({backdrop_brightness});
}}
#zcode-beautify-backdrop[data-on="1"] {{ display:block; }}
"#,
            opacity = s.opacity, blur = s.blur, brightness = 1.0 - s.dim / 100.0,
            backdrop_brightness = 0.85 * (1.0 - s.dim / 100.0)));
    }
    css
}

fn fingerprint(s: &WallpaperSettings, hash: &str) -> String {
    // 路径与端口不影响视觉；相同内容换文件名仍可复用。
    content_hash(json!({
        "version": VERSION, "hash": hash, "enabled": s.enabled,
        "blur": s.blur, "dim": s.dim, "opacity": s.opacity, "monet": s.monet,
        "visible": s.visible, "fit": s.fit, "x": s.focus_x, "y": s.focus_y,
    }).to_string().as_bytes())
}

fn payload(s: &WallpaperSettings, assets: Option<&Assets>, operation: &str) -> Value {
    let mut p = json!({"op": operation, "enabled": s.enabled, "version": VERSION});
    if let Some(assets) = assets {
        p["fingerprint"] = fingerprint(s, &assets.hash).into();
        p["imageHash"] = assets.hash.clone().into();
        p["visible"] = s.visible.into();
        if operation == "apply" {
            p["css"] = build_css(s, assets).into();
            p["dataUri"] = if s.visible { assets.data_uri.clone() } else { String::new() }.into();
            p["width"] = assets.width.into(); p["height"] = assets.height.into();
            p["autoX"] = assets.focus.0.into(); p["autoY"] = assets.focus.1.into();
            p["fit"] = s.fit.clone().into();
            p["focusX"] = s.focus_x.into(); p["focusY"] = s.focus_y.into();
            p["blur"] = s.blur.into(); p["opacity"] = s.opacity.into();
        }
    }
    p
}

fn expression(payload: &Value) -> String {
    format!("{RUNTIME}({payload})")
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

fn loopback_host(host: &str) -> Option<IpAddr> {
    if host.eq_ignore_ascii_case("localhost") { return Some(IpAddr::V4(Ipv4Addr::LOCALHOST)); }
    host.trim_matches(['[', ']']).parse::<IpAddr>().ok().filter(IpAddr::is_loopback)
}

/// 明确识别 ZCode renderer；绝不退回任意 page/devtools/扩展页。
pub fn is_zcode_renderer(kind: &str, title: &str, url: &str) -> bool {
    if kind != "page" { return false; }
    let Ok(url) = Url::parse(url) else { return false; };
    let local = match url.scheme() {
        "file" | "app" | "zcode" => true,
        "http" | "https" => url.host_str().and_then(loopback_host).is_some(),
        _ => false,
    };
    local && (url.path().ends_with("/out/renderer/index.html") || title == "ZCode")
}

fn websocket_endpoint(raw: &str, expected_port: u16) -> Result<(Url, SocketAddr), String> {
    let url = Url::parse(raw).map_err(|_| "CDP 返回了无效 WebSocket 地址。")?;
    let ip = url.host_str().and_then(loopback_host).ok_or("拒绝连接非本机 CDP 地址。")?;
    if url.scheme() != "ws" || url.port_or_known_default() != Some(expected_port)
        || !url.username().is_empty() || url.password().is_some() || url.query().is_some()
        || url.fragment().is_some() || !url.path().starts_with("/devtools/page/")
        || url.path().trim_start_matches("/devtools/page/").is_empty()
    {
        return Err("CDP WebSocket 地址不属于当前本机页面。".into());
    }
    Ok((url, SocketAddr::new(ip, expected_port)))
}

fn list_targets(port: u16) -> Result<Vec<Target>, String> {
    let client = Client::builder().no_proxy().redirect(Policy::none())
        .connect_timeout(IO_TIMEOUT).timeout(IO_TIMEOUT)
        .build().map_err(|_| "无法初始化本机 CDP 连接。")?;
    let response = client.get(format!("http://127.0.0.1:{port}/json/list"))
        .send().map_err(|_| "无法连接本机 CDP；请确认 ZCode 已启用远程调试端口。")?;
    if !response.status().is_success() { return Err("本机 CDP 页面列表请求失败。".into()); }
    let mut bytes = Vec::new();
    response.take(MAX_CDP_BYTES as u64 + 1).read_to_end(&mut bytes)
        .map_err(|_| "无法读取本机 CDP 页面列表。")?;
    if bytes.len() > MAX_CDP_BYTES { return Err("本机 CDP 页面列表过大。".into()); }
    let targets: Vec<Target> = serde_json::from_slice(&bytes).map_err(|_| "本机 CDP 页面列表格式无效。")?;
    let mut selected = Vec::new();
    for target in targets {
        if !is_zcode_renderer(&target.kind, &target.title, &target.url) || target.websocket.is_empty() { continue; }
        websocket_endpoint(&target.websocket, port)?;
        if !selected.iter().any(|t: &Target| t.websocket == target.websocket) { selected.push(target); }
    }
    if selected.is_empty() { return Err("CDP 已连接，尚未找到 ZCode 渲染窗口。".into()); }
    if selected.len() > 32 { return Err("ZCode 渲染窗口数量超出预期。".into()); }
    Ok(selected)
}

struct Cdp {
    socket: WebSocket<TcpStream>,
    next_id: u64,
}

impl Cdp {
    fn connect(target: &Target, port: u16) -> Result<Self, String> {
        let (url, address) = websocket_endpoint(&target.websocket, port)?;
        // 自己创建 loopback TCP，不让 WebSocket 库做代理或非本机 DNS 解析。
        let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)
            .map_err(|_| "无法连接 ZCode 渲染窗口。")?;
        stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|_| "无法设置 CDP 读取超时。")?;
        stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|_| "无法设置 CDP 写入超时。")?;
        let config = WebSocketConfig::default().max_message_size(Some(MAX_CDP_BYTES)).max_frame_size(Some(MAX_CDP_BYTES));
        let (socket, _) = tungstenite::client::client_with_config(url.as_str(), stream, Some(config))
            .map_err(|_| "ZCode CDP WebSocket 握手失败。")?;
        Ok(Self { socket, next_id: 1 })
    }

    fn evaluate(&mut self, code: &str) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"id": id, "method": "Runtime.evaluate", "params": {
            "expression": code, "returnByValue": true, "awaitPromise": true,
            "timeout": EVAL_TIMEOUT.as_millis() as u64, "silent": true,
        }});
        self.socket.send(Message::Text(request.to_string().into())).map_err(|_| "CDP 壁纸请求发送失败。")?;
        let deadline = Instant::now() + EVAL_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err("等待 ZCode 壁纸脚本执行超时。".into()); }
            self.socket.get_mut().set_read_timeout(Some(remaining)).map_err(|_| "无法设置 CDP 超时。")?;
            match self.socket.read().map_err(|_| "CDP 连接中断或执行超时。")? {
                Message::Text(text) => {
                    let response: Value = serde_json::from_str(&text).map_err(|_| "CDP 返回了无效 JSON。")?;
                    if response.get("id").and_then(Value::as_u64) != Some(id) { continue; }
                    return evaluation_value(&response);
                }
                Message::Ping(_) => { self.socket.flush().map_err(|_| "CDP 心跳响应失败。")?; }
                Message::Close(_) => return Err("ZCode 渲染窗口已关闭。".into()),
                _ => {}
            }
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) { let _ = self.socket.close(None); }
}

fn evaluation_value(response: &Value) -> Result<Value, String> {
    // 绝不把 exception.description 回显；其中可能含完整注入脚本或图像 data URI。
    if response.get("error").is_some() { return Err("CDP 拒绝执行壁纸请求。".into()); }
    let result = response.get("result").ok_or("CDP 没有返回执行结果。")?;
    if result.get("exceptionDetails").is_some() { return Err("ZCode 壁纸脚本执行异常。".into()); }
    let remote = result.get("result").ok_or("CDP 没有返回脚本结果。")?;
    if remote.get("subtype").and_then(Value::as_str) == Some("error") {
        return Err("ZCode 壁纸脚本返回错误。".into());
    }
    remote.get("value").cloned().ok_or_else(|| "CDP 脚本没有返回可验证的结果。".into())
}

/// 同页刷新/重启/旧插件重注入都会令 healthy=false；离线与缺图返回可直接显示的短错误。
pub fn health(settings: &WallpaperSettings) -> Result<WallpaperHealth, String> {
    let _guard = OPERATION.lock().map_err(|_| "壁纸操作锁不可用。")?;
    let s = settings.normalized();
    let assets = if s.enabled { Some(load_assets(Path::new(&s.path))?) } else { None };
    let targets = list_targets(s.port)?;
    let code = expression(&payload(&s, assets.as_deref(), "health"));
    let mut healthy = true;
    for target in &targets {
        let value = Cdp::connect(target, s.port)?.evaluate(&code)?;
        healthy &= value.get("healthy").and_then(Value::as_bool).ok_or("壁纸健康检查返回无效结果。")?;
    }
    Ok(WallpaperHealth {
        healthy, needs_apply: !healthy, windows: targets.len(),
        message: if healthy { if s.enabled { "壁纸已生效。" } else { "已恢复默认外观。" } }
            else { "壁纸现场已变化，等待自动恢复。" }.into(),
    })
}

/// 相同图片内容和规范化设置幂等；对所有 ZCode 窗口应用，部分失败不会假报成功。
pub fn apply(settings: &WallpaperSettings) -> Result<WallpaperReport, String> {
    let _guard = OPERATION.lock().map_err(|_| "壁纸操作锁不可用。")?;
    let s = settings.normalized();
    if !s.enabled { return reset_inner(s.port); }
    if s.path.is_empty() { return Err("尚未选择壁纸图片。".into()); }
    let assets = load_assets(Path::new(&s.path))?;
    let targets = list_targets(s.port)?;
    let check = expression(&payload(&s, Some(&assets), "health"));
    let mut apply_code = None;
    let mut reused = true;
    let mut done = 0;
    let mut first_error: Option<String> = None;
    for target in &targets {
        let result = (|| {
            let mut connection = Cdp::connect(target, s.port)?;
            let state = connection.evaluate(&check)?;
            if state.get("healthy").and_then(Value::as_bool) == Some(true) { return Ok(true); }
            let code = apply_code.get_or_insert_with(|| expression(&payload(&s, Some(&assets), "apply")));
            let result = connection.evaluate(code)?;
            if result.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err("壁纸脚本执行后状态校验未通过。".into());
            }
            Ok(result.get("reused").and_then(Value::as_bool).unwrap_or(false))
        })();
        match result {
            Ok(unchanged) => { done += 1; reused &= unchanged; }
            Err(error) => { if first_error.is_none() { first_error = Some(error); } }
        }
    }
    if let Some(error) = first_error {
        return Err(format!("壁纸仅在 {done}/{} 个窗口完成。{error}", targets.len()));
    }
    Ok(WallpaperReport { windows: done, reused,
        message: if reused { "壁纸内容和设置未变，已复用。" } else { "壁纸已应用，刷新或重启后将自动恢复。" }.into() })
}

/// 仅删除本模块、已核实旧插件及 Python 铺法的固定节点，不清其它自定义主题。
pub fn reset(port: u16) -> Result<WallpaperReport, String> {
    let _guard = OPERATION.lock().map_err(|_| "壁纸操作锁不可用。")?;
    reset_inner(if port == 0 { 9222 } else { port })
}

fn reset_inner(port: u16) -> Result<WallpaperReport, String> {
    let targets = list_targets(port)?;
    let code = expression(&payload(&WallpaperSettings::default(), None, "reset"));
    let mut done = 0;
    let mut reused = true;
    let mut first_error: Option<String> = None;
    for target in &targets {
        let result = Cdp::connect(target, port).and_then(|mut c| c.evaluate(&code));
        match result {
            Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
                done += 1;
                reused &= value.get("reused").and_then(Value::as_bool).unwrap_or(false);
            }
            Ok(_) => { first_error.get_or_insert_with(|| "默认外观状态校验未通过。".to_string()); }
            Err(error) => { first_error.get_or_insert(error); }
        }
    }
    if let Some(error) = first_error {
        return Err(format!("仅在 {done}/{} 个窗口恢复默认。{error}", targets.len()));
    }
    Ok(WallpaperReport { windows: done, reused, message: "已恢复默认外观；原始图片与配置仍保留。".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_normalization_are_safe() {
        let s: WallpaperSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, WallpaperSettings::default());
        let mut s = s;
        s.blur = f64::NAN; s.dim = 250.0; s.opacity = -1.0;
        s.focus_x = f64::INFINITY; s.focus_y = -3.0; s.port = 0;
        s.fit = "contain; background:url(https://invalid)".into();
        let n = s.normalized();
        assert_eq!((n.blur, n.dim, n.opacity, n.focus_x, n.focus_y, n.port), (0.0, 100.0, 0.0, 0.5, 0.0, 9222));
        assert_eq!(n.fit, "cover");
    }

    #[test]
    fn migration_understands_plugin_keys_and_hidden_wallpaper() {
        let s = parse_legacy_config(r#"{"wallpaperPath":"old.png","wallpaperVisible":false,"fit":"smart","blur":8,"dim":32,"monet":false,"focusX":0.8,"focusY":0.1,"port":9333,"token":"unused"}"#).unwrap();
        assert!(s.enabled); assert!(!s.visible); assert!(!s.monet);
        assert_eq!((s.blur, s.dim, s.focus_x, s.focus_y, s.port), (8.0, 32.0, 0.8, 0.1, 9333));
        assert_eq!(s.fit, "smart");
        assert!(!parse_legacy_config("{}").unwrap().enabled);
        assert!(parse_legacy_config("[]").is_err());
        assert_eq!(parse_legacy_config("\u{feff}{\"fit\":\"stretch\"}").unwrap().fit, "stretch");
    }

    #[test]
    fn renderer_selection_never_falls_back_to_unrelated_pages() {
        assert!(is_zcode_renderer("page", "ZCode", "file:///D:/app/index.html"));
        assert!(is_zcode_renderer("page", "Project", "file:///D:/app/out/renderer/index.html"));
        assert!(is_zcode_renderer("page", "ZCode", "http://127.0.0.1:5173/"));
        for (kind, title, url) in [
            ("page", "Other", "file:///D:/app/index.html"),
            ("page", "ZCode", "https://example.com/out/renderer/index.html"),
            ("page", "ZCode", "devtools://devtools/bundled/inspector.html"),
            ("service_worker", "ZCode", "file:///D:/out/renderer/index.html"),
            ("page", "ZCode", "chrome-extension://abc/out/renderer/index.html"),
            ("page", "ZCode", "http://127.0.0.1.example.com/")
        ] { assert!(!is_zcode_renderer(kind, title, url)); }
    }

    #[test]
    fn websocket_validation_pins_loopback_port_and_page() {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            assert!(websocket_endpoint(&format!("ws://{host}:9222/devtools/page/abc"), 9222).is_ok());
        }
        for url in [
            "ws://example.com:9222/devtools/page/abc", "ws://192.168.1.2:9222/devtools/page/abc",
            "ws://127.0.0.1:9333/devtools/page/abc", "ws://user@127.0.0.1:9222/devtools/page/abc",
            "wss://127.0.0.1:9222/devtools/page/abc", "ws://127.0.0.1:9222/devtools/browser/abc",
            "ws://127.0.0.1:9222/devtools/page/abc?secret=value", "ws://127.0.0.1:9222/devtools/page/"
        ] { assert!(websocket_endpoint(url, 9222).is_err()); }
    }

    #[test]
    fn cdp_exceptions_are_failures_without_leaking_details() {
        let response = json!({"result":{"exceptionDetails":{"text":"secret-image-data"},"result":{"value":{"ok":true}}}});
        let error = evaluation_value(&response).unwrap_err();
        assert!(!error.contains("secret"));
        assert!(evaluation_value(&json!({"error":{"message":"secret"}})).is_err());
        assert!(evaluation_value(&json!({"result":{"result":{"type":"undefined"}}})).is_err());
        assert_eq!(evaluation_value(&json!({"result":{"result":{"value":{"healthy":false}}}})).unwrap(), json!({"healthy":false}));
    }

    #[test]
    fn content_and_options_determine_reuse_not_filename() {
        let a = WallpaperSettings { enabled: true, path: "one.png".into(), ..Default::default() };
        let mut b = a.clone(); b.path = "two.png".into(); b.port = 9333;
        assert_eq!(fingerprint(&a, &content_hash(b"pixels")), fingerprint(&b, &content_hash(b"pixels")));
        assert_ne!(fingerprint(&a, &content_hash(b"pixels")), fingerprint(&b, &content_hash(b"PIXELS")));
        b.focus_y = 0.1;
        assert_ne!(fingerprint(&a, "same"), fingerprint(&b, "same"));
        b = a.clone(); b.visible = false;
        assert_ne!(fingerprint(&a, "same"), fingerprint(&b, "same"));
    }

    #[test]
    fn fit_and_focus_adapt_to_viewport() {
        assert_eq!(resolved_fit("smart", (2400, 1200), (1600, 900)), "cover");
        assert_eq!(resolved_fit("smart", (2400, 1200), (600, 1200)), "contain");
        assert_eq!(resolved_fit("contain", (2400, 1200), (1600, 900)), "contain");
        assert_eq!(resolved_fit("stretch", (10, 10), (10, 10)), "100% 100%");
        assert_eq!(cover_position((2000, 1000), (1000, 1000), (0.75, 0.25)), (1.0, 0.5));
        assert_eq!(cover_position((1000, 2000), (1000, 1000), (0.25, 0.25)), (0.5, 0.0));
        assert_eq!(cover_position((0, 0), (1000, 1000), (0.75, 0.25)), (0.5, 0.5));
    }

    #[test]
    fn palette_uses_image_colors_and_ignores_transparency() {
        let red = RgbaImage::from_pixel(12, 12, image::Rgba([235, 25, 35, 255]));
        let blue = RgbaImage::from_pixel(12, 12, image::Rgba([20, 70, 240, 255]));
        let red_source = source_color(&red); let blue_source = source_color(&blue);
        assert!(red_source.red > red_source.blue);
        assert!(blue_source.blue > blue_source.red);
        let a = ThemeBuilder::with_source(red_source).build();
        let b = ThemeBuilder::with_source(blue_source).build();
        assert_ne!(a.schemes.dark.primary, b.schemes.dark.primary);
        assert_ne!(a.schemes.dark.primary, a.schemes.light.primary);
        let empty = RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 0]));
        assert_eq!(source_color(&empty), Rgb::new(103, 80, 164));
        assert_eq!(analyze_focus(&empty), (0.5, 0.5));
    }

    #[test]
    fn focus_moves_toward_salient_region() {
        let mut image = RgbaImage::from_pixel(100, 100, image::Rgba([20, 20, 20, 255]));
        for y in 10..30 { for x in 75..95 { image.put_pixel(x, y, image::Rgba([255, 200, 30, 255])); } }
        let (x, y) = analyze_focus(&image);
        assert!(x > 0.75 && y < 0.3);
    }

    #[test]
    fn hidden_wallpaper_uses_opaque_theme_and_css_contains_no_source_path() {
        let assets = Assets { hash: "x".into(), data_uri: String::new(), width: 1, height: 1, focus: (0.5, 0.5),
            theme: ThemeBuilder::with_source(Rgb::new(120, 80, 200)).build() };
        let mut s = WallpaperSettings { enabled: true, path: "private-source.jpg".into(), visible: false, ..Default::default() };
        let css = build_css(&s, &assets);
        assert!(!css.contains("background:transparent"));
        assert!(!css.contains("#zcode-beautify-wallpaper"));
        assert!(!css.contains("private-source"));
        assert!(css.contains("--color-primary:"));
        s.monet = false;
        assert!(build_css(&s, &assets).is_empty());
        s.visible = true; s.dim = 50.0; s.opacity = 0.6;
        let css = build_css(&s, &assets);
        assert!(css.contains("brightness(0.5)"));
        assert!(css.contains("opacity:0.6"));
        assert!(!css.contains("--color-primary:"));
    }

    #[test]
    fn imports_are_deduplicated_and_survive_source_removal() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = env::temp_dir().join(format!("zcode-wallpaper-test-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let source = root.join("selected.data");
        let data = root.join("persistent");
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2, 3).write_to(&mut bytes, ImageFormat::Png).unwrap();
        fs::write(&source, bytes.get_ref()).unwrap();
        let first = import_image(&source, &data).unwrap();
        let second = import_image(&source, &data).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.extension().unwrap(), "png");
        assert!(first.is_absolute());
        fs::remove_file(&source).unwrap();
        assert_eq!(fs::read(&first).unwrap(), *bytes.get_ref());
        fs::write(&source, bytes.get_ref()).unwrap();
        fs::write(&first, b"damaged").unwrap();
        assert!(import_image(&source, &data).is_err());
        assert_eq!(fs::read(&first).unwrap(), b"damaged");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_validation_uses_bytes_not_filename() {
        assert!(decode_image(b"not an image").is_err());
        let image = DynamicImage::new_rgb8(2, 3);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        assert_eq!(image_format(bytes.get_ref()).unwrap().1, "png");
        let decoded = decode_image(bytes.get_ref()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
    }
}
