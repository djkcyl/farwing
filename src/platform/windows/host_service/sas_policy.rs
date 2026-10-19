//! Explicit installer-owned SAS policy change with conditional restoration.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use windows::{
    Win32::{Foundation::*, System::Registry::*},
    core::w,
};
const KEY: windows::core::PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
const VALUE: windows::core::PCWSTR = w!("SoftwareSASGeneration");
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    schema: u8,
    previous: Option<u32>,
    applied: u32,
}
fn read() -> Result<Option<u32>> {
    let mut value = 0u32;
    let mut size = 4;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            KEY,
            VALUE,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    ensure!(value <= 3, "安全注意序列策略值未知，未修改");
    Ok(Some(value))
}
fn write(value: Option<u32>) -> Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, KEY, Some(0), KEY_SET_VALUE, &mut key).ok()?;
    }
    let result = unsafe {
        match value {
            Some(value) => {
                RegSetValueExW(key, VALUE, Some(0), REG_DWORD, Some(&value.to_le_bytes()))
            }
            None => RegDeleteValueW(key, VALUE),
        }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if value.is_none() && result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    result.ok()?;
    Ok(())
}
fn receipt(directory: &Path) -> std::path::PathBuf {
    directory.join("sas-policy.json")
}
pub(super) fn enable(directory: &Path) -> Result<()> {
    let current = read()?;
    let file = receipt(directory);
    if file.exists() {
        let previous: Change = serde_json::from_slice(&std::fs::read(&file)?)?;
        ensure!(
            previous.schema == 1 && matches!(previous.applied, 1 | 3),
            "被控服务策略记录无效"
        );
        if current == Some(previous.applied) {
            return Ok(());
        }
    }
    if matches!(current, Some(1 | 3)) {
        return Ok(());
    }
    let applied = if current == Some(2) { 3 } else { 1 };
    // Record before the write. Removal checks the applied value before restoring.
    std::fs::write(
        file,
        serde_json::to_vec(&Change {
            schema: 1,
            previous: current,
            applied,
        })?,
    )?;
    write(Some(applied))
}
pub(super) fn restore(directory: &Path) -> Result<()> {
    let file = receipt(directory);
    if !file.exists() {
        return Ok(());
    }
    let change: Change = serde_json::from_slice(&std::fs::read(&file)?)?;
    ensure!(
        change.schema == 1
            && matches!(change.applied, 1 | 3)
            && change.previous.is_none_or(|v| v <= 3),
        "被控服务策略记录无效"
    );
    if read()? == Some(change.applied) {
        write(change.previous)?;
    }
    std::fs::remove_file(file)?;
    Ok(())
}
