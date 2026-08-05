use anyhow::{Context, Result};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "notch";

/// Syncs the HKCU Run entry with the configured start-on-login state.
/// Writing the key lets Windows launch notch.exe at logon; deleting it
/// removes the entry. A missing value while disabling is not an error.
pub fn apply(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe().context("getting the executable path")?;
    // Quote the path: Windows splits an unquoted Run-key command line on
    // spaces when resolving the executable, so an install path containing a
    // space could fail to launch at logon or resolve to a different program.
    let exe = wide(&format!("\"{}\"", exe.to_string_lossy()));
    let value = wide(VALUE_NAME);
    let run_key = wide(RUN_KEY);
    unsafe {
        let mut key = HKEY::default();
        if !RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(run_key.as_ptr()), &mut key).is_ok() {
            anyhow::bail!("RegCreateKeyW failed for the Run key");
        }

        let error = if enabled {
            let data = std::slice::from_raw_parts(exe.as_ptr().cast::<u8>(), exe.len() * 2);
            RegSetValueExW(key, PCWSTR(value.as_ptr()), 0, REG_SZ, Some(data))
        } else {
            RegDeleteValueW(key, PCWSTR(value.as_ptr()))
        };
        let _ = RegCloseKey(key);

        // ERROR_FILE_NOT_FOUND: the entry is not present; nothing to remove.
        if error.is_ok() || (!enabled && error == WIN32_ERROR(2)) {
            Ok(())
        } else {
            anyhow::bail!("updating the start-on-login registry entry failed: {error:?}")
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
