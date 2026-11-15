//! GitHub 更新：检查永远只读；只有 UI 的明确更新操作才调用 prepare_update。
//! 源码模式不执行 git fetch/pull/merge，不改 dirty 工作区。仅接受明确标识本应用
//! Rust/Windows/架构的独立 exe 或只含单个 exe（及文档）的 zip 发行资产。
//!
//! 依赖：reqwest 0.12(blocking,json,rustls-tls)、serde/serde_json、semver 1、
//! sha2 0.10、zip 2(default-features=false,features=["deflate"])、system 模块。
//! main 在单实例锁之前调用 handle_internal_command()；true 则立即 return。

use crate::system::{self, AppPaths};
use reqwest::blocking::{Client, Response};
use reqwest::{redirect, StatusCode, Url};
use semver::Version;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;
const APPLY_FLAG: &str = "--token-bar-internal-apply";
const MAX_ASSET: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const MAX_JSON: u64 = 8 * 1024 * 1024;
const MAX_CHECKSUMS: u64 = 1024 * 1024;
const MAX_PAGES: usize = 10;
fn err(e: impl std::fmt::Display) -> String { e.to_string() }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository { pub owner: String, pub name: String }
impl Default for Repository {
    fn default() -> Self { Self { owner: "suiren0219".into(), name: "zcode-token-bar".into() } }
}
impl Repository {
    pub fn new(owner: &str, name: &str) -> Result<Self> {
        let repo = Self { owner: owner.into(), name: name.into() }; repo.validate()?; Ok(repo)
    }
    fn validate(&self) -> Result<()> {
        for part in [&self.owner, &self.name] {
            if part.is_empty() || part.len() > 100 || matches!(part.as_str(), "." | "..")
                || !part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                return Err("无效的 GitHub 仓库名".into());
            }
        }
        Ok(())
    }
    pub fn page(&self) -> String { format!("https://github.com/{}/{}", self.owner, self.name) }
    pub fn releases_page(&self) -> String { format!("{}/releases", self.page()) }
    fn api(&self, endpoint: &str) -> String { format!("https://api.github.com/repos/{}/{}/{}", self.owner, self.name, endpoint) }
}

#[derive(Clone, Debug)]
pub enum InstallMode {
    Release,
    /// commit 为完整 HEAD；None 表示 git 不可用或尚无提交，不能假定已最新。
    Source { root: PathBuf, commit: Option<String>, dirty: Option<bool> },
}
#[derive(Clone, Debug)]
pub struct InstalledVersion { pub version: String, pub mode: InstallMode, pub arch: String }
impl InstalledVersion {
    pub fn release(version: &str) -> Self {
        Self { version: version.into(), mode: InstallMode::Release, arch: std::env::consts::ARCH.into() }
    }
    /// 仅从 exe 祖先（或显式 source_root）识别源码模式，不依赖启动工作目录。
    /// git 仅运行 --no-optional-locks rev-parse/status，并禁用全局配置、hooks、fsmonitor。
    pub fn detect(version: &str, source_root: Option<&Path>) -> Result<Self> {
        let exe = std::env::current_exe().map_err(err)?;
        let root = source_root.map(Path::to_path_buf).or_else(|| {
            exe.parent().into_iter().flat_map(Path::ancestors).take(6)
                .find(|p| p.join(".git").exists() && p.join("Cargo.toml").is_file()).map(Path::to_path_buf)
        });
        let Some(root) = root else { return Ok(Self::release(version)); };
        let root = fs::canonicalize(root).map_err(err)?;
        let commit = git_read(&root, &["rev-parse", "--verify", "HEAD"])
            .ok().filter(|s| valid_commit(s.trim())).map(|s| s.trim().to_ascii_lowercase());
        let dirty = git_read(&root, &["status", "--porcelain=v1", "--untracked-files=normal"])
            .ok().map(|s| !s.trim().is_empty());
        Ok(Self { version: version.into(), mode: InstallMode::Source { root, commit, dirty }, arch: std::env::consts::ARCH.into() })
    }
}
fn git_read(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.arg("--no-optional-locks").arg("-c").arg("core.fsmonitor=false")
        .arg("-c").arg("core.hooksPath=").arg("-c").arg("core.untrackedCache=false")
        .args(args).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    let mut child = command.spawn().map_err(err)?;
    let stdout = child.stdout.take().ok_or("git stdout 不可用")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new(); stdout.take(MAX_JSON + 1).read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait().map_err(err)? {
            Some(status) => break status,
            None if start.elapsed() >= Duration::from_secs(8) => {
                let _ = child.kill(); let _ = child.wait(); let _ = reader.join();
                return Err("git 只读查询超时".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let bytes = reader.join().map_err(|_| "git 读取线程失败")?.map_err(err)?;
    if !status.success() || bytes.len() as u64 > MAX_JSON { return Err("git 只读查询失败".into()); }
    String::from_utf8(bytes).map_err(err)
}
fn valid_commit(value: &str) -> bool { matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit()) }

#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String, pub browser_download_url: String, pub size: u64,
    #[serde(default)] pub digest: Option<String>,
    #[serde(default)] pub state: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)] pub draft: bool,
    #[serde(default)] pub prerelease: bool,
    #[serde(default)] pub assets: Vec<ReleaseAsset>,
}
#[derive(Clone, Debug, Deserialize)]
struct Tag { name: String }
#[derive(Deserialize)]
struct Commit { sha: String }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckState { UpToDate, Available, NoRustAsset, SourceChanged, SourceCurrent, Unknown }
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub state: CheckState, pub message: String, pub release_page: String,
    pub latest_release: Option<String>, pub latest_tag: Option<String>,
    pub main_commit: Option<String>, pub warnings: Vec<String>,
    offer: Option<UpdateOffer>,
}
impl CheckResult { pub fn can_update(&self) -> bool { self.offer.is_some() && self.state == CheckState::Available } }
#[derive(Clone, Debug)]
struct UpdateOffer { repository: Repository, release: Release, asset: ReleaseAsset, arch: String }

pub struct UpdateClient { client: Client, repository: Repository }
impl UpdateClient {
    pub fn new(repository: Repository) -> Result<Self> {
        repository.validate()?;
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 { return attempt.error("GitHub 重定向次数过多"); }
            if trusted_github_url(attempt.url()) { attempt.follow() }
            else { attempt.error("拒绝非 HTTPS/GitHub 下载跳转") }
        });
        let client = Client::builder().user_agent("zcode-token-bar-rs-updater/1")
            .https_only(true).redirect(policy).connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90)).build().map_err(err)?;
        Ok(Self { client, repository })
    }

    /// 只获取公开元数据，无认证、无资产下载、无落盘，适合后台线程调用。
    pub fn check(&self, installed: &InstalledVersion) -> Result<CheckResult> {
        let mut warnings = Vec::new();
        let mut any_metadata = false;
        let mut releases_known = false;
        let releases = match self.list::<Release>("releases") {
            Ok(v) => { any_metadata = true; releases_known = true; v }
            Err(e) => { warnings.push(format!("发行版查询失败：{e}")); Vec::new() }
        };
        let tags = match self.list::<Tag>("tags") {
            Ok(v) => { any_metadata = true; v.into_iter().map(|t| t.name).collect::<Vec<_>>() }
            Err(e) => { warnings.push(format!("tag 查询失败：{e}")); Vec::new() }
        };
        let main_commit = match self.get_json::<Commit>(&self.repository.api("commits/main")) {
            Ok(Some(c)) if valid_commit(&c.sha) => { any_metadata = true; Some(c.sha.to_ascii_lowercase()) }
            Ok(Some(_)) => { warnings.push("main commit 格式无效".into()); None }
            Ok(None) => { warnings.push("仓库没有可访问的 main 分支".into()); None }
            Err(e) => { warnings.push(format!("main 查询失败：{e}")); None }
        };
        if !any_metadata { return Err(format!("无法获取 GitHub 更新信息；可访问 {}。{}",
            self.repository.releases_page(), warnings.join("；"))); }
        // 所需的权威查询失败必须报错：不能把限流/404/超时折叠成空列表。
        if (!releases_known && matches!(installed.mode, InstallMode::Release))
            || (main_commit.is_none() && matches!(installed.mode, InstallMode::Source { .. })) {
            return Err(format!("更新检查未完成，无法确定远端状态。{}；可访问 {}。",
                warnings.join("；"), self.repository.releases_page()));
        }
        let mut result = evaluate_updates(&self.repository, installed, &releases, &tags, main_commit)?;
        result.warnings.extend(warnings);
        Ok(result)
    }
    fn list<T: DeserializeOwned>(&self, endpoint: &str) -> Result<Vec<T>> {
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{}?per_page=100&page={page}", self.repository.api(endpoint));
            let batch: Vec<T> = self.get_json(&url)?
                .ok_or_else(|| format!("GitHub {endpoint} 返回 HTTP 404（仓库/接口不可访问），不能判断为没有更新"))?;
            let count = batch.len(); items.extend(batch);
            if count < 100 { return Ok(items); }
        }
        Err("版本列表超过检查上限，不能保证已找到最高版本".into())
    }
    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let url = Url::parse(url).map_err(err)?;
        if url.host_str() != Some("api.github.com") || !trusted_github_url(&url) { return Err("非 GitHub API 地址".into()); }
        let response = self.client.get(url).header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28").send().map_err(network_error)?;
        if response.status() == StatusCode::NOT_FOUND { return Ok(None); }
        let bytes = response_bytes(checked_response(response)?, MAX_JSON)?;
        serde_json::from_slice(&bytes).map(Some).map_err(err)
    }

    /// 仅用户点更新后调用。下载/验证到独立空目录，准备原生 helper；不替换、不启动。
    pub fn prepare_update(&self, check: &CheckResult, paths: &AppPaths, restart_args: &[String]) -> Result<PreparedUpdate> {
        if !check.can_update() { return Err(format!("当前没有可安装的 Rust 发行资产。{}", check.release_page)); }
        let offer = check.offer.as_ref().ok_or("缺少发行资产")?;
        if offer.repository != self.repository { return Err("检查结果来自不同仓库".into()); }
        system::validate_restart_args(restart_args)?;
        let exe = fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?;
        if !system::paths_equal(&exe, &paths.exe) { return Err("只允许更新正在运行的悬浮条 exe".into()); }
        if !matches!(InstalledVersion::detect("0.0.0", None)?.mode, InstallMode::Release) {
            return Err("源码工作区不执行二进制自更新；请访问发布页或手动构建".into());
        }
        let work = system::new_work_dir(&paths.update_dir, "update")?;
        let result = self.prepare_in(&work, &exe, paths, restart_args, offer);
        if result.is_err() { let _ = fs::remove_dir_all(&work); }
        result
    }
    fn prepare_in(&self, work: &Path, exe: &Path, paths: &AppPaths, args: &[String], offer: &UpdateOffer) -> Result<PreparedUpdate> {
        let asset = &offer.asset;
        validate_asset_url(&self.repository, &offer.release.tag_name, asset)?;
        if asset.size == 0 || asset.size > MAX_ASSET { return Err("发行资产大小无效或超过 256 MiB".into()); }
        let digest = asset.digest.as_deref().map(parse_digest).transpose()?;
        let mut checksums = Vec::new();
        for other in &offer.release.assets {
            if checksum_asset(&other.name, &asset.name) {
                validate_asset_url(&self.repository, &offer.release.tag_name, other)?;
                if other.size == 0 || other.size > MAX_CHECKSUMS { return Err("SHA256 清单过大或为空".into()); }
                let bytes = self.download_bytes(other, MAX_CHECKSUMS)?;
                if let Some(expected) = other.digest.as_deref().map(parse_digest).transpose()? {
                    verify_bytes(&bytes, &expected)?;
                }
                let text = std::str::from_utf8(&bytes).map_err(err)?;
                let specific = other.name.eq_ignore_ascii_case(&format!("{}.sha256", asset.name))
                    || other.name.eq_ignore_ascii_case(&format!("{}.sha256sum", asset.name));
                let expected = checksum_for_asset(text, &asset.name, specific)?
                    .ok_or("SHA256 清单没有包含本次资产，停止更新")?;
                checksums.push(expected);
            }
        }
        let download_dir = work.join("download"); fs::create_dir(&download_dir).map_err(err)?;
        let archive = download_dir.join("asset.bin");
        let response = checked_response(self.client.get(&asset.browser_download_url).send().map_err(network_error)?)?;
        let mut file = OpenOptions::new().create_new(true).write(true).open(&archive).map_err(err)?;
        let (size, archive_hash) = copy_hashed(response, &mut file, MAX_ASSET)?;
        file.sync_all().map_err(err)?; drop(file);
        if size != asset.size { return Err("下载大小与 GitHub 发行元数据不一致".into()); }
        for expected in digest.iter().chain(checksums.iter()) {
            if !archive_hash.eq_ignore_ascii_case(expected) { return Err("SHA256 校验失败，未准备更新".into()); }
        }
        let payload_dir = work.join("payload"); fs::create_dir(&payload_dir).map_err(err)?;
        let payload = payload_dir.join("replacement.exe");
        if asset.name.to_ascii_lowercase().ends_with(".zip") {
            extract_executable(&archive, &payload, &offer.arch)?;
        } else {
            copy_new(&archive, &payload)?;
            validate_pe(&payload, &offer.arch)?;
        }
        let payload_hash = hash_file(&payload)?;
        let original_hash = hash_file(exe)?;
        // helper 是当前可信 exe 的副本，下载内容从不作为 helper 执行。
        let helper = work.join("token-bar-update-helper.exe"); copy_new(exe, &helper)?;
        if hash_file(&helper)? != original_hash { return Err("更新 helper 复制校验失败".into()); }
        let pending = PendingUpdate { schema: 1, parent: system::current_process_identity()?,
            target: exe.to_path_buf(), payload, payload_hash, original_hash,
            arch: offer.arch.clone(), args: args.to_vec(), data_dir: paths.data_dir.clone(),
            tag: offer.release.tag_name.clone() };
        let manifest = work.join("pending.json");
        system::write_new(&manifest, &serde_json::to_vec_pretty(&pending).map_err(err)?)?;
        let integrity = if digest.is_some() || !checksums.is_empty() { Integrity::PublisherSha256 }
            else { Integrity::HttpsOnly };
        Ok(PreparedUpdate { manifest, helper, data_dir: paths.data_dir.clone(), tag: pending.tag,
            integrity, sha256: archive_hash, release_page: self.repository.releases_page() })
    }
    fn download_bytes(&self, asset: &ReleaseAsset, limit: u64) -> Result<Vec<u8>> {
        let response = checked_response(self.client.get(&asset.browser_download_url).send().map_err(network_error)?)?;
        let bytes = response_bytes(response, limit)?;
        if bytes.len() as u64 != asset.size { return Err("SHA256 清单下载大小不匹配".into()); }
        Ok(bytes)
    }
}
fn network_error(_: reqwest::Error) -> String {
    // 不把 GitHub 对象存储重定向的签名 query/token 回显给日志或 UI。
    "GitHub 网络请求失败（连接、TLS 或超时），请稍后重试".into()
}
fn checked_response(response: Response) -> Result<Response> {
    check_status(response.status())?;
    Ok(response)
}
fn check_status(status: StatusCode) -> Result<()> {
    if status.is_success() { return Ok(()); }
    if matches!(status, StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS) {
        return Err(format!("GitHub 返回 HTTP {}（请求被拒绝或 API 限流），请稍后重试", status.as_u16()));
    }
    Err(format!("GitHub 返回 HTTP {}", status.as_u16()))
}
fn response_bytes(response: Response, limit: u64) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit) { return Err("响应超过大小限制".into()); }
    let mut bytes = Vec::new(); response.take(limit + 1).read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() as u64 > limit { return Err("响应超过大小限制".into()); }
    Ok(bytes)
}
fn trusted_github_url(url: &Url) -> bool {
    url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
        && url.port().is_none()
        && matches!(url.host_str(), Some("github.com" | "api.github.com" | "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com" | "github-releases.githubusercontent.com"))
}
fn validate_asset_url(repo: &Repository, tag: &str, asset: &ReleaseAsset) -> Result<()> {
    let name = safe_zip_path(&asset.name)?;
    if name.components().count() != 1 || asset.name.ends_with('/') { return Err("资产名称必须是单个安全文件名".into()); }
    let url = Url::parse(&asset.browser_download_url).map_err(err)?;
    if !trusted_github_url(&url) || url.host_str() != Some("github.com") || url.query().is_some() || url.fragment().is_some() {
        return Err("资产必须来自本仓库 HTTPS GitHub release 下载地址".into());
    }
    let mut expected = Url::parse(&repo.page()).map_err(err)?;
    expected.path_segments_mut().map_err(|_| "无效的仓库 URL")?
        .push("releases").push("download").push(tag).push(&asset.name);
    if url != expected { return Err("资产下载地址与仓库、tag 或文件名不匹配".into()); }
    Ok(())
}

/// v 前缀及 rust-v 前缀可接受；保留 SemVer prerelease 次序，忽略 build metadata。
pub fn parse_version(tag: &str) -> Result<Version> {
    let value = tag.strip_prefix("rust-v").or_else(|| tag.strip_prefix("rust-"))
        .or_else(|| tag.strip_prefix('v')).unwrap_or(tag);
    let mut version = Version::parse(value).map_err(|_| format!("不是语义版本：{tag}"))?;
    version.build = semver::BuildMetadata::EMPTY;
    Ok(version)
}
fn newest_tag(tags: &[String], allow_prerelease: bool) -> Option<String> {
    tags.iter().filter_map(|tag| parse_version(tag).ok().map(|v| (v, tag)))
        .filter(|(v, _)| allow_prerelease || v.pre.is_empty()).max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag.clone())
}
fn asset_score(name: &str, arch: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".exe") && !lower.ends_with(".zip") { return None; }
    let tokens: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).collect();
    let has = |s| tokens.contains(&s);
    if !lower.starts_with("zcode-token-bar-") || !(has("rust") || has("rs"))
        || !(has("windows") || has("win") || has("win64") || has("win32"))
        || ["setup", "installer", "python", "symbols", "debug", "source"].iter().any(|s| has(s)) { return None; }
    let architecture_matches = match arch {
        "x86_64" => (lower.contains("x86_64") || has("amd64") || has("x64")) && !has("arm64") && !has("aarch64"),
        "aarch64" => has("aarch64") || has("arm64"),
        "x86" => has("i686") || (has("x86") && !lower.contains("x86_64")),
        _ => false,
    };
    architecture_matches.then_some(if lower.ends_with(".exe") { 2 } else { 1 })
}
fn select_asset(release: &Release, arch: &str) -> Option<ReleaseAsset> {
    let mut matches: Vec<_> = release.assets.iter().filter_map(|asset| {
        if asset.size == 0 || asset.size > MAX_ASSET || (!asset.state.is_empty() && asset.state != "uploaded") { return None; }
        asset_score(&asset.name, arch).map(|score| (score, asset))
    }).collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let first = matches.first()?;
    if matches.get(1).is_some_and(|second| second.0 == first.0) { return None; }
    Some(first.1.clone())
}

/// 纯决策函数，测试无需联网或 git。tag/main 有变更不等于存在 Rust 二进制发行版。
pub fn evaluate_updates(repo: &Repository, installed: &InstalledVersion, releases: &[Release], tags: &[String], main: Option<String>) -> Result<CheckResult> {
    repo.validate()?;
    let current = parse_version(&installed.version).ok();
    let allow_pre = current.as_ref().is_some_and(|v| !v.pre.is_empty());
    let latest = releases.iter().filter(|r| !r.draft && (allow_pre || !r.prerelease))
        .filter_map(|r| parse_version(&r.tag_name).ok().map(|v| (v, r)))
        .filter(|(v, _)| allow_pre || v.pre.is_empty()).max_by(|a, b| a.0.cmp(&b.0));
    let mut result = CheckResult { state: CheckState::Unknown, message: String::new(),
        release_page: repo.releases_page(), latest_release: latest.as_ref().map(|(_, r)| r.tag_name.clone()),
        latest_tag: newest_tag(tags, allow_pre), main_commit: main.clone(), warnings: Vec::new(), offer: None };
    if let InstallMode::Source { commit, dirty, .. } = &installed.mode {
        result.state = match (commit.as_deref(), main.as_deref()) {
            (Some(local), Some(remote)) if local.eq_ignore_ascii_case(remote) => CheckState::SourceCurrent,
            (Some(_), Some(_)) => CheckState::SourceChanged,
            _ => CheckState::Unknown,
        };
        result.message = match result.state {
            CheckState::SourceCurrent => "源码模式：本地 HEAD 与 GitHub main 相同".into(),
            CheckState::SourceChanged => "源码模式：本地 HEAD 与 GitHub main 不同（不代表可快进）".into(),
            _ => "源码模式：暂时无法完整比较本地 HEAD 与 GitHub main".into(),
        };
        result.message.push_str(match dirty {
            Some(true) => "；工作区有未提交改动。不会拉取、覆盖或安装更新，请手动处理源码或访问发布页。",
            Some(false) => "；工作区干净。源码更新请手动处理或访问发布页，不执行 git pull。",
            None => "；工作区状态未知。不会修改工作区，请手动处理或访问发布页。",
        });
        return Ok(result);
    }
    let Some((version, release)) = latest else {
        result.state = CheckState::NoRustAsset;
        result.message = "没有可用的语义版 Rust 发行资产；tag/main 的变更不构成二进制更新。可访问发布页。".into();
        return Ok(result);
    };
    let Some(current) = current else {
        result.message = "本地版本不是语义版本，无法安全判断升级顺序。可访问发布页。".into(); return Ok(result);
    };
    if let Some(asset) = select_asset(release, &installed.arch) {
        if validate_asset_url(repo, &release.tag_name, &asset).is_ok() {
            if version <= current {
                result.state = CheckState::UpToDate;
                result.message = format!("已是最新发行版（本地 {}；远端 {}）", installed.version, release.tag_name);
                return Ok(result);
            }
            result.state = CheckState::Available;
            result.message = format!("发现 Rust 发行版 {} → {}；点击更新才下载并校验。", installed.version, release.tag_name);
            if asset.digest.is_none() && !release.assets.iter().any(|a| checksum_asset(&a.name, &asset.name)) {
                result.warnings.push("发行方没有提供 SHA256；只能校验 HTTPS 来源、结构和本地暂存完整性，不能证明发布者签名。".into());
            }
            result.offer = Some(UpdateOffer { repository: repo.clone(), release: release.clone(), asset, arch: installed.arch.clone() });
            return Ok(result);
        }
    }
    result.state = CheckState::NoRustAsset;
    result.message = format!("发现 {}，但没有可唯一识别的 Rust Windows {} 独立发行资产，不能自动更新。可访问发布页。", release.tag_name, installed.arch);
    Ok(result)
}

fn parse_digest(text: &str) -> Result<String> {
    let digest = text.strip_prefix("sha256:").ok_or("不支持的 digest 算法，要求 sha256")?;
    if !is_sha256(digest) { return Err("无效的 SHA256 digest".into()); }
    Ok(digest.to_ascii_lowercase())
}
fn is_sha256(text: &str) -> bool { text.len() == 64 && text.bytes().all(|c| c.is_ascii_hexdigit()) }
fn checksum_asset(name: &str, asset: &str) -> bool {
    let name = name.to_ascii_lowercase(); let asset = asset.to_ascii_lowercase();
    name == format!("{asset}.sha256") || name == format!("{asset}.sha256sum")
        || matches!(name.as_str(), "sha256sums" | "sha256sums.txt" | "sha256sum.txt" | "checksums.txt" | "checksums.sha256")
}
/// 兼容 GNU sha256sum、BSD SHA256(file)=hash；通用清单必须精确命中文件名。
pub fn parse_checksum_file(text: &str, asset: &str) -> Result<Option<String>> {
    checksum_for_asset(text, asset, false)
}
fn checksum_for_asset(text: &str, asset: &str, allow_bare: bool) -> Result<Option<String>> {
    let lines: Vec<_> = text.trim_start_matches('\u{feff}').lines().map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#')).collect();
    if allow_bare && lines.len() == 1 && is_sha256(lines[0]) { return Ok(Some(lines[0].to_ascii_lowercase())); }
    let mut found: Option<String> = None;
    for line in lines {
        let pair = if let Some(rest) = line.strip_prefix("SHA256 (") {
            rest.rsplit_once(") = ").map(|(name, hash)| (hash.trim(), name))
        } else {
            line.split_once(char::is_whitespace).map(|(hash, name)| (hash, name.trim_start().trim_start_matches('*')))
        };
        let Some((hash, name)) = pair else { continue; };
        let name = name.strip_prefix("./").unwrap_or(name);
        if name != asset { continue; }
        if !is_sha256(hash) { return Err("校验清单中的 SHA256 格式错误".into()); }
        let hash = hash.to_ascii_lowercase();
        if found.as_ref().is_some_and(|old| *old != hash) { return Err("校验清单包含冲突的 SHA256".into()); }
        found = Some(hash);
    }
    Ok(found)
}
fn verify_bytes(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) { Ok(()) } else { Err("SHA256 校验失败".into()) }
}
fn copy_hashed(mut reader: impl Read, writer: &mut impl Write, limit: u64) -> Result<(u64, String)> {
    let mut hash = Sha256::new(); let mut total = 0u64; let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer).map_err(err)?;
        if n == 0 { break; }
        total = total.checked_add(n as u64).ok_or("大小溢出")?;
        if total > limit { return Err("下载或解压大小超过限制".into()); }
        hash.update(&buffer[..n]); writer.write_all(&buffer[..n]).map_err(err)?;
    }
    Ok((total, format!("{:x}", hash.finalize())))
}
pub fn hash_file(path: &Path) -> Result<String> {
    copy_hashed(File::open(path).map_err(err)?, &mut std::io::sink(), MAX_EXPANDED).map(|(_, hash)| hash)
}
fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(destination).map_err(err)?;
    copy_hashed(File::open(source).map_err(err)?, &mut file, MAX_EXPANDED)?;
    file.sync_all().map_err(err)
}

/// 平台无关地验证 Windows zip 名称；拒绝穿越、ADS、UNC、盘符、设备名、别名和控制符。
pub fn safe_zip_path(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('\\') || name.starts_with('/') || name.contains(':') || name.chars().any(|c| c < ' ') {
        return Err("ZIP 条目包含不安全路径".into());
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    let mut path = PathBuf::new();
    for part in trimmed.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty() || matches!(part, "." | "..") || part.ends_with(['.', ' '])
            || part.chars().any(|c| "<>\"|?*".contains(c))
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(&stem[3..], "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")) {
            return Err("ZIP 条目包含 Windows 特殊路径".into());
        }
        path.push(part);
    }
    Ok(path)
}
fn extract_executable(archive: &Path, output: &Path, arch: &str) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(archive).map_err(err)?).map_err(err)?;
    if zip.is_empty() || zip.len() > 4096 { return Err("ZIP 条目数量无效".into()); }
    let mut names = HashSet::new(); let mut total = 0u64; let mut executable = None;
    for index in 0..zip.len() {
        let file = zip.by_index(index).map_err(err)?;
        let name = std::str::from_utf8(file.name_raw()).map_err(|_| "ZIP 条目名称不是 UTF-8")?;
        let path = safe_zip_path(name)?;
        let normalized = path.to_string_lossy().replace('\\', "/").to_lowercase();
        if !names.insert(normalized) { return Err("ZIP 有重复或大小写冲突的条目".into()); }
        if file.unix_mode().is_some_and(|mode| {
            let ty = mode & 0o170000; ty != 0 && ty != 0o100000 && ty != 0o040000
        }) { return Err("ZIP 包含符号链接或特殊文件".into()); }
        total = total.checked_add(file.size()).ok_or("ZIP 大小溢出")?;
        if total > MAX_EXPANDED || file.size() > MAX_ASSET { return Err("ZIP 展开大小超限".into()); }
        if file.is_dir() { continue; }
        let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
        if base.ends_with(".exe") {
            if !matches!(base.as_str(), "zcode-token-bar.exe" | "zcode-token-bar-rs.exe") || executable.is_some() {
                return Err("ZIP 不是单个已知 Rust 悬浮条 exe 的独立发行包".into());
            }
            executable = Some(index);
        } else if !matches!(path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase().as_str(), "md" | "txt" | "sha256")
            && !matches!(base.as_str(), "license" | "copying" | "notice") {
            return Err("发行包包含需要联合安装的文件；请从发布页手动安装".into());
        }
    }
    let index = executable.ok_or("ZIP 中没有 Rust 悬浮条 exe")?;
    let mut entry = zip.by_index(index).map_err(err)?;
    let expected = entry.size();
    let mut file = OpenOptions::new().write(true).create_new(true).open(output).map_err(err)?;
    let result = (|| {
        let (size, _) = copy_hashed(&mut entry, &mut file, MAX_ASSET)?; // 读到 EOF 触发 CRC 检查。
        if size != expected { return Err("ZIP exe 解压长度不一致".into()); }
        file.sync_all().map_err(err)
    })();
    drop(file);
    let result = result.and_then(|_| validate_pe(output, arch));
    if result.is_err() { let _ = fs::remove_file(output); }
    result
}
/// 只读 PE 头，验证架构和 GUI 子系统；不 LoadLibrary、不运行下载文件。
fn validate_pe(path: &Path, arch: &str) -> Result<()> {
    let mut file = File::open(path).map_err(err)?;
    let size = file.metadata().map_err(err)?.len();
    let mut dos = [0u8; 64]; file.read_exact(&mut dos).map_err(err)?;
    if &dos[..2] != b"MZ" { return Err("下载内容不是 PE 可执行文件".into()); }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset < 64 || offset.checked_add(24 + 70).is_none_or(|n| n > size) { return Err("PE 头越界".into()); }
    file.seek(SeekFrom::Start(offset)).map_err(err)?;
    let mut header = [0u8; 94]; file.read_exact(&mut header).map_err(err)?;
    if &header[..4] != b"PE\0\0" { return Err("PE 签名无效".into()); }
    let machine = u16::from_le_bytes([header[4], header[5]]);
    let expected = match arch { "x86_64" => 0x8664, "aarch64" => 0xaa64, "x86" => 0x14c, _ => return Err("不支持的架构".into()) };
    let opt_size = u16::from_le_bytes([header[20], header[21]]);
    let characteristic = u16::from_le_bytes([header[22], header[23]]);
    let magic = u16::from_le_bytes([header[24], header[25]]);
    let subsystem = u16::from_le_bytes([header[92], header[93]]);
    if offset.checked_add(24 + opt_size as u64).is_none_or(|n| n > size) { return Err("PE 可选头越界".into()); }
    if machine != expected || opt_size < 70 || characteristic & 0x0002 == 0 || characteristic & 0x2000 != 0
        || (arch == "x86" && magic != 0x10b) || (arch != "x86" && magic != 0x20b) || subsystem != 2 {
        return Err("发行 exe 架构/格式不符，或不是 Windows GUI 程序".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integrity { PublisherSha256, HttpsOnly }
#[derive(Debug)]
pub struct PreparedUpdate {
    pub manifest: PathBuf, pub tag: String, pub integrity: Integrity,
    pub sha256: String, pub release_page: String,
    helper: PathBuf, data_dir: PathBuf,
}
impl PreparedUpdate {
    /// 不会让主进程自行退出；成功后 UI 先正常释放单实例锁并退出，helper 才替换。
    pub fn launch(&self) -> Result<u32> {
        let pending: PendingUpdate = serde_json::from_slice(&system::read_limited(&self.manifest, MAX_CHECKSUMS)?).map_err(err)?;
        let identity = system::current_process_identity()?;
        if pending.schema != 1 || pending.parent.pid != identity.pid || pending.parent.created != identity.created
            || !system::paths_equal(&pending.target, &std::env::current_exe().map_err(err)?)
            || hash_file(&pending.target)? != pending.original_hash || hash_file(&self.helper)? != pending.original_hash
            || hash_file(&pending.payload)? != pending.payload_hash {
            return Err("更新载荷、helper 或父进程已变化，请重新准备更新".into());
        }
        system::spawn_hidden(&self.helper, &[OsString::from(APPLY_FLAG), self.manifest.clone().into_os_string()], &self.data_dir)
    }
}
#[derive(Serialize, Deserialize)]
struct PendingUpdate {
    schema: u32, parent: system::ProcessIdentity, target: PathBuf,
    payload: PathBuf, payload_hash: String, original_hash: String,
    arch: String, args: Vec<String>, data_dir: PathBuf, tag: String,
}
#[derive(Serialize)]
struct ApplyResult { success: bool, message: String, backup: Option<PathBuf> }

/// main 最早分派；不依赖 UI、日志、settings。失败写 result.json，旧程序保持/恢复。
pub fn handle_internal_command() -> Result<bool> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().and_then(|s| s.to_str()) != Some(APPLY_FLAG) { return Ok(false); }
    if args.len() != 2 { return Err("更新 helper 参数错误".into()); }
    let manifest = PathBuf::from(&args[1]);
    let manifest = fs::canonicalize(manifest).map_err(err)?;
    let directory = manifest.parent().ok_or("更新清单缺少父目录")?;
    let helper = fs::canonicalize(std::env::current_exe().map_err(err)?).map_err(err)?;
    if manifest.file_name().and_then(|s| s.to_str()) != Some("pending.json")
        || helper.file_name().and_then(|s| s.to_str()) != Some("token-bar-update-helper.exe")
        || !system::paths_equal(helper.parent().ok_or("helper 目录无效")?, directory) {
        return Err("更新 helper 与清单不在同一暂存目录".into());
    }
    let pending: PendingUpdate = serde_json::from_slice(&system::read_limited(&manifest, MAX_CHECKSUMS)?).map_err(err)?;
    // 同一暂存任务只能有一个 helper 执行，文件保留以阻止重复应用。
    system::write_new(&directory.join("apply.lock"), b"1")?;
    let result = apply_pending(directory, &helper, &pending);
    let report = match &result {
        Ok(backup) => ApplyResult { success: true, message: "已更新并启动；旧 exe 备份已保留".into(), backup: Some(backup.clone()) },
        Err(message) => ApplyResult { success: false, message: message.clone(), backup: None },
    };
    let report_path = directory.join(format!("result-{}.json", system::unique_suffix()));
    system::write_new(&report_path, &serde_json::to_vec_pretty(&report).map_err(err)?)?;
    result.map(|_| true)
}
fn apply_pending(directory: &Path, helper: &Path, pending: &PendingUpdate) -> Result<PathBuf> {
    apply_pending_with(directory, helper, pending, system::wait_for_process_exit,
        system::spawn_hidden, replace_with_backup)
}
// 测试注入等待/启动，真实文件替换仍可在临时目录验证；不启动任何应用或 fixture。
fn apply_pending_with(
    directory: &Path, helper: &Path, pending: &PendingUpdate,
    wait: impl FnOnce(&system::ProcessIdentity, &Path, Duration) -> Result<()>,
    mut spawn: impl FnMut(&Path, &[OsString], &Path) -> Result<u32>,
    mut replace: impl FnMut(&Path, &Path, &Path) -> Result<()>,
) -> Result<PathBuf> {
    if pending.schema != 1 || !pending.target.is_absolute() || !pending.data_dir.is_absolute()
        || !is_sha256(&pending.payload_hash) || !is_sha256(&pending.original_hash)
        || pending.parent.pid == 0 || pending.parent.created == 0 {
        return Err("无效的更新清单".into());
    }
    system::validate_restart_args(&pending.args)?;
    let directory = fs::canonicalize(directory).map_err(err)?;
    let expected_payload = directory.join("payload").join("replacement.exe");
    let payload = fs::canonicalize(&pending.payload).map_err(err)?;
    // 比较已解析路径与固定拼接路径，不能把两边都 canonicalize 后放过 reparse。
    if payload != expected_payload || has_reparse(&directory.join("payload"))? || has_reparse(&payload)? {
        return Err("更新载荷越出暂存目录或使用重解析点".into());
    }
    let target = fs::canonicalize(&pending.target).map_err(err)?;
    if system::paths_equal(&target, helper) || target.starts_with(&directory) || has_reparse(&pending.target)? {
        return Err("更新目标不能是 helper、暂存文件或重解析点".into());
    }
    if hash_file(helper)? != pending.original_hash || hash_file(&target)? != pending.original_hash {
        return Err("目标或 helper 与原始悬浮条不匹配，未更新".into());
    }
    validate_pe(&payload, &pending.arch)?;
    if hash_file(&payload)? != pending.payload_hash { return Err("暂存 exe 二次 SHA256 校验失败".into()); }
    wait(&pending.parent, &target, Duration::from_secs(120))?;
    if hash_file(&target)? != pending.original_hash { return Err("等待期间目标 exe 已变化，未更新".into()); }
    let parent = target.parent().ok_or("目标目录无效")?;
    let name = target.file_name().and_then(|n| n.to_str()).ok_or("目标名称无效")?;
    let stage = parent.join(format!(".{name}.{}.new", system::unique_suffix()));
    let backup = parent.join(format!("{name}.{}.bak", system::unique_suffix()));
    let staging = copy_new(&payload, &stage).and_then(|_| {
        if hash_file(&stage)? == pending.payload_hash { Ok(()) } else { Err("同卷暂存校验失败".into()) }
    });
    if let Err(e) = staging { let _ = fs::remove_file(&stage); return Err(e); }
    let args: Vec<OsString> = pending.args.iter().map(OsString::from).collect();
    let failure = match replace(&target, &stage, &backup) {
        Err(e) => Some(format!("替换失败：{e}")),
        Ok(()) => match hash_file(&target) {
            Ok(hash) if hash == pending.payload_hash => spawn(&target, &args, &pending.data_dir)
                .err().map(|e| format!("新版启动失败：{e}")),
            _ => Some("替换后的 exe SHA256 验证失败".into()),
        },
    };
    let _ = fs::remove_file(&stage);
    let Some(failure) = failure else { return Ok(backup); };
    match restore_original(&target, &backup, &pending.original_hash, &mut replace) {
        Ok(()) => {
            let restarted = spawn(&target, &args, &pending.data_dir);
            Err(format!("{failure}；旧 exe 已保留/恢复，旧版重启{}{}",
                if restarted.is_ok() { "成功" } else { "失败" },
                restarted.err().map(|e| format!("：{e}")).unwrap_or_default()))
        }
        Err(rollback) => Err(format!("{failure}；回滚未完成：{rollback}；备份路径：{}", backup.display())),
    }
}
fn has_reparse(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path).map_err(err)?;
    #[cfg(windows)] { use std::os::windows::fs::MetadataExt; Ok(meta.file_attributes() & 0x400 != 0) }
    #[cfg(not(windows))] { Ok(meta.file_type().is_symlink()) }
}
fn restore_original(
    target: &Path, backup: &Path, original_hash: &str,
    replace: &mut impl FnMut(&Path, &Path, &Path) -> Result<()>,
) -> Result<()> {
    if hash_file(target).is_ok_and(|hash| hash == original_hash) { return Ok(()); }
    if hash_file(backup)? != original_hash { return Err("旧 exe 备份 SHA256 不匹配，保留文件供手动恢复".into()); }
    if target.exists() {
        let failed = target.with_extension(format!("{}.failed", system::unique_suffix()));
        let result = replace(target, backup, &failed);
        if let Err(e) = result {
            if !hash_file(target).is_ok_and(|hash| hash == original_hash) { return Err(e); }
        }
    } else {
        // create_new 防止竞态覆盖；保留原始备份，以便复制失败后仍可恢复。
        copy_new(backup, target)?;
    }
    if hash_file(target)? != original_hash { return Err("回滚后 SHA256 验证失败".into()); }
    Ok(())
}
#[cfg(windows)]
fn replace_with_backup(target: &Path, replacement: &Path, backup: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};
    fn wide(path: &Path) -> Result<Vec<u16>> {
        let words: Vec<u16> = path.as_os_str().encode_wide().collect();
        if words.contains(&0) { return Err("路径包含 NUL".into()); }
        Ok(words.into_iter().chain(Some(0)).collect())
    }
    if backup.exists() { return Err("更新备份已存在，拒绝覆盖".into()); }
    let t = wide(target)?; let r = wide(replacement)?; let b = wide(backup)?;
    let mut last = String::new();
    for _ in 0..20 {
        let result = unsafe { ReplaceFileW(PCWSTR(t.as_ptr()), PCWSTR(r.as_ptr()), PCWSTR(b.as_ptr()), REPLACE_FILE_FLAGS(0), None, None) };
        match result { Ok(()) => return Ok(()), Err(e) => last = e.to_string() }
        // ReplaceFile 的罕见部分失败可能已移动旧文件，保留备份并恢复缺失目标。
        if !target.exists() && backup.is_file() {
            fs::rename(backup, target).map_err(|e| format!("替换失败，旧 exe 在 {}；恢复失败：{e}", backup.display()))?;
            return Err(format!("替换失败，已恢复旧 exe：{last}"));
        }
        if backup.exists() || !replacement.exists() { break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("无法替换悬浮条 exe（权限/文件锁）；原版及已生成备份已保留：{last}"))
}
#[cfg(not(windows))]
fn replace_with_backup(_: &Path, _: &Path, _: &Path) -> Result<()> { Err("原生自更新仅支持 Windows".into()) }

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self { let p = std::env::temp_dir().join(format!("token-bar-update-test-{}", system::unique_suffix())); fs::create_dir(&p).unwrap(); Self(p) } }
    impl Drop for Temp { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn release(tag: &str) -> Release {
        let repo = Repository::default(); let name = "zcode-token-bar-rs-windows-x86_64.exe";
        Release { tag_name: tag.into(), draft: false, prerelease: false,
            assets: vec![ReleaseAsset { name: name.into(), browser_download_url: format!("{}/releases/download/{tag}/{name}", repo.page()), size: 100,
                digest: None, state: "uploaded".into() }] }
    }
    #[test]
    fn semantic_sort_and_no_fake_update() {
        let repo = Repository::default(); let mut installed = InstalledVersion::release("1.2.0"); installed.arch = "x86_64".into();
        let releases = vec![release("v1.9.0"), release("v1.10.0"), release("v1.3.0")];
        let result = evaluate_updates(&repo, &installed, &releases, &[], None).unwrap();
        assert_eq!(result.latest_release.as_deref(), Some("v1.10.0")); assert!(result.can_update());
        let mut no_asset = release("v2.0.0"); no_asset.assets.clear();
        let result = evaluate_updates(&repo, &installed, &[no_asset], &["v99.0.0".into()], Some("f".repeat(40))).unwrap();
        assert_eq!(result.state, CheckState::NoRustAsset); assert!(!result.can_update());
        assert_eq!(parse_version("v1.2.3+build.2").unwrap(), parse_version("1.2.3+build.1").unwrap());
        assert!(parse_version("v1.2.3-rc.10").unwrap() > parse_version("v1.2.3-rc.2").unwrap());
    }
    #[test]
    fn dirty_source_cannot_offer_binary_update() {
        let repo = Repository::default(); let mut installed = InstalledVersion::release("0.1.0");
        installed.mode = InstallMode::Source { root: PathBuf::from("source"), commit: Some("a".repeat(40)), dirty: Some(true) };
        let result = evaluate_updates(&repo, &installed, &[release("v9.0.0")], &[], Some("b".repeat(40))).unwrap();
        assert_eq!(result.state, CheckState::SourceChanged); assert!(!result.can_update()); assert!(result.message.contains("未提交"));
    }
    #[test]
    fn assets_require_rust_platform_and_arch() {
        assert!(asset_score("zcode-token-bar-windows-x86_64.zip", "x86_64").is_none());
        assert!(asset_score("zcode-token-bar-rs-windows-arm64.zip", "x86_64").is_none());
        assert!(asset_score("zcode-token-bar-rust-windows-x86_64.zip", "x86_64").is_some());
        assert!(asset_score("zcode-token-bar-rs-windows-x64-setup.exe", "x86_64").is_none());
    }
    #[test]
    fn https_exact_hosts_and_release_repository() {
        for url in ["http://github.com/a", "https://github.com.evil.test/a", "https://evil.test/github.com", "https://u:p@github.com/a"] {
            assert!(!trusted_github_url(&Url::parse(url).unwrap()));
        }
        assert!(trusted_github_url(&Url::parse("https://release-assets.githubusercontent.com/a").unwrap()));
        let repo = Repository::default(); let mut release = release("v1.0.0");
        assert!(validate_asset_url(&repo, &release.tag_name, &release.assets[0]).is_ok());
        release.assets[0].browser_download_url = "https://github.com/other/project/releases/download/v1.0.0/x.exe".into();
        assert!(validate_asset_url(&repo, &release.tag_name, &release.assets[0]).is_err());
    }
    #[test]
    fn zip_paths_reject_windows_aliases_and_escape() {
        for name in ["../bad.exe", "/bad.exe", "C:/bad.exe", "a\\..\\bad.exe", "a/../bad", "a//b", "a/CON.txt", "a/file. ", "a:b", "a/COM1", "a\0b"] {
            assert!(safe_zip_path(name).is_err(), "{name}");
        }
        assert_eq!(safe_zip_path("bin/zcode-token-bar.exe").unwrap(), Path::new("bin").join("zcode-token-bar.exe"));
    }
    #[test]
    fn hashes_and_checksums_use_exact_asset() {
        let hash = format!("{:x}", Sha256::digest(b"abc"));
        assert_eq!(hash, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(parse_checksum_file(&format!("{hash} *bar.zip\n{}  unrelated.zip", "b".repeat(64)), "bar.zip").unwrap(), Some(hash.clone()));
        assert_eq!(parse_checksum_file(&format!("SHA256 (bar.zip) = {hash}"), "bar.zip").unwrap(), Some(hash.clone()));
        assert!(parse_checksum_file(&format!("{hash} bar.zip\n{} bar.zip", "b".repeat(64)), "bar.zip").is_err());
        assert!(verify_bytes(b"abc", &"0".repeat(64)).is_err());
        let temp = Temp::new(); let path = temp.0.join("bytes"); fs::write(&path, b"abc").unwrap(); assert_eq!(hash_file(&path).unwrap(), hash);
    }
    #[test]
    fn bounded_copy_never_accepts_oversize() {
        let mut target = Vec::new(); assert!(copy_hashed(&b"12345"[..], &mut target, 4).is_err()); assert!(target.is_empty());
    }
    fn fake_pe(arch: u16) -> Vec<u8> {
        let mut data = vec![0u8; 512]; data[..2].copy_from_slice(b"MZ"); data[60..64].copy_from_slice(&64u32.to_le_bytes());
        data[64..68].copy_from_slice(b"PE\0\0"); data[68..70].copy_from_slice(&arch.to_le_bytes());
        data[84..86].copy_from_slice(&240u16.to_le_bytes()); data[86..88].copy_from_slice(&2u16.to_le_bytes());
        data[88..90].copy_from_slice(&0x20bu16.to_le_bytes()); data[156..158].copy_from_slice(&2u16.to_le_bytes()); data
    }
    #[test]
    fn pe_validation_does_not_execute() {
        let temp = Temp::new(); let path = temp.0.join("sample.bin"); fs::write(&path, fake_pe(0x8664)).unwrap();
        assert!(validate_pe(&path, "x86_64").is_ok()); assert!(validate_pe(&path, "aarch64").is_err());
        fs::write(&path, b"<html>not executable</html>").unwrap(); assert!(validate_pe(&path, "x86_64").is_err());
    }
    #[test]
    fn archive_checks_every_entry_before_extracting() {
        use zip::write::SimpleFileOptions;
        let temp = Temp::new(); let archive = temp.0.join("test.zip"); let output = temp.0.join("output.exe");
        let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
        zip.start_file("bin/zcode-token-bar-rs.exe", SimpleFileOptions::default()).unwrap(); zip.write_all(&fake_pe(0x8664)).unwrap();
        zip.start_file("../escape.txt", SimpleFileOptions::default()).unwrap(); zip.write_all(b"escape").unwrap(); zip.finish().unwrap();
        assert!(extract_executable(&archive, &output, "x86_64").is_err()); assert!(!output.exists()); assert!(!temp.0.join("escape.txt").exists());
    }
    #[test]
    fn http_failures_and_missing_assets_never_claim_current() {
        for code in [403, 404, 429, 500] {
            let error = check_status(StatusCode::from_u16(code).unwrap()).unwrap_err();
            assert!(error.contains(&code.to_string())); assert!(!error.contains("已是最新"));
        }
        let mut release = release("v0.1.0"); release.assets.clear();
        let result = evaluate_updates(&Repository::default(), &InstalledVersion::release("9.0.0"), &[release], &[], None).unwrap();
        assert_eq!(result.state, CheckState::NoRustAsset); assert!(!result.can_update());
    }
    #[test]
    fn prerelease_draft_and_ambiguous_assets_are_not_offered() {
        let mut installed = InstalledVersion::release("1.0.0"); installed.arch = "x86_64".into();
        let mut draft = release("v9.0.0"); draft.draft = true;
        let mut prerelease = release("v8.0.0-rc.1"); prerelease.prerelease = true;
        let result = evaluate_updates(&Repository::default(), &installed, &[draft, prerelease, release("v1.0.0")], &[], None).unwrap();
        assert_eq!(result.state, CheckState::UpToDate);
        let mut ambiguous = release("v2.0.0"); ambiguous.assets.push(ambiguous.assets[0].clone());
        assert!(select_asset(&ambiguous, "x86_64").is_none());
    }
    #[test]
    fn digest_and_bare_checksum_must_be_unambiguous() {
        let digest = "a".repeat(64);
        assert_eq!(parse_digest(&format!("sha256:{}", digest.to_uppercase())).unwrap(), digest);
        assert!(parse_digest("sha256:short").is_err());
        assert!(parse_digest(&format!("sha512:{digest}")).is_err());
        assert_eq!(parse_checksum_file(&digest, "app.zip").unwrap(), None);
        assert_eq!(checksum_for_asset(&digest, "app.zip", true).unwrap(), Some(digest.clone()));
        assert_eq!(checksum_for_asset(&format!("{digest}\n{digest}"), "app.zip", true).unwrap(), None);
        assert!(parse_checksum_file("invalid app.zip", "app.zip").is_err());
        assert_eq!(parse_checksum_file(&format!("{digest} *another.zip"), "app.zip").unwrap(), None);
    }
    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use zip::write::SimpleFileOptions;
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            writer.start_file(*name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    #[test]
    fn zip_success_extracts_only_executable_and_rejects_duplicate_or_extra_payload() {
        let temp = Temp::new(); let archive = temp.0.join("test.zip"); let output = temp.0.join("result.bin");
        let pe = fake_pe(0x8664);
        write_zip(&archive, &[("app/zcode-token-bar-rs.exe", &pe), ("README.md", b"docs")]);
        extract_executable(&archive, &output, "x86_64").unwrap(); assert_eq!(fs::read(&output).unwrap(), pe);
        assert!(!temp.0.join("README.md").exists()); fs::remove_file(&output).unwrap();
        for entries in [
            vec![("zcode-token-bar-rs.exe", pe.as_slice()), ("ZCODE-TOKEN-BAR-RS.EXE", pe.as_slice())],
            vec![("zcode-token-bar-rs.exe", pe.as_slice()), ("runtime.dll", b"runtime".as_slice())],
            vec![("zcode-token-bar-rs.exe", pe.as_slice()), ("settings.json", b"{}".as_slice())],
        ] {
            write_zip(&archive, &entries);
            assert!(extract_executable(&archive, &output, "x86_64").is_err()); assert!(!output.exists());
        }
    }
    #[test]
    fn zip_symlink_and_corrupted_crc_are_rejected_without_output() {
        use zip::write::SimpleFileOptions;
        let temp = Temp::new(); let archive = temp.0.join("test.zip"); let output = temp.0.join("result.bin");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer.add_symlink("zcode-token-bar-rs.exe", "../../outside.exe", SimpleFileOptions::default()).unwrap();
        writer.finish().unwrap();
        assert!(extract_executable(&archive, &output, "x86_64").is_err()); assert!(!output.exists());
        write_zip(&archive, &[("zcode-token-bar-rs.exe", &fake_pe(0x8664))]);
        let offset = { let mut zip = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap(); let entry = zip.by_index(0).unwrap(); entry.data_start() as usize };
        let mut bytes = fs::read(&archive).unwrap(); bytes[offset + 400] ^= 0x80; fs::write(&archive, bytes).unwrap();
        assert!(extract_executable(&archive, &output, "x86_64").is_err()); assert!(!output.exists());
    }
    struct ApplyFixture { _temp: Temp, directory: PathBuf, helper: PathBuf, pending: PendingUpdate }
    impl ApplyFixture {
        fn new() -> Self {
            let temp = Temp::new(); let directory = temp.0.join("update"); fs::create_dir(&directory).unwrap();
            fs::create_dir(directory.join("payload")).unwrap();
            let helper = directory.join("helper.bin"); let target = temp.0.join("installed.bin");
            fs::write(&helper, b"original").unwrap(); fs::write(&target, b"original").unwrap();
            let payload = directory.join("payload/replacement.exe"); fs::write(&payload, fake_pe(0x8664)).unwrap();
            let pending = PendingUpdate { schema: 1, parent: system::ProcessIdentity { pid: 1234, created: 1 },
                target, payload_hash: hash_file(&payload).unwrap(), payload, original_hash: hash_file(&helper).unwrap(),
                arch: "x86_64".into(), args: vec!["--wait-zcode".into()], data_dir: temp.0.clone(), tag: "v1.0.0".into() };
            Self { _temp: temp, directory, helper, pending }
        }
    }
    #[test]
    fn waiting_or_digest_failure_cannot_touch_installed_file_or_start_process() {
        let fixture = ApplyFixture::new();
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, _, _| Err("等待超时".into()), |_, _, _| panic!("等待失败不得启动"), |_, _, _| panic!("等待失败不得替换"));
        assert!(result.unwrap_err().contains("等待超时"));
        fs::write(&fixture.pending.payload, fake_pe(0xaa64)).unwrap();
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, _, _| panic!("校验失败不得等待/应用"), |_, _, _| panic!("校验失败不得启动"), |_, _, _| panic!("校验失败不得替换"));
        assert!(result.is_err()); assert_eq!(fs::read(&fixture.pending.target).unwrap(), b"original");
    }
    #[test]
    fn target_mutation_while_waiting_aborts_before_replace() {
        let fixture = ApplyFixture::new();
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, target, _| { fs::write(target, b"changed-by-user").unwrap(); Ok(()) },
            |_, _, _| panic!("目标改变不得启动"), |_, _, _| panic!("目标改变不得替换"));
        assert!(result.unwrap_err().contains("等待期间"));
        assert_eq!(fs::read(&fixture.pending.target).unwrap(), b"changed-by-user");
    }
    #[cfg(windows)]
    #[test]
    fn native_file_replace_keeps_old_backup_and_waits_before_installing() {
        let fixture = ApplyFixture::new(); let waited = std::cell::Cell::new(false);
        let backup = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, target, _| { assert_eq!(fs::read(target).unwrap(), b"original"); waited.set(true); Ok(()) },
            |target, args, data| { assert!(waited.get()); assert_eq!(hash_file(target).unwrap(), fixture.pending.payload_hash);
                assert_eq!(args, [OsString::from("--wait-zcode")]); assert_eq!(data, fixture.pending.data_dir); Ok(1) },
            replace_with_backup).unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"original");
        assert_eq!(hash_file(&fixture.pending.target).unwrap(), fixture.pending.payload_hash);
    }
    #[cfg(windows)]
    #[test]
    fn native_file_replace_rolls_back_when_new_launch_fails() {
        let fixture = ApplyFixture::new(); let mut launches = 0;
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, _, _| Ok(()), |target, _, _| {
                launches += 1;
                if launches == 1 { assert_eq!(hash_file(target).unwrap(), fixture.pending.payload_hash); Err("模拟启动失败".into()) }
                else { assert_eq!(fs::read(target).unwrap(), b"original"); Ok(2) }
            }, replace_with_backup);
        assert!(result.unwrap_err().contains("旧版重启成功")); assert_eq!(launches, 2);
        assert_eq!(fs::read(&fixture.pending.target).unwrap(), b"original");
    }
    #[test]
    fn partial_replace_failure_restores_missing_target_from_verified_backup() {
        let fixture = ApplyFixture::new(); let mut restored_launches = 0;
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, _, _| Ok(()), |target, _, _| { restored_launches += 1; assert_eq!(fs::read(target).unwrap(), b"original"); Ok(3) },
            |target, _, backup| { fs::rename(target, backup).unwrap(); Err("模拟移动原件后的失败".into()) });
        assert!(result.unwrap_err().contains("旧版重启成功")); assert_eq!(restored_launches, 1);
        assert_eq!(fs::read(&fixture.pending.target).unwrap(), b"original");
    }
    #[test]
    fn rollback_failure_preserves_original_backup_and_reports_failure() {
        let fixture = ApplyFixture::new(); let mut calls = 0; let backup_path = std::cell::RefCell::new(None);
        let result = apply_pending_with(&fixture.directory, &fixture.helper, &fixture.pending,
            |_, _, _| Ok(()), |_, _, _| Err("模拟启动失败".into()),
            |target, staged, backup| {
                calls += 1;
                if calls == 1 { fs::rename(target, backup).unwrap(); fs::rename(staged, target).unwrap(); *backup_path.borrow_mut() = Some(backup.to_path_buf()); Ok(()) }
                else { Err("模拟回滚被锁".into()) }
            });
        assert!(result.unwrap_err().contains("回滚未完成")); assert_eq!(calls, 2);
        assert_eq!(fs::read(backup_path.borrow().as_ref().unwrap()).unwrap(), b"original");
    }
}
