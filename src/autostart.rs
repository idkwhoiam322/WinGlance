use crate::winutil::wide;
use anyhow::{Context, Result};
use log::warn;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "WinGlance";

/// What the Run key currently holds for this app's value name.
enum RunValue {
    /// No value present.
    Missing,
    /// A value exists but its type or content could not be verified as ours:
    /// never overwrite or delete it.
    Foreign,
    /// A REG_SZ value whose command names this installation's executable:
    /// still owned when stale (the exe moved or the command gained
    /// arguments).
    Ours(String),
}

/// Syncs the HKCU Run entry with the configured start-on-login state. The
/// `WinGlance` value is owned by this app only when it is a REG_SZ whose
/// command names this executable: an unreadable, differently-typed, or
/// foreign value is left untouched (both when enabling and disabling), so
/// the toggle can never destroy an entry it does not own. A stale entry that
/// still names this executable (the exe moved, or the command gained
/// arguments) is ours: enabling repairs it, disabling removes it. A missing
/// value while disabling is not an error.
pub fn apply(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe().context("getting the executable path")?;
    // Quote the path: Windows splits an unquoted Run-key command line on
    // spaces when resolving the executable, so an install path containing a
    // space could fail to launch at logon or resolve to a different program.
    let target = format!("\"{}\"", exe.to_string_lossy());
    let target_wide = wide(&target);
    // Ownership is keyed on the executable file name, not the full command
    // line: a stale entry pointing at a WinGlance.exe that has since moved
    // still identifies as ours.
    let exe_name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let value = wide(VALUE_NAME);
    let run_key = wide(RUN_KEY);
    unsafe {
        let mut key = HKEY::default();
        if !RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(run_key.as_ptr()), &mut key).is_ok() {
            anyhow::bail!("RegCreateKeyW failed for the Run key");
        }

        // Note: the ownership check and the write/delete below are not atomic;
        // another process could replace the value in between. Registry offers
        // no read-modify-write transaction for a single value, so this window
        // is accepted and the check at least refuses to clobber anything we
        // could not positively identify as ours.
        let error = match read_run_value(key, &value) {
            RunValue::Ours(current) if owned_by(exe_name, &current) => {
                if enabled {
                    if current == target {
                        // Already in the desired state.
                        WIN32_ERROR(0)
                    } else {
                        // Repair the stale command (the exe moved, or the
                        // entry gained arguments). The value is ours, so
                        // rewriting it cannot clobber another program's entry.
                        let data = std::slice::from_raw_parts(target_wide.as_ptr().cast::<u8>(), target_wide.len() * 2);
                        RegSetValueExW(key, PCWSTR(value.as_ptr()), 0, REG_SZ, Some(data))
                    }
                } else {
                    RegDeleteValueW(key, PCWSTR(value.as_ptr()))
                }
            }
            RunValue::Ours(_) | RunValue::Foreign => {
                warn!("the Run entry '{VALUE_NAME}' is not owned by this installation; leaving it untouched");
                WIN32_ERROR(0)
            }
            RunValue::Missing if enabled => {
                let data = std::slice::from_raw_parts(target_wide.as_ptr().cast::<u8>(), target_wide.len() * 2);
                RegSetValueExW(key, PCWSTR(value.as_ptr()), 0, REG_SZ, Some(data))
            }
            RunValue::Missing => {
                // No entry to remove.
                WIN32_ERROR(0)
            }
        };
        let _ = RegCloseKey(key);

        // ERROR_FILE_NOT_FOUND: the value vanished between the read and the
        // delete; nothing to remove.
        if error.is_ok() || (!enabled && error == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            anyhow::bail!("updating the start-on-login registry entry failed: {error:?}")
        }
    }
}

/// Reads the current value. Only a readable REG_SZ counts as owned-by-someone:
/// every other outcome (query failure, non-REG_SZ type, undecodable UTF-16) is
/// `Foreign` so the caller never writes over or deletes a value it could not
/// positively identify.
fn read_run_value(key: HKEY, name: &[u16]) -> RunValue {
    let mut ty = REG_VALUE_TYPE(0);
    let mut len: u32 = 0;
    let rc = unsafe { RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, Some(&mut ty), None, Some(&mut len)) };
    if rc == ERROR_FILE_NOT_FOUND {
        return RunValue::Missing;
    }
    if rc != WIN32_ERROR(0) || ty != REG_SZ {
        return RunValue::Foreign;
    }
    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
    };
    if rc != WIN32_ERROR(0) || ty != REG_SZ {
        return RunValue::Foreign;
    }
    let mut units: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    match String::from_utf16(&units) {
        Ok(value) => RunValue::Ours(value),
        Err(_) => RunValue::Foreign,
    }
}

/// Whether a stored Run value belongs to this installation. The command's
/// executable file name (case-insensitive) decides, not the whole command
/// line: an entry left behind by a WinGlance.exe that has since moved or
/// gained arguments is still recognized as ours. A value naming a different
/// executable stays foreign and is never touched.
fn owned_by(current_exe_name: &str, stored: &str) -> bool {
    let stored = stored.trim();
    let token = if let Some(rest) = stored.strip_prefix('"') {
        // Quoted command: take up to the closing quote, so a path with
        // spaces is not split.
        let end = rest.find('"').unwrap_or(rest.len());
        &rest[..end]
    } else {
        // Unquoted command: take up to the first space.
        let end = stored.find(' ').unwrap_or(stored.len());
        &stored[..end]
    };
    if token.is_empty() {
        return false;
    }
    let name = std::path::Path::new(token)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    !name.is_empty() && name.eq_ignore_ascii_case(current_exe_name)
}
