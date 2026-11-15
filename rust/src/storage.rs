//! 配置文件写入：同目录临时文件、刷盘、原子替换，失败时保留旧文件。

use std::{fs::{self, OpenOptions}, io::Write, path::Path, sync::atomic::{AtomicU64, Ordering}};
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("文件路径没有父目录")?;
    fs::create_dir_all(dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
    let tmp = dir.join(format!(".{}.{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| e.to_string())?;
        f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
        drop(f);
        replace(&tmp, path)
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

#[cfg(windows)]
fn replace(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH}};
    let s: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let t: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { MoveFileExW(PCWSTR(s.as_ptr()), PCWSTR(t.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }.map_err(|e| format!("无法替换配置文件：{e}"))
}
#[cfg(not(windows))]
fn replace(source: &Path, target: &Path) -> Result<(), String> { fs::rename(source, target).map_err(|e| e.to_string()) }

pub fn json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    atomic_write(path, &serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn replacement_keeps_a_valid_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        json(&p, &serde_json::json!({"a":1})).unwrap();
        json(&p, &serde_json::json!({"b":2})).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&fs::read(p).unwrap()).unwrap(), serde_json::json!({"b":2}));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
