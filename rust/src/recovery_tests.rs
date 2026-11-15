use super::*;

fn fixture(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let install = root.join(name);
    fs::create_dir_all(install.join("resources/app")).unwrap();
    fs::write(install.join("ZCode.exe"), b"fixture executable, never run").unwrap();
    fs::write(install.join("resources/app.asar"), bytes).unwrap();
    fs::write(install.join("resources/app/package.json"), br#"{"version":"1.2.3","auth":"must-not-be-saved"}"#).unwrap();
    install.join("ZCode.exe")
}

fn enabled() -> WallpaperSettings {
    WallpaperSettings { enabled: true, path: "persistent-wallpapers/fixture.png".into(), ..Default::default() }
}

fn targets(name: &str) -> Result<Vec<String>, String> {
    Ok(vec![format!("{:x}", Sha256::digest(name.as_bytes()))])
}

fn observe_fixture(root: &Path, exe: Option<&Path>, settings: &WallpaperSettings, target: &str) -> RecoveryReport {
    let current = load_state(root).unwrap();
    let stamp = exe.map(|p| inspect_with_previous(p, current.client.as_ref()));
    observe_inner(root, current, stamp, settings, targets(target)).unwrap()
}

#[test]
fn first_online_baseline_and_unchanged_package_are_not_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"official package v1");
    let data = tmp.path().join("data");
    let settings = enabled();
    let first = observe_fixture(&data, Some(&exe), &settings, "renderer-1");
    assert!(!first.changed && !first.needs_restore && !first.restored);
    let bytes = fs::read(data.join(STATE_FILE)).unwrap();
    let second = observe_fixture(&data, Some(&exe), &settings, "renderer-1");
    assert!(!second.changed && !second.needs_restore && !second.restored);
    assert_eq!(bytes, fs::read(data.join(STATE_FILE)).unwrap());
    assert!(!data.join("backups").exists());
    let stamp = inspect(&exe).unwrap();
    assert_eq!(stamp.version.as_deref(), Some("1.2.3"));
    assert_eq!(stamp.hash, format!("{:x}", Sha256::digest(b"official package v1")));
    assert!(!String::from_utf8(bytes).unwrap().contains("must-not-be-saved"));
}

#[test]
fn replaced_package_records_both_fingerprints_without_copying_package() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"official package v1");
    let data = tmp.path().join("data");
    let settings = enabled();
    observe_fixture(&data, Some(&exe), &settings, "renderer-1");
    let before = load_state(&data).unwrap().client.unwrap();
    fs::write(exe.parent().unwrap().join("resources/app.asar"), b"official package v2 is larger").unwrap();
    let report = observe_fixture(&data, Some(&exe), &settings, "renderer-1");
    assert!(report.changed && report.needs_restore && !report.restored);
    let state = load_state(&data).unwrap();
    assert!(state.pending);
    let after = state.client.unwrap();
    assert_ne!(before.hash, after.hash);
    let backups: Vec<_> = fs::read_dir(data.join("backups")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(backups.len(), 1);
    let backup: serde_json::Value = read_json(&backups[0]).unwrap();
    assert_eq!(backup["before"]["hash"], before.hash);
    assert_eq!(backup["after"]["hash"], after.hash);
    assert_eq!(backup["wallpaper_settings"]["path"], settings.path);
    assert!(backups[0].metadata().unwrap().len() < 4096);
    assert!(!fs::read_to_string(&backups[0]).unwrap().contains("must-not-be-saved"));
    let repeat = observe_fixture(&data, Some(&exe), &settings, "renderer-1");
    assert!(!repeat.changed && repeat.needs_restore);
    assert_eq!(fs::read_dir(data.join("backups")).unwrap().count(), 1);
}

#[test]
fn same_length_replacement_with_changed_mtime_rehashes() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"v001");
    let before = inspect(&exe).unwrap();
    let package = exe.parent().unwrap().join("resources/app.asar");
    fs::write(&package, b"v002").unwrap();
    // 显式设置 fixture 时间，不依赖文件系统时间粒度或 sleep。
    File::options().write(true).open(&package).unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();
    let after = inspect(&exe).unwrap();
    assert_eq!(before.len, after.len);
    assert_ne!(before.modified, after.modified);
    assert_ne!(before.hash, after.hash);
}

#[test]
fn moving_client_schedules_restore_even_with_identical_content() {
    let tmp = tempfile::tempdir().unwrap();
    let old = fixture(tmp.path(), "client-old", b"same official content");
    let new = fixture(tmp.path(), "client-new", b"same official content");
    let data = tmp.path().join("data");
    let settings = enabled();
    observe_fixture(&data, Some(&old), &settings, "renderer-1");
    let report = observe_fixture(&data, Some(&new), &settings, "renderer-1");
    assert!(report.changed && report.needs_restore);
    assert_eq!(inspect(&old).unwrap().hash, inspect(&new).unwrap().hash);
    assert_eq!(load_state(&data).unwrap().client.unwrap().exe_path, fs::canonicalize(new).unwrap());
}

#[test]
fn offline_pending_survives_reload_and_reconnect_until_explicit_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = enabled();
    let report = observe_inner(tmp.path(), RecoveryState::default(), None, &settings, Err("offline".into())).unwrap();
    assert!(!report.changed && report.needs_restore && !report.restored);
    let state = load_state(tmp.path()).unwrap();
    assert!(state.pending && !state.cdp_online);
    assert_eq!(state.wallpaper_settings.as_ref(), Some(&settings));
    let reconnected = observe_fixture(tmp.path(), None, &settings, "renderer-1");
    assert!(!reconnected.changed && reconnected.needs_restore && !reconnected.restored);
    assert!(load_state(tmp.path()).unwrap().pending);
}

#[test]
fn renderer_restart_and_new_window_schedule_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = enabled();
    observe_fixture(tmp.path(), None, &settings, "renderer-old");
    let report = observe_fixture(tmp.path(), None, &settings, "renderer-new");
    assert!(report.changed && report.needs_restore && !report.restored);
    assert!(!tmp.path().join("backups").exists());
}

#[test]
fn offline_observation_keeps_previous_target_and_client() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"package");
    let data = tmp.path().join("data");
    let settings = enabled();
    observe_fixture(&data, Some(&exe), &settings, "renderer-old");
    let previous = load_state(&data).unwrap();
    observe_inner(&data, previous.clone(), None, &settings, Err("offline".into())).unwrap();
    let offline = load_state(&data).unwrap();
    assert_eq!(offline.client, previous.client);
    assert_eq!(offline.target_fingerprints, previous.target_fingerprints);
    let report = observe_fixture(&data, None, &settings, "renderer-new");
    assert!(report.changed && report.needs_restore);
}

#[test]
fn changed_settings_including_disable_preserve_pending_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = enabled();
    observe_fixture(tmp.path(), None, &settings, "renderer");
    let mut disabled = settings.clone();
    disabled.enabled = false;
    let report = observe_fixture(tmp.path(), None, &disabled, "renderer");
    assert!(report.changed && report.needs_restore && !report.restored);
    assert_eq!(load_state(tmp.path()).unwrap().wallpaper_settings, Some(disabled));
}

#[test]
fn initially_disabled_visuals_do_not_schedule_restore_for_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"v1");
    let data = tmp.path().join("data");
    let settings = WallpaperSettings::default();
    observe_fixture(&data, Some(&exe), &settings, "renderer");
    fs::write(exe.parent().unwrap().join("resources/app.asar"), b"v2-longer").unwrap();
    let report = observe_fixture(&data, Some(&exe), &settings, "renderer");
    assert!(report.changed && !report.needs_restore && !report.restored);
}

#[test]
fn unavailable_client_does_not_erase_previous_stamp_or_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fixture(tmp.path(), "client", b"package");
    let data = tmp.path().join("data");
    let settings = enabled();
    observe_fixture(&data, Some(&exe), &settings, "renderer");
    let previous = load_state(&data).unwrap();
    fs::remove_file(exe.parent().unwrap().join("resources/app.asar")).unwrap();
    let report = observe_fixture(&data, Some(&exe), &settings, "renderer");
    assert!(!report.changed && !report.restored);
    assert_eq!(load_state(&data).unwrap().client, previous.client);
}

#[test]
fn corrupt_or_future_state_is_never_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(STATE_FILE);
    fs::write(&path, b"broken fixture").unwrap();
    assert!(load_state(tmp.path()).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"broken fixture");
    fs::write(&path, br#"{"schema_version":999}"#).unwrap();
    assert!(load_state(tmp.path()).is_err());
    assert_eq!(fs::read(&path).unwrap(), br#"{"schema_version":999}"#);
}

#[test]
fn target_selection_deduplicates_and_never_persists_raw_urls() {
    let bytes = br#"[
        {"type":"page","title":"ZCode","url":"file:///app/out/renderer/index.html","webSocketDebuggerUrl":"ws://localhost:9222/devtools/page/one"},
        {"type":"page","title":"ZCode","url":"file:///app/out/renderer/index.html","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/page/one"},
        {"type":"page","title":"Other","url":"https://example.com/","webSocketDebuggerUrl":"ws://evil.example:9222/devtools/page/other"}
    ]"#;
    let selected = target_fingerprints(bytes, 9222).unwrap();
    assert_eq!(selected.len(), 1);
    assert!(valid_hash(&selected[0]));
    let bad = br#"[{"type":"page","title":"ZCode","url":"file:///app/out/renderer/index.html","webSocketDebuggerUrl":"ws://evil.example:9222/devtools/page/one"}]"#;
    assert!(target_fingerprints(bad, 9222).is_err());
    assert!(target_fingerprints(bytes, 9223).is_err());
}
