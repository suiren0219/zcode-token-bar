//! 模型页控制器。主程序持有窗口强引用并转发回调；本模块不绑定 SettingsBridge。
//! 配置读取、发现、探针、导入与缓存写入均在后台。只有明确点击才发网络请求。

use crate::models::{
    self, DiscoveryBatch, ImportReport, ModelsService, Operation, ProbeImportReport, Progress,
    ProviderInfo, RichModelRow, RichProbeOutcome, Verification,
};
use crate::{win, SettingsWindow};
#[path = "model_pending.rs"]
mod pending;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type RowKey = (String, String);
type Groups = Vec<(String, Vec<RichModelRow>)>;
const MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CACHE_ROWS: usize = 10_000;
const CONFIRM_WINDOW: Duration = Duration::from_secs(90);
const EMPTY_DETAILS: &str = "选择一个模型查看能力与来源。未知表示尚无可靠声明或实测证据，不等于不支持。";

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Refresh,
    Discover,
    Manual,
    Import,
    Probe,
    PendingResume,
    PendingPreview,
    PendingCancel,
}

struct ActiveJob {
    generation: u64,
    kind: JobKind,
    operation: Operation,
    thread: JoinHandle<()>,
}

enum Message {
    Progress(u64, Progress),
    Finished(u64, Result<Completed, String>),
}

enum Completed {
    Refreshed {
        service: ModelsService,
        providers: Vec<ProviderInfo>,
        cache: Vec<RichModelRow>,
        note: String,
    },
    Discovered {
        batch: DiscoveryBatch,
        requested: usize,
        cache: Vec<RichModelRow>,
        cache_note: String,
    },
    Manual {
        provider_id: String,
        input_ids: Vec<String>,
        rows: Vec<RichModelRow>,
    },
    Imported {
        report: ImportReport,
        verification: Verification,
        pending_note: String,
    },
    PendingPreview { snapshot: pending::Snapshot, groups: Groups },
    PendingNotice(String),
    Probed {
        report: ProbeImportReport,
        verification: Verification,
        pending_note: String,
        requested: usize,
        cache: Vec<RichModelRow>,
        cache_note: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmKind {
    Filtered,
    Manual,
    Probe,
    PendingProbe,
}

struct PendingWatch {
    operation: Operation,
    thread: JoinHandle<Result<pending::Inspection, String>>,
}

struct PendingProbePreview {
    snapshot: pending::Snapshot,
    groups: Groups,
}

struct Confirmation {
    kind: ConfirmKind,
    signature: Vec<RowKey>,
    since: Instant,
}

impl Confirmation {
    fn matches(&self, kind: ConfirmKind, signature: &[RowKey]) -> bool {
        self.kind == kind && self.signature == signature && self.since.elapsed() < CONFIRM_WINDOW
    }
}

struct ManualPreview {
    provider_id: String,
    input_ids: Vec<String>,
    rows: Vec<RichModelRow>,
}

#[derive(Serialize, Deserialize)]
struct CacheDocument {
    version: u32,
    rows: Vec<RichModelRow>,
}

pub struct ModelUi {
    panel: slint::Weak<SettingsWindow>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    service: Option<ModelsService>,
    // 此快照同时生成下拉框和解释索引。0 始终表示全部，不能直接索引 providers。
    providers: Vec<ProviderInfo>,
    selected_provider: Option<String>,
    rows: Vec<RichModelRow>,
    visible: Vec<usize>,
    filter_text: String,
    selected: Option<RowKey>,
    sources: BTreeMap<RowKey, String>,
    outcomes: BTreeMap<RowKey, RichProbeOutcome>,
    cache: Vec<RichModelRow>,
    cache_path: PathBuf,
    confirmation: Option<Confirmation>,
    manual_preview: Option<ManualPreview>,
    generation: u64,
    active: Option<ActiveJob>,
    completed: Option<Result<Completed, String>>,
    refresh_pending: bool,
    prior_write_note: String,
    pending_path: PathBuf,
    pending_watch: Option<PendingWatch>,
    pending_snapshot: Option<pending::Snapshot>,
    pending_probe: Option<PendingProbePreview>,
    pending_note: String,
    pending_due: Instant,
    pending_quiet: Option<Instant>,
    pending_attempted: Option<[u8; 32]>,
    pending_cancel_requested: bool,
    pending_resume_requested: bool,
    status: String,
}

impl ModelUi {
    pub fn new(panel: &SettingsWindow) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut ui = Self {
            panel: panel.as_weak(),
            tx,
            rx,
            service: None,
            providers: Vec::new(),
            selected_provider: None,
            rows: Vec::new(),
            visible: Vec::new(),
            filter_text: panel.get_filter_text().to_string(),
            selected: None,
            sources: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            cache: Vec::new(),
            cache_path: crate::data_dir().join("model-discovery-cache.json"),
            confirmation: None,
            manual_preview: None,
            generation: 0,
            active: None,
            completed: None,
            refresh_pending: false,
            prior_write_note: String::new(),
            pending_path: crate::data_dir().join("pending-import.json"),
            pending_watch: None,
            pending_snapshot: None,
            pending_probe: None,
            pending_note: String::new(),
            pending_due: Instant::now(),
            pending_quiet: None,
            pending_attempted: None,
            pending_cancel_requested: false,
            pending_resume_requested: false,
            status: String::new(),
        };
        ui.render();
        ui.refresh();
        ui
    }

    /// 面板重开时只读刷新。保留 provider ID，而不是旧下拉框索引。
    pub fn refresh(&mut self) {
        self.disarm();
        if let Some(active) = &self.active {
            if active.kind == JobKind::Refresh && !active.operation.cancel.is_cancelled() {
                return;
            }
            active.operation.cancel.cancel();
            self.generation = self.generation.wrapping_add(1);
            self.refresh_pending = true;
            self.set_status("正在停止上一任务，随后只读刷新提供方与缓存。已发出的探针请求可能已计费。".into());
            return;
        }
        self.start_refresh();
    }

    fn start_refresh(&mut self) {
        self.refresh_pending = false;
        let path = self.cache_path.clone();
        self.start(JobKind::Refresh, "正在读取已启用提供方和发现缓存（不联网）…", move |op| {
            let service = ModelsService::from_env()?;
            op.check()?;
            let providers = service.providers()?;
            let (cache, note) = load_cache(&path, &service, &providers, op);
            op.check()?;
            Ok(Completed::Refreshed { service, providers, cache, note })
        });
    }

    pub fn provider_change(&mut self, index: i32) {
        let Some(next) = provider_at(&self.providers, index) else {
            self.render();
            return;
        };
        if self.selected_provider == next {
            return;
        }
        self.invalidate();
        self.selected_provider = next;
        self.show_cached_rows();
        let suffix = if self.active.is_some() { "正在等待旧任务停止。" } else { "点“发现模型”才会联网。" };
        self.set_status(format!("已切换提供方，显示 {} 个缓存模型。{suffix}", self.rows.len()));
        self.render();
    }

    pub fn select(&mut self, index: i32) {
        self.selected = usize::try_from(index).ok().and_then(|i| self.visible.get(i))
            .and_then(|i| self.rows.get(*i)).map(row_key);
        self.render_details();
    }

    pub fn filter(&mut self, text: &str) {
        if self.filter_text != text {
            let had_confirmation = self.confirmation.is_some();
            self.filter_text = text.to_owned();
            self.disarm();
            self.render();
            if had_confirmation && self.active.is_none() {
                self.set_status(format!("筛选已变更；当前 {} / {} 个模型。导入需重新确认。", self.visible.len(), self.rows.len()));
            }
        }
    }

    pub fn action(&mut self, name: &str) -> bool {
        match name {
            "discover-models" => self.discover(),
            "cancel-models" => self.cancel(),
            "import-filtered-models" => self.import_filtered(),
            "probe-import-models" => self.probe_import(),
            "copy-model-id" => self.copy_selected(),
            "resume-pending-models" => self.resume_pending(),
            "cancel-pending-models" => self.cancel_pending(),
            _ => return false,
        }
        true
    }

    fn discover(&mut self) {
        if !self.ready() { return; }
        self.disarm();
        let ids: Vec<String> = match &self.selected_provider {
            Some(pid) => vec![pid.clone()],
            None => self.providers.iter().map(|p| p.id.clone()).collect(),
        };
        if ids.is_empty() {
            self.set_status("没有已启用提供方；请在 ZCode 配置后重新打开面板。".into());
            return;
        }
        let service = self.service.clone().unwrap();
        let mut cache = self.cache.clone();
        let path = self.cache_path.clone();
        self.start(JobKind::Discover, "正在发现模型；各提供方共享 20 秒总时限，可取消…", move |op| {
            let batch = service.discover_all(&ids, op);
            // 失败提供方的旧缓存保留；本次列表只显示本次成功结果，避免冒充新发现。
            for discovery in &batch.providers {
                replace_cached_provider(&mut cache, &discovery.provider_id, &discovery.rows);
            }
            let cache_note = if batch.providers.is_empty() { String::new() } else { save_cache(&path, &cache, op) };
            Ok(Completed::Discovered { batch, requested: ids.len(), cache, cache_note })
        });
    }

    fn import_filtered(&mut self) {
        if !self.ready() { return; }
        let groups = self.visible_groups();
        let signature = group_signature(&groups);
        if signature.is_empty() {
            self.disarm();
            self.set_status("当前筛选列表为空，没有可导入模型。".into());
            return;
        }
        if signature.len() > models::MAX_MODELS {
            self.disarm();
            self.set_status(format!("当前有 {} 个模型；单次最多导入 {} 个，请缩小筛选范围。", signature.len(), models::MAX_MODELS));
            return;
        }
        let prompt = format!("将导入当前可见的 {} 个模型（{} 个提供方），保留能力及来源，不发探针。90 秒内再次点击“导入筛选列表”确认；更改筛选或提供方会取消确认。", signature.len(), groups.len());
        if !self.confirm(ConfirmKind::Filtered, signature, prompt) { return; }
        self.start_import(groups);
    }

    fn probe_import(&mut self) {
        if !self.ready() { return; }
        let groups = self.visible_groups();
        let signature = group_signature(&groups);
        if signature.is_empty() {
            self.disarm();
            self.set_status("当前筛选列表为空，没有可实测模型。".into());
            return;
        }
        let requested = signature.len();
        let count = requested.min(models::MAX_PROBE_MODELS);
        let prompt = format!("将对当前筛选集前 {count} 个模型发起可能计费的实测（共 {requested} 个，整批上限 {}；每模型最多 5 次请求）。不自动重试；仅文本确认可用才导入，完整保留已知能力，未知项保持未知；取消或总超时不导入。90 秒内再次点击“一键实测并导入”确认。", models::MAX_PROBE_MODELS);
        if !self.confirm(ConfirmKind::Probe, signature, prompt) { return; }
        // 上限按屏幕当前顺序截取，再分组；避免提供方交错时分组重排选错前 12 项。
        let groups = group_rows(self.visible.iter().filter_map(|i| self.rows.get(*i)).take(count));
        let service = self.service.clone().unwrap();
        let mut cache = self.cache.clone();
        let path = self.cache_path.clone();
        let pending_path = self.pending_path.clone();
        self.start(JobKind::Probe, "实测中：全批最多 12 个模型、240 秒；完成后统一导入。已发出的请求可能计费，可取消…", move |op| {
            let report = service.probe_all_and_import(&groups, op)?;
            // verify 也共享取消令牌；取消后不能偷偷继续另一轮写入或收费请求。
            let verification = verify_import(&service, &report.import, op);
            let cache_note = if report.probe.cancelled || report.probe.timed_out {
                String::new()
            } else {
                for row in &report.probe.rows { upsert_cached(&mut cache, row.clone()); }
                if report.probe.rows.is_empty() { String::new() } else { save_cache(&path, &cache, op) }
            };
            let usable = group_rows(report.probe.rows.iter());
            let pending_note = pending::save_failed(&pending_path, &usable, &report.import, &verification, op);
            Ok(Completed::Probed { report, verification, pending_note, requested, cache, cache_note })
        });
    }

    /// 第一次点击只读解析/预览；第二次相同 provider 与 ID 集合才在后台写入。
    pub fn manual_import(&mut self, text: &str) {
        if !self.ready() { return; }
        let Some(pid) = self.selected_provider.clone() else {
            self.disarm();
            self.set_status("按 ID 导入前，请先选择一个具体提供方；“全部”不能作为手填目标。".into());
            return;
        };
        let input_ids = match parse_manual_ids(text) {
            Ok(ids) => ids,
            Err(error) => { self.disarm(); self.set_status(error); return; }
        };
        if let Some(preview) = &self.manual_preview {
            let groups = vec![(preview.provider_id.clone(), preview.rows.clone())];
            let signature = group_signature(&groups);
            if preview.provider_id == pid && preview.input_ids == input_ids
                && self.confirmation.as_ref().is_some_and(|c| c.matches(ConfirmKind::Manual, &signature))
            {
                self.disarm();
                self.start_import(groups);
                return;
            }
        }
        self.disarm();
        // text 是回调借用，绝不能带进线程；规范化的 ID 列表归本任务所有。
        let service = self.service.clone().unwrap();
        self.start(JobKind::Manual, "正在只读校验填写的 ID 与目标提供方；尚未写入…", move |op| {
            op.check()?;
            let rows = service.manual_rows(&pid, &input_ids.join("\n"))?;
            op.check()?;
            Ok(Completed::Manual { provider_id: pid, input_ids, rows })
        });
    }

    fn start_import(&mut self, groups: Groups) {
        let service = self.service.clone().unwrap();
        let pending_path = self.pending_path.clone();
        self.start(JobKind::Import, "正在备份并写入确认的模型，随后延迟读回校验…", move |op| {
            // 克隆原始 RichModelRow，不从 ID 重建，确保 provider 与能力不丢失。
            let report = service.import_rows(&groups, op);
            let verification = verify_import(&service, &report, op);
            let pending_note = pending::save_failed(&pending_path, &groups, &report, &verification, op);
            Ok(Completed::Imported { report, verification, pending_note })
        });
    }

    fn pending_tick(&mut self) {
        if self.pending_watch.as_ref().is_some_and(|w| w.thread.is_finished()) {
            let watch = self.pending_watch.take().unwrap();
            match watch.thread.join().unwrap_or_else(|_| Err("待办检查未完成，文件保留。".into())) {
                Ok(inspection) => {
                    let changed = self.pending_snapshot.as_ref().map(|s| s.fingerprint)
                        != inspection.snapshot.as_ref().map(|s| s.fingerprint);
                    if changed {
                        self.pending_quiet = None;
                        self.pending_attempted = None;
                        self.pending_probe = None;
                        if self.confirmation.as_ref().is_some_and(|c| c.kind == ConfirmKind::PendingProbe) { self.confirmation = None; }
                    }
                    if inspection.running == Some(false) {
                        self.pending_quiet.get_or_insert_with(Instant::now);
                    } else { self.pending_quiet = None; }
                    if inspection.snapshot.is_none() && self.pending_resume_requested {
                        self.pending_resume_requested = false;
                        self.status = "没有持久待办可继续。".into();
                    }
                    if inspection.running == Some(true) { self.pending_attempted = None; }
                    self.pending_snapshot = inspection.snapshot;
                    self.pending_note = inspection.note;
                    self.paint_status();
                }
                Err(error) => { self.pending_quiet = None; self.pending_note = error; self.paint_status(); }
            }
            self.pending_due = Instant::now() + Duration::from_secs(3);
        }
        if self.active.is_none() {
            if self.pending_cancel_requested {
                self.pending_cancel_requested = false;
                self.cancel_pending();
            } else if self.pending_resume_requested && self.pending_watch.is_none() && self.pending_snapshot.is_some() {
                self.pending_resume_requested = false;
                self.resume_pending();
            } else if self.service.is_some() && self.confirmation.is_none()
                && self.pending_quiet.is_some_and(|t| t.elapsed() >= Duration::from_secs(3))
            {
                if let Some(snapshot) = self.pending_snapshot.clone() {
                    if matches!(&snapshot.intent, Ok(pending::Intent::Import(_)))
                        && self.pending_attempted != Some(snapshot.fingerprint)
                    { self.start_pending_import(snapshot); }
                }
            }
        }
        if self.pending_watch.is_none() && Instant::now() >= self.pending_due {
            let path = self.pending_path.clone();
            let operation = Operation::new(Duration::from_secs(10));
            let op = operation.clone();
            if let Ok(thread) = std::thread::Builder::new().name("model-pending-check".into()).spawn(move || pending::inspect(&path, &op)) {
                self.pending_watch = Some(PendingWatch { operation, thread });
            } else { self.pending_due = Instant::now() + Duration::from_secs(10); }
        }
    }

    fn start_pending_import(&mut self, snapshot: pending::Snapshot) {
        if self.active.is_some() { return; }
        let Some(service) = self.service.clone() else { return; };
        self.pending_attempted = Some(snapshot.fingerprint);
        let path = self.pending_path.clone();
        self.start(JobKind::PendingResume, "客户端已退出，正在续办原先已确认的模型导入；不发送探针…", move |op| {
            let (report, verification, pending_note) = pending::resume_import(&path, &snapshot, &service, op)?;
            Ok(Completed::Imported { report, verification, pending_note })
        });
    }

    fn resume_pending(&mut self) {
        if self.active.is_some() { return; }
        let Some(snapshot) = self.pending_snapshot.clone() else {
            self.pending_resume_requested = true;
            self.pending_due = Instant::now();
            self.set_status("正在重新读取模型待办；不发探针、不写配置。".into());
            return;
        };
        let Some(service) = self.service.clone() else {
            self.set_status("当前配置未加载，待办保留；请重新打开面板后继续。".into());
            return;
        };
        match snapshot.intent.clone() {
            Err(error) => self.set_status(error),
            Ok(pending::Intent::Import(_)) => {
                self.disarm();
                self.start_pending_import(snapshot);
            }
            Ok(pending::Intent::Probe { .. }) => {
                if let Some(preview) = &self.pending_probe {
                    let signature = group_signature(&preview.groups);
                    if preview.snapshot.fingerprint == snapshot.fingerprint
                        && self.confirmation.as_ref().is_some_and(|c| c.matches(ConfirmKind::PendingProbe, &signature))
                    {
                        let groups = preview.groups.clone();
                        self.disarm();
                        let path = self.pending_path.clone();
                        self.start(JobKind::Probe, "正在执行再次确认的旧探针待办；可能计费，不自动重试…", move |op| {
                            if !pending::matches(&path, &snapshot)? { return Err("待办已变化，请重新预览确认。".into()); }
                            let groups = pending::validate_groups(&service, &groups, op)?;
                            let report = service.probe_all_and_import(&groups, op)?;
                            let verification = verify_import(&service, &report.import, op);
                            let note = pending::finish_legacy_probe(&path, &snapshot, &report.probe.rows, &report.import, &verification, op);
                            Ok(Completed::PendingNotice(format!("已实测 {} 个模型。{}{}", report.probe.probed, import_summary(&report.import, &verification), note)))
                        });
                        return;
                    }
                }
                self.disarm();
                let path = self.pending_path.clone();
                self.start(JobKind::PendingPreview, "正在读取旧探针待办模型；若原任务为自动发现，仅请求模型清单，不发收费探针…", move |op| {
                    if !pending::matches(&path, &snapshot)? { return Err("待办已变化，请重新读取。".into()); }
                    let groups = pending::prepare_probe(&service, &snapshot, op)?;
                    Ok(Completed::PendingPreview { snapshot, groups })
                });
            }
        }
    }

    fn cancel_pending(&mut self) {
        self.disarm();
        self.pending_resume_requested = false;
        if let Some(active) = &self.active {
            active.operation.cancel.cancel();
            self.pending_cancel_requested = true;
            self.set_status("正在停止后台任务；停止后先备份已读取的待办，再校验并移除，不撤销已提交的配置。".into());
            return;
        }
        let expected = self.pending_snapshot.clone();
        let path = self.pending_path.clone();
        self.start(JobKind::PendingCancel, "正在备份并校验待办快照，随后取消持久待办…", move |op| {
            let snapshot = match expected { Some(s) => Some(s), None => pending::read(&path)? };
            let Some(snapshot) = snapshot else { return Ok(Completed::PendingNotice("没有持久待办可取消。".into())); };
            pending::retire(&path, &snapshot, op)?;
            Ok(Completed::PendingNotice("已备份并取消本次持久待办；外部新待办不会被删除。".into()))
        });
    }

    fn copy_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            self.set_status("请先选择一个模型。".into());
            return;
        };
        let result = win::copy_text(&row.row.id);
        if self.active.is_none() {
            self.disarm();
            self.set_status(if result.is_ok() { "已复制选中模型的 ID。" } else { "复制失败，剪贴板可能正被占用，请重试。" }.into());
        }
    }

    /// 请求取消但继续占用任务槽，直到后台线程真正退出，不能提前开启新收费任务。
    pub fn cancel(&mut self) {
        self.disarm();
        self.refresh_pending = false;
        if let Some(active) = &self.active {
            active.operation.cancel.cancel();
            let message = if active.kind == JobKind::Probe {
                "正在取消实测；已发出的请求可能计费，等待后台停止后才可再操作。已完成的配置提交不能由取消撤销。"
            } else {
                "正在取消任务，等待后台停止；若配置已提交，将如实显示写入及读回结果。"
            };
            self.set_status(message.into());
        } else {
            self.set_status("当前没有后台任务；待确认的导入已取消。".into());
        }
    }

    /// 主程序 UI 定时器调用。所有 Slint 属性修改只发生在 UI 线程。
    pub fn tick(&mut self) {
        self.expire_confirmation();
        self.pending_tick();
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Progress(generation, progress) => {
                    if self.active.as_ref().is_some_and(|a| a.generation == generation
                        && generation == self.generation && !a.operation.cancel.is_cancelled())
                    {
                        self.set_status(progress_text(&progress));
                    }
                }
                Message::Finished(generation, result) => {
                    if self.active.as_ref().is_some_and(|a| a.generation == generation) {
                        self.completed = Some(result);
                    }
                }
            }
        }
        // Finished 消息可能比线程析构更早到；is_finished 后 join 才不会阻塞 UI。
        if !self.active.as_ref().is_some_and(|a| a.thread.is_finished()) { return; }
        let active = self.active.take().unwrap();
        let join_ok = active.thread.join().is_ok();
        // 线程可能刚在上述 try_recv 后投递最后一条消息，需再收一次。
        while let Ok(message) = self.rx.try_recv() {
            if let Message::Finished(generation, result) = message {
                if generation == active.generation { self.completed = Some(result); }
            }
        }
        let result = self.completed.take().unwrap_or_else(|| Err("后台任务意外结束；没有自动重试。请检查结果后再手动操作。".into()));
        if active.generation == self.generation {
            if !join_ok {
                self.set_status("后台任务意外结束；没有自动重试，配置是否写入请以读回检查为准。".into());
            } else {
                self.finish(active.kind, result, active.operation.cancel.is_cancelled());
            }
        } else {
            // 旧任务绝不回填模型/选择；已经完成的写入属于重要事实，单独保留提示。
            let write_note = previous_write_note(&result);
            if !write_note.is_empty() {
                self.prior_write_note = write_note.clone();
                self.set_status(write_note);
            } else if !self.refresh_pending {
                self.set_status(format!("上一任务已停止；当前提供方显示 {} 个已知模型，可开始新操作。", self.rows.len()));
            }
        }
        self.pending_due = Instant::now();
        self.render();
        if self.refresh_pending { self.start_refresh(); }
    }

    fn start(&mut self, kind: JobKind, message: &str,
        work: impl FnOnce(&Operation) -> Result<Completed, String> + Send + 'static)
    {
        if self.active.is_some() { return; }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let progress_tx = self.tx.clone();
        let operation = match kind {
            JobKind::Discover => Operation::discovery(),
            JobKind::Probe => Operation::probe(),
            _ => Operation::new(Duration::from_secs(30)),
        }.with_progress(move |p| { let _ = progress_tx.send(Message::Progress(generation, p)); });
        let thread_op = operation.clone();
        let tx = self.tx.clone();
        let thread = std::thread::Builder::new().name("model-page-worker".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&thread_op)))
                .unwrap_or_else(|_| Err("后台任务异常，已停止且没有自动重试；请确认配置状态后重试。".into()));
            let _ = tx.send(Message::Finished(generation, result));
        });
        match thread {
            Ok(thread) => {
                self.completed = None;
                self.active = Some(ActiveJob { generation, kind, operation, thread });
                self.set_status(message.into());
            }
            Err(_) => self.set_status("无法创建后台任务，尚未发出请求或写入配置。".into()),
        }
        self.render();
    }

    fn finish(&mut self, kind: JobKind, result: Result<Completed, String>, cancelled: bool) {
        let completed = match result {
            Ok(completed) => completed,
            Err(error) => {
                if kind == JobKind::Refresh {
                    self.service = None;
                    self.providers.clear();
                    self.selected_provider = None;
                    self.cache.clear();
                    self.show_cached_rows();
                }
                let write_note = if kind == JobKind::Refresh { std::mem::take(&mut self.prior_write_note) } else { String::new() };
                self.set_status(format!("{}：{}{}", if cancelled { "任务已停止" } else { "操作未完成" }, safe_message(&error), write_note));
                return;
            }
        };
        match completed {
            Completed::Refreshed { service, providers, cache, note } => {
                if cancelled { self.set_status("只读刷新已取消。".into()); return; }
                self.selected_provider = retain_provider(self.selected_provider.as_deref(), &providers);
                self.service = Some(service);
                self.providers = providers;
                self.cache = cache;
                self.show_cached_rows();
                let write_note = std::mem::take(&mut self.prior_write_note);
                self.set_status(format!("已读取 {} 个启用提供方、{} 个缓存模型，本次刷新未联网。点击“发现模型”获取清单。{}{}", self.providers.len(), self.rows.len(), note, write_note));
            }
            Completed::Discovered { batch, requested, cache, cache_note } => {
                self.disarm();
                self.rows.clear();
                self.sources.clear();
                self.outcomes.clear();
                self.selected = None;
                for discovery in &batch.providers {
                    if !self.providers.iter().any(|p| p.id == discovery.provider_id) { continue; }
                    let note = if discovery.truncated {
                        format!("{}（清单已达分页或数量上限）", safe_message(&discovery.source_note))
                    } else { safe_message(&discovery.source_note) };
                    for row in &discovery.rows {
                        if row.row.provider_id == discovery.provider_id {
                            self.sources.insert(row_key(row), note.clone());
                            self.rows.push(row.clone());
                        }
                    }
                }
                dedup_rows(&mut self.rows);
                self.cache = cache;
                self.set_status(discovery_summary(&batch, requested, self.rows.len(), &cache_note));
            }
            Completed::Manual { provider_id, input_ids, rows } => {
                if cancelled || self.selected_provider.as_deref() != Some(provider_id.as_str()) {
                    self.set_status("手填校验已取消；没有写入配置。".into());
                    return;
                }
                let current_input = self.panel.upgrade().and_then(|w| parse_manual_ids(w.get_manual_model_ids().as_str()).ok());
                if current_input.as_ref() != Some(&input_ids) {
                    self.set_status("填写的 ID 已变化；本次只读预览已丢弃，请再次点击校验。".into());
                    return;
                }
                self.rows = rows.clone();
                self.sources = rows.iter().map(|r| (row_key(r), "手填 ID；能力来自当前配置/规则，尚未向 API 验证可用性。".into())).collect();
                self.outcomes.clear();
                self.selected = None;
                self.filter_text.clear();
                let count = rows.len();
                let skipped = input_ids.len().saturating_sub(count);
                let signature = group_signature(&vec![(provider_id.clone(), rows.clone())]);
                self.confirmation = Some(Confirmation { kind: ConfirmKind::Manual, signature, since: Instant::now() });
                self.manual_preview = Some(ManualPreview { provider_id, input_ids, rows });
                self.set_status(format!("已只读校验并显示 {count} 个模型；已忽略 {skipped} 个无效或敏感 ID。尚未写入。90 秒内再次点击“导入填写的 ID”确认；目标为当前所选提供方。"));
            }
            Completed::Imported { report, verification, pending_note } => {
                self.disarm();
                self.set_status(format!("{}{}", import_summary(&report, &verification), pending_note));
            }
            Completed::PendingNotice(note) => {
                self.disarm();
                self.set_status(note);
            }
            Completed::PendingPreview { snapshot, groups } => {
                if cancelled { return; }
                let signature = group_signature(&groups);
                let count = signature.len();
                self.rows = groups.iter().flat_map(|(_, rows)| rows.iter().cloned()).collect();
                self.selected_provider = None;
                self.filter_text.clear();
                self.selected = None;
                self.outcomes.clear();
                self.sources = self.rows.iter().map(|r| (row_key(r), "旧探针待办的显式预览；尚未重发实测。".into())).collect();
                self.confirmation = Some(Confirmation { kind: ConfirmKind::PendingProbe, signature, since: Instant::now() });
                self.pending_probe = Some(PendingProbePreview { snapshot, groups });
                self.set_status(format!("已预览旧待办的 {count} 个模型。再次点击“继续待办”才会发出可能计费的实测（每模型最多5次请求）；仅本批确认可用模型参与导入。更改筛选或等待90秒会取消确认。"));
            }
            Completed::Probed { report, verification, pending_note, requested, cache, cache_note } => {
                self.disarm();
                // 不用可用子集替换整张列表；失败和未知的行仍可选择查看实测原因。
                for outcome in &report.probe.outcomes {
                    let key = (outcome.provider_id.clone(), outcome.model_id.clone());
                    if let Some(row) = self.rows.iter_mut().find(|r| row_key(r) == key) { outcome.apply(row); }
                    self.outcomes.insert(key, outcome.clone());
                }
                self.cache = cache;
                let eligible = report.probe.outcomes.iter().filter(|o| probe_eligible(o)).count();
                let requests: usize = report.probe.outcomes.iter().map(|o| o.requests).sum();
                let mut text = format!("实测完成 {}/{} 个（筛选集 {requested} 个），文本确认可用 {eligible} 个，共发出 {requests} 次请求。", report.probe.probed, report.probe.total);
                if requested > models::MAX_PROBE_MODELS || report.probe.truncated {
                    text.push_str(&format!("整批上限 {} 个，其余未请求。", models::MAX_PROBE_MODELS));
                }
                if report.probe.cancelled { text.push_str("实测已取消。 "); }
                if report.probe.timed_out { text.push_str("实测总时限已到。 "); }
                text.push_str(&import_summary(&report.import, &verification));
                text.push_str("失败/未知不会自动重试；选择模型可看各项实测结果。 ");
                text.push_str(&cache_note);
                text.push_str(&pending_note);
                self.set_status(text);
            }
        }
    }

    fn ready(&mut self) -> bool {
        if self.active.is_some() {
            // 不覆盖进度/取消文案，也不丢掉原任务的 cancel token。
            return false;
        }
        if self.service.is_none() {
            self.set_status("尚未成功读取配置；请重新打开面板刷新后再操作。".into());
            return false;
        }
        true
    }

    fn invalidate(&mut self) {
        self.disarm();
        self.generation = self.generation.wrapping_add(1);
        if let Some(active) = &self.active { active.operation.cancel.cancel(); }
    }

    fn confirm(&mut self, kind: ConfirmKind, signature: Vec<RowKey>, prompt: String) -> bool {
        let matched = self.confirmation.as_ref().is_some_and(|c| c.matches(kind, &signature));
        self.disarm();
        if matched { return true; }
        self.confirmation = Some(Confirmation { kind, signature, since: Instant::now() });
        self.set_status(prompt);
        false
    }

    fn disarm(&mut self) {
        self.confirmation = None;
        self.manual_preview = None;
        self.pending_probe = None;
    }

    fn expire_confirmation(&mut self) {
        let expired = self.confirmation.as_ref().is_some_and(|c| c.since.elapsed() >= CONFIRM_WINDOW);
        let changed = self.manual_preview.as_ref().is_some_and(|p| {
            self.panel.upgrade().is_some_and(|w| parse_manual_ids(w.get_manual_model_ids().as_str()).ok().as_ref() != Some(&p.input_ids))
        });
        if expired || changed {
            self.disarm();
            if self.active.is_none() {
                self.set_status(if expired { "导入确认已过期；请重新点击确认。" } else { "填写的 ID 已变更；请重新点击校验并确认。" }.into());
            }
        }
    }

    fn visible_groups(&self) -> Groups {
        group_rows(self.visible.iter().filter_map(|i| self.rows.get(*i)).filter(|r| {
            self.providers.iter().any(|p| p.id == r.row.provider_id)
                && self.selected_provider.as_ref().is_none_or(|pid| pid == &r.row.provider_id)
        }))
    }

    fn show_cached_rows(&mut self) {
        self.rows = self.cache.iter().filter(|row| {
            self.providers.iter().any(|p| p.id == row.row.provider_id)
                && self.selected_provider.as_ref().is_none_or(|id| id == &row.row.provider_id)
        }).cloned().collect();
        self.sources = self.rows.iter().map(|r| (row_key(r), "上次发现缓存；尚未重新请求 API，声明或实测可能已变化。".into())).collect();
        self.outcomes.clear();
        self.selected = None;
        self.disarm();
    }

    fn set_status(&mut self, status: String) {
        self.status = status;
        self.paint_status();
    }

    fn paint_status(&self) {
        if let Some(panel) = self.panel.upgrade() {
            let text = if self.pending_note.is_empty() { self.status.clone() } else { format!("{}\n{}", self.status, self.pending_note) };
            panel.set_models_status(text.into());
        }
    }

    fn render(&mut self) {
        self.visible = filtered_indices(&self.rows, &self.filter_text);
        if self.selected.as_ref().is_some_and(|key| !self.visible.iter().any(|i| row_key(&self.rows[*i]) == *key)) {
            self.selected = None;
        }
        if let Some(panel) = self.panel.upgrade() {
            let names: Vec<SharedString> = std::iter::once(SharedString::from("全部已启用提供方"))
                .chain(self.providers.iter().map(|p| format!("{} · {} [{}]", p.name, p.kind, p.id).into())).collect();
            panel.set_provider_names(ModelRc::from(Rc::new(VecModel::from(names))));
            panel.set_provider_index(provider_index(self.selected_provider.as_deref(), &self.providers));
            let items: Vec<SharedString> = self.visible.iter().map(|i| {
                let r = &self.rows[*i];
                if self.selected_provider.is_none() { format!("{} · {} [{}]", r.row.id, r.row.provider, r.row.provider_id).into() }
                else { r.row.id.as_str().into() }
            }).collect();
            panel.set_model_items(ModelRc::from(Rc::new(VecModel::from(items))));
            // Slint 已显示 model-items.length / model-count，分母是筛选前总量。
            panel.set_model_count(self.rows.len().min(i32::MAX as usize) as i32);
            panel.set_filter_text(self.filter_text.as_str().into());
            panel.set_models_busy(self.active.is_some());
        }
        self.paint_status();
        self.render_details();
    }

    fn selected_row(&self) -> Option<&RichModelRow> {
        let key = self.selected.as_ref()?;
        self.visible.iter().filter_map(|i| self.rows.get(*i)).find(|r| row_key(r) == *key)
    }

    fn render_details(&self) {
        let Some(panel) = self.panel.upgrade() else { return; };
        let index = self.selected.as_ref().and_then(|key| self.visible.iter().position(|i| row_key(&self.rows[*i]) == *key));
        panel.set_model_selected_index(index.map(|i| i as i32).unwrap_or(-1));
        let Some(row) = self.selected_row() else { panel.set_model_details(EMPTY_DETAILS.into()); return; };
        let key = row_key(row);
        let api = self.providers.iter().find(|p| p.id == row.row.provider_id).map(|p| p.kind.as_str()).unwrap_or("未知");
        let source = self.sources.get(&key).map(String::as_str).unwrap_or("已知模型声明");
        let mut text = format!("模型 ID：{}\n提供方：{} [{}]\nAPI 类型：{}\n清单来源：{}\n\n{}\n\n未知不等于不支持；API/配置/规则为声明，只有标注实测的字段经过探针。", row.row.id, row.row.provider, row.row.provider_id, api, source, models::capability_lines(&row.capabilities).join("\n"));
        if let Some(outcome) = self.outcomes.get(&key) {
            text.push_str(&format!("\n\n本轮实测（{} 次请求）\n文本可用：{}\n工具调用：{}\n图片输入：{}\nPDF 输入：{}\n{}", outcome.requests, flag_text(outcome.usable), flag_text(outcome.tools), flag_text(outcome.image), flag_text(outcome.pdf), if probe_eligible(outcome) { "文本已确认可用，保留所有已知能力，未知项不伪造为不支持；是否成功写入见操作状态。" } else { "文本未确认可用或任务已中止，本轮不会自动导入。" }));
            if !outcome.error.is_empty() { text.push_str(&format!("\n{}", safe_message(&outcome.error))); }
            for (field, error) in &outcome.errors {
                text.push_str(&format!("\n{}：{}", stage_name(field), safe_message(error)));
            }
        }
        panel.set_model_details(text.into());
    }
}

impl Drop for ModelUi {
    fn drop(&mut self) {
        if let Some(active) = &self.active { active.operation.cancel.cancel(); }
        if let Some(watch) = &self.pending_watch { watch.operation.cancel.cancel(); }
        // 不在 UI 析构中 join 网络线程；后台不持有任何窗口句柄。
    }
}

fn provider_at(providers: &[ProviderInfo], index: i32) -> Option<Option<String>> {
    if index == 0 { Some(None) }
    else { index.checked_sub(1).and_then(|i| usize::try_from(i).ok()).and_then(|i| providers.get(i)).map(|p| Some(p.id.clone())) }
}

fn retain_provider(previous: Option<&str>, providers: &[ProviderInfo]) -> Option<String> {
    previous.filter(|id| providers.iter().any(|p| p.id == *id)).map(str::to_owned)
}

fn provider_index(selected: Option<&str>, providers: &[ProviderInfo]) -> i32 {
    selected.and_then(|id| providers.iter().position(|p| p.id == id)).map(|i| i as i32 + 1).unwrap_or(0)
}

fn row_key(row: &RichModelRow) -> RowKey { (row.row.provider_id.clone(), row.row.id.clone()) }

fn filtered_indices(rows: &[RichModelRow], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    rows.iter().enumerate().filter(|(_, row)| needle.is_empty()
        || row.row.id.to_lowercase().contains(&needle)
        || row.row.provider.to_lowercase().contains(&needle)
        || row.row.provider_id.to_lowercase().contains(&needle))
        .map(|(i, _)| i).collect()
}

fn group_rows<'a>(rows: impl Iterator<Item = &'a RichModelRow>) -> Groups {
    let mut groups: Groups = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        if !seen.insert(row_key(row)) { continue; }
        match groups.iter_mut().find(|(pid, _)| pid == &row.row.provider_id) {
            Some((_, rows)) => rows.push(row.clone()),
            None => groups.push((row.row.provider_id.clone(), vec![row.clone()])),
        }
    }
    groups
}

fn group_signature(groups: &Groups) -> Vec<RowKey> {
    let mut keys: Vec<_> = groups.iter().flat_map(|(pid, rows)| rows.iter().map(move |r| (pid.clone(), r.row.id.clone()))).collect();
    keys.sort();
    keys.dedup();
    keys
}

fn parse_manual_ids(input: &str) -> Result<Vec<String>, String> {
    if input.len() > 64 * 1024 { return Err("填写内容过长，请分批导入。".into()); }
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for id in input.split(|c: char| c.is_whitespace() || ",，;；、".contains(c)).filter(|s| !s.is_empty()) {
        if !valid_identifier(id) { return Err("模型 ID 含无效字符或超过 256 字符，请修正后重试。".into()); }
        if seen.insert(id) { ids.push(id.to_owned()); }
        if ids.len() > models::MAX_MODELS { return Err(format!("单次最多填写 {} 个不同模型 ID，请分批导入。", models::MAX_MODELS)); }
    }
    if ids.is_empty() { return Err("请填写至少一个模型 ID。".into()); }
    ids.sort();
    Ok(ids)
}

fn valid_identifier(id: &str) -> bool {
    !id.is_empty() && id.trim() == id && id.chars().count() <= 256
        && !id.chars().any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200b}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

fn dedup_rows(rows: &mut Vec<RichModelRow>) {
    let mut seen = HashSet::new();
    rows.retain(|r| valid_identifier(&r.row.provider_id) && valid_identifier(&r.row.id) && seen.insert(row_key(r)));
}

fn probe_eligible(outcome: &RichProbeOutcome) -> bool {
    outcome.usable == Some(true) && !outcome.cancelled && !outcome.timed_out
}

fn flag_text(flag: Option<bool>) -> &'static str {
    match flag { Some(true) => "支持", Some(false) => "不支持", None => "未知（未取得可靠结果）" }
}

fn stage_name(stage: &str) -> &'static str {
    match stage {
        "discovery" => "发现清单", "provider" => "提供方", "model" => "模型进度",
        "text" => "文本", "tools" => "工具调用", "tool_result" => "工具结果回传",
        "image" => "图片", "pdf" => "PDF", "probe" => "能力探针", "import" => "配置写入",
        _ => "处理进度",
    }
}

fn progress_text(p: &Progress) -> String {
    let location = match (p.provider_id.is_empty(), p.model_id.is_empty()) {
        (true, true) => String::new(),
        (_, true) => safe_message(&p.provider_id),
        _ => format!("{} / {}", safe_message(&p.provider_id), safe_message(&p.model_id)),
    };
    format!("{}：{} / {}  {}", stage_name(&p.stage), p.completed, p.total, location)
}

fn safe_message(text: &str) -> String {
    // 服务承诺只返回固定错误和脱敏标识。额外阻止常见认证串进入状态/详情。
    let lower = text.to_ascii_lowercase();
    if ["authorization:", "authorization=", "bearer ", "api_key=", "apikey=", "access_token=", "sk-"].iter().any(|p| lower.contains(p)) {
        return "信息含认证样式内容，已隐藏。".into();
    }
    text.chars().filter(|c| (!c.is_control() || *c == '\n') && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).take(1200).collect()
}

fn discovery_summary(batch: &DiscoveryBatch, requested: usize, count: usize, cache_note: &str) -> String {
    let attempted = batch.providers.len() + batch.errors.len();
    let mut text = format!("发现 {count} 个模型：{} 个提供方成功，{} 个失败，{} 个未处理。", batch.providers.len(), batch.errors.len(), requested.saturating_sub(attempted));
    if batch.cancelled { text.push_str("已取消，保留本次已取得的结果。 "); }
    if batch.timed_out { text.push_str("已到整批总时限，保留部分结果。 "); }
    if batch.providers.iter().any(|d| d.truncated) { text.push_str("部分清单达到分页/数量上限。 "); }
    for (pid, error) in batch.errors.iter().take(8) { text.push_str(&format!("\n{}：{}", safe_message(pid), safe_message(error))); }
    if batch.errors.len() > 8 { text.push_str(&format!("\n另有 {} 个提供方失败；可选择具体提供方重新发现。", batch.errors.len() - 8)); }
    if !cache_note.is_empty() { text.push('\n'); text.push_str(cache_note); }
    text
}

fn verify_import(service: &ModelsService, report: &ImportReport, operation: &Operation) -> Verification {
    // 单独给延迟读回留时间，但必须共用取消令牌；这一步从不写配置或联网。
    let mut verify = Operation::new(Duration::from_secs(10));
    verify.cancel = operation.cancel.clone();
    service.verify(report, &verify)
}

fn previous_write_note(result: &Result<Completed, String>) -> String {
    let (report, verification) = match result {
        Ok(Completed::Imported { report, verification, .. }) => (report, verification),
        Ok(Completed::Probed { report, verification, .. }) => (&report.import, verification),
        _ => return String::new(),
    };
    if report.result.added == 0 { return String::new(); }
    format!("\n上一任务在停止前已提交配置：{}", import_summary(report, verification))
}

fn import_summary(report: &ImportReport, verification: &Verification) -> String {
    let mut text = if report.result.added == 0 {
        "没有新增写入。".to_owned()
    } else if verification.retained && verification.missing.is_empty() && verification.error.is_empty() && !verification.cancelled {
        format!("已写入并读回确认 {} 个模型。尚未取得 ZCode 运行态接受确认。", report.result.added)
    } else {
        format!("已写入 {} 个模型，但未完成读回确认；不代表客户端已接受。", report.result.added)
    };
    if !report.result.skipped.is_empty() { text.push_str(&format!("已存在跳过 {} 个。", report.result.skipped.len())); }
    if !report.result.missing.is_empty() { text.push_str(&format!("{} 个目标提供方缺失/停用。", report.result.missing.len())); }
    if report.partial { text.push_str("仅部分文件完成，请勿把本次视为全部成功。 "); }
    if !report.result.error.is_empty() { text.push_str(&format!("\n{}", safe_message(&report.result.error))); }
    if !verification.error.is_empty() { text.push_str(&format!("\n读回：{}", safe_message(&verification.error))); }
    if !verification.missing.is_empty() { text.push_str(&format!("读回缺失 {} 项。", verification.missing.len())); }
    let backups = report.files.iter().filter(|f| !f.backup.is_empty()).count();
    if backups > 0 { text.push_str(&format!("已保留 {backups} 份配置备份。")); }
    text
}

fn replace_cached_provider(cache: &mut Vec<RichModelRow>, pid: &str, rows: &[RichModelRow]) {
    cache.retain(|r| r.row.provider_id != pid);
    cache.extend(rows.iter().filter(|r| r.row.provider_id == pid).cloned());
    dedup_rows(cache);
    cache.truncate(MAX_CACHE_ROWS);
}

fn upsert_cached(cache: &mut Vec<RichModelRow>, row: RichModelRow) {
    let key = row_key(&row);
    if let Some(existing) = cache.iter_mut().find(|r| row_key(r) == key) { *existing = row; }
    else if cache.len() < MAX_CACHE_ROWS { cache.push(row); }
}

fn sanitize_caps(row: &mut RichModelRow) {
    let caps = &mut row.capabilities;
    caps.context_window = caps.context_window.filter(|n| *n > 0 && *n < 100_000_000);
    caps.output_limit = caps.output_limit.filter(|n| *n > 0 && *n < 100_000_000);
    caps.sources.retain(|key, value| {
        let known_key = matches!(key.as_str(), "tools" | "reasoning" | "structured_output" | "web_search" | "context_window" | "output_limit")
            || key.strip_prefix("input.").or_else(|| key.strip_prefix("output.")).is_some_and(|m| models::MODALITIES.contains(&m));
        known_key && matches!(value.as_str(), "provider API" | "ZCode config" | "ZCode rules" | "实测探针")
    });
}

fn decode_cache(bytes: &[u8]) -> Result<Vec<RichModelRow>, String> {
    use serde_json::Value;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "发现缓存损坏，已忽略；可重新发现。".to_owned())?;
    if value.get("version").is_some_and(|v| v.as_u64() != Some(1)) {
        return Err("发现缓存版本不兼容，已忽略。".into());
    }
    let mut rows = Vec::new();
    let mut add = |pid: &str, items: &Value| -> Result<(), String> {
        if let Some(items) = items.as_array() {
            for item in items {
                if let Ok(row) = pending::normalize_row(pid, None, item) { rows.push(row); }
            }
        } else if let Some(items) = items.as_object() {
            for (id, item) in items {
                if let Ok(row) = pending::normalize_row(pid, Some(id), item) { rows.push(row); }
            }
        } else { return Err("发现缓存模型清单无效。".into()); }
        if rows.len() > MAX_CACHE_ROWS { return Err("发现缓存条目过多，已忽略。".into()); }
        Ok(())
    };
    if let Some(items) = value["rows"].as_array() {
        for item in items {
            let pid = item["provider_id"].as_str().unwrap_or("");
            add(pid, &Value::Array(vec![item.clone()]))?;
        }
    } else if let Some(providers) = value["providers"].as_array() {
        for provider in providers {
            let pid = provider["provider_id"].as_str().or_else(|| provider["id"].as_str()).unwrap_or("");
            add(pid, provider.get("models").or_else(|| provider.get("rows")).unwrap_or(&Value::Null))?;
        }
    } else if let Some(providers) = value["providers"].as_object() {
        for (pid, provider) in providers {
            add(pid, provider.get("models").or_else(|| provider.get("rows")).unwrap_or(provider))?;
        }
    } else if let Some(groups) = value.as_object() {
        for (pid, items) in groups { add(pid, items)?; }
    } else { return Err("发现缓存格式无效，已忽略。".into()); }
    Ok(rows)
}

fn load_cache(path: &Path, service: &ModelsService, providers: &[ProviderInfo], op: &Operation) -> (Vec<RichModelRow>, String) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), String::new()),
        Err(_) => return (Vec::new(), "发现缓存暂不可读。".into()),
    };
    let mut bytes = Vec::new();
    if file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_CACHE_BYTES {
        return (Vec::new(), "发现缓存读取失败或超过 8 MiB，已忽略。".into());
    }
    let mut cached = match decode_cache(&bytes) {
        Ok(rows) => rows,
        Err(note) => return (Vec::new(), note),
    };
    dedup_rows(&mut cached);
    let mut valid = Vec::new();
    for provider in providers {
        if op.check().is_err() { break; }
        let rows: Vec<_> = cached.iter().filter(|r| r.row.provider_id == provider.id).take(models::MAX_MODELS).collect();
        if rows.is_empty() { continue; }
        // 再用服务的当前凭据脱敏规则校验缓存 ID；不信任旧缓存里的名称/任意来源字符串。
        let ids = rows.iter().map(|r| r.row.id.as_str()).collect::<Vec<_>>().join("\n");
        let Ok(checked) = service.manual_rows(&provider.id, &ids) else { continue; };
        let allowed: HashSet<_> = checked.iter().map(|r| r.row.id.as_str()).collect();
        for row in rows {
            if !allowed.contains(row.row.id.as_str()) { continue; }
            let mut row = row.clone();
            row.row.provider = provider.name.clone();
            sanitize_caps(&mut row);
            valid.push(row);
        }
    }
    let note = if valid.len() < cached.len() { "已忽略已移除提供方或无法校验的缓存条目。" } else { "" };
    (valid, note.into())
}

fn save_cache(path: &Path, rows: &[RichModelRow], op: &Operation) -> String {
    if op.cancel.is_cancelled() { return String::new(); }
    let mut rows = rows.iter().take(MAX_CACHE_ROWS).cloned().collect::<Vec<_>>();
    for row in &mut rows { sanitize_caps(row); }
    let document = CacheDocument { version: 1, rows };
    let Ok(bytes) = serde_json::to_vec(&document) else { return "发现缓存未能编码；当前结果仍可使用。".into(); };
    if bytes.len() as u64 > MAX_CACHE_BYTES { return "模型清单较大，未保存发现缓存；当前结果仍可使用。".into(); }
    if op.cancel.is_cancelled() { return String::new(); }
    match crate::storage::atomic_write(path, &bytes) {
        Ok(()) => String::new(),
        Err(_) => "发现缓存保存失败；当前结果仍可使用，不影响模型配置读回结果。".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> ProviderInfo { ProviderInfo { id: id.into(), name: format!("提供方 {id}"), kind: "openai".into() } }
    fn row(pid: &str, id: &str) -> RichModelRow {
        let mut row = RichModelRow::from(models::ModelRow { provider: format!("提供方 {pid}"), provider_id: pid.into(), id: id.into() });
        row.capabilities.tools = Some(true);
        row.capabilities.context_window = Some(128_000);
        row.capabilities.sources.insert("tools".into(), "provider API".into());
        row
    }

    #[test]
    fn all_and_provider_selection_survive_reordering_by_id() {
        let providers = vec![provider("b"), provider("a")];
        assert_eq!(provider_at(&providers, 0), Some(None));
        assert_eq!(provider_at(&providers, 1), Some(Some("b".into())));
        assert_eq!(provider_at(&providers, -1), None);
        assert_eq!(provider_at(&providers, i32::MIN), None);
        assert_eq!(provider_at(&providers, 3), None);
        assert_eq!(provider_index(Some("a"), &providers), 2);
        assert_eq!(provider_index(None, &providers), 0);
        assert_eq!(retain_provider(Some("removed"), &providers), None);
    }

    #[test]
    fn filtered_groups_preserve_provider_and_capabilities() {
        let rows = vec![row("a", "GPT-X"), row("b", "GPT-X"), row("a", "unrelated")];
        let visible = filtered_indices(&rows, "gpt-x");
        let groups = group_rows(visible.iter().map(|i| &rows[*i]));
        assert_eq!(groups.len(), 2);
        assert_eq!(group_signature(&groups), vec![("a".into(), "GPT-X".into()), ("b".into(), "GPT-X".into())]);
        assert_eq!(groups[1].1[0].row.provider_id, "b");
        assert_eq!(groups[0].1[0].capabilities, rows[0].capabilities);
        assert_eq!(filtered_indices(&rows, "提供方 b"), vec![1]);
    }

    #[test]
    fn confirmation_requires_same_action_and_provider_model_pairs() {
        let groups = vec![("a".into(), vec![row("a", "one")])];
        let signature = group_signature(&groups);
        let confirmation = Confirmation { kind: ConfirmKind::Filtered, signature: signature.clone(), since: Instant::now() };
        assert!(confirmation.matches(ConfirmKind::Filtered, &signature));
        assert!(!confirmation.matches(ConfirmKind::Probe, &signature));
        assert!(!confirmation.matches(ConfirmKind::Manual, &signature));
        assert!(!confirmation.matches(ConfirmKind::Filtered, &[("b".into(), "one".into())]));
    }

    #[test]
    fn confirmation_expires_and_never_survives_changed_rows() {
        let confirmation = Confirmation {
            kind: ConfirmKind::Filtered,
            signature: vec![("a".into(), "one".into())],
            since: Instant::now() - CONFIRM_WINDOW - Duration::from_secs(1),
        };
        assert!(!confirmation.matches(ConfirmKind::Filtered, &confirmation.signature));
        let current = Confirmation { since: Instant::now(), ..confirmation };
        assert!(!current.matches(ConfirmKind::Filtered, &[("a".into(), "two".into())]));
    }

    #[test]
    fn stale_write_notice_reports_committed_work_without_returning_old_rows() {
        let mut report = ImportReport::default();
        report.result.added = 1;
        let result = Ok(Completed::Imported { report, verification: Verification { cancelled: true, ..Default::default() }, pending_note: String::new() });
        let note = previous_write_note(&result);
        assert!(note.contains("上一任务在停止前已提交配置"));
        assert!(note.contains("未完成读回确认"));
        assert!(!note.contains("已写入并读回确认"));
        assert!(previous_write_note(&Err("cancelled".into())).is_empty());
    }

    #[test]
    fn manual_ids_are_bounded_deduplicated_and_not_silently_truncated() {
        assert_eq!(parse_manual_ids(" b，a\na; b、c ").unwrap(), vec!["a", "b", "c"]);
        assert!(parse_manual_ids("\u{202e}bad").is_err());
        assert!(parse_manual_ids(&"x".repeat(257)).is_err());
        let too_many = (0..=models::MAX_MODELS).map(|i| format!("m{i}")).collect::<Vec<_>>().join(",");
        assert!(parse_manual_ids(&too_many).is_err());
    }

    #[test]
    fn text_usable_keeps_unknown_capabilities_but_cancelled_is_ineligible() {
        let mut outcome = RichProbeOutcome { usable: Some(true), tools: Some(false), image: Some(true), ..Default::default() };
        assert!(probe_eligible(&outcome));
        assert_eq!(outcome.pdf, None);
        outcome.cancelled = true;
        assert!(!probe_eligible(&outcome));
        outcome.cancelled = false;
        outcome.usable = None;
        assert!(!probe_eligible(&outcome));
    }

    #[test]
    fn discovery_summary_keeps_partial_errors_and_deadline() {
        let mut batch = DiscoveryBatch::default();
        batch.providers.push(models::RichDiscovery { provider_id: "a".into(), rows: vec![row("a", "one")], ..Default::default() });
        batch.errors.insert("b".into(), "认证失败，请检查配置。".into());
        batch.timed_out = true;
        let summary = discovery_summary(&batch, 3, 1, "");
        assert!(summary.contains("1 个提供方成功"));
        assert!(summary.contains("1 个失败"));
        assert!(summary.contains("1 个未处理"));
        assert!(summary.contains("认证失败"));
        assert!(summary.contains("总时限"));
    }

    #[test]
    fn import_result_never_claims_success_without_write_and_readback() {
        let mut report = ImportReport::default();
        let mut verification = Verification { retained: true, ..Default::default() };
        report.result.error = "配置已变化，尚未修改配置。".into();
        assert!(!import_summary(&report, &verification).contains("已写入并读回确认"));
        report.result.added = 2;
        report.partial = true;
        assert!(import_summary(&report, &verification).contains("仅部分文件"));
        assert!(import_summary(&report, &verification).contains("尚未取得 ZCode 运行态接受确认"));
        verification.cancelled = true;
        assert!(!import_summary(&report, &verification).contains("已写入并读回确认"));
    }

    #[test]
    fn cache_update_does_not_drop_other_providers_or_mix_same_ids() {
        let mut cache = vec![row("a", "one"), row("b", "one")];
        replace_cached_provider(&mut cache, "a", &[row("a", "new"), row("wrong", "ignored")]);
        assert_eq!(cache.len(), 2);
        assert!(cache.iter().any(|r| row_key(r) == ("b".into(), "one".into())));
        assert!(cache.iter().any(|r| row_key(r) == ("a".into(), "new".into())));
    }

    #[test]
    fn cache_schema_ignores_unknown_fields_and_only_retains_known_sources() {
        let bytes = br#"{"version":1,"apiKey":"must-not-survive","rows":[{"provider_id":"a","provider":"old","id":"one","capabilities":{"tools":true,"sources":{"tools":"raw-secret","input.image":"provider API","authorization":"secret"}}}]}"#;
        let mut rows = decode_cache(bytes).unwrap();
        sanitize_caps(&mut rows[0]);
        assert_eq!(rows[0].capabilities.sources.len(), 1);
        let encoded = serde_json::to_string(&CacheDocument { version: 1, rows }).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("apiKey"));
        assert!(decode_cache(br#"{"version":99,"rows":[]}"#).is_err());
    }

    #[test]
    fn auth_style_errors_are_never_rendered_raw() {
        assert_eq!(safe_message("request failed Authorization: Bearer secret"), "信息含认证样式内容，已隐藏。");
        assert_eq!(safe_message("认证失败。"), "认证失败。");
    }
}
