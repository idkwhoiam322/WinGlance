use crate::winutil::wide;
use anyhow::{Context, Result};
use log::warn;
use std::path::Path;
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
    // Ownership is keyed on the full executable path, with a file-name
    // fallback: a stale entry pointing at a WinGlance.exe that has since
    // moved still identifies as ours.
    let exe_path = exe.clone();
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
            RunValue::Ours(current) if owned_by(&exe_path, exe_name, &current) => {
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

/// Whether a stored Run value belongs to this installation. The full
/// executable path decides first (case-insensitive, as Path equality is on
/// Windows): an entry naming this exact installation is ours no matter what
/// its file name is. When the stored absolute path is a *different* live
/// executable, the entry is foreign and never touched — a foreign app that
/// merely shares the file name must not be clobbered. The executable file
/// name only decides (case-insensitive) for relative commands and for
/// absolute paths that no longer exist: an entry left behind by a
/// WinGlance.exe that has since moved is still recognized as ours.
fn owned_by(current_exe: &Path, current_exe_name: &str, stored: &str) -> bool {
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
    let stored_path = Path::new(token);
    if stored_path.is_absolute() {
        if stored_path == current_exe {
            return true;
        }
        // A live executable at a different path is a different program, even
        // with the same file name. Only a stored path that no longer exists
        // can be a stale entry of this installation.
        if std::fs::metadata(stored_path).is_ok() {
            return false;
        }
    }
    let name = stored_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Documented trade-off of the basename fallback: a *deleted* foreign
    // program whose entry survives in the Run key with the same file name
    // would be treated as owned. The Run value is only ever rewritten or
    // removed when the user toggles WinGlance autostart, so the blast radius
    // is a single stale foreign entry, accepted in exchange for cleaning up
    // entries left by a WinGlance.exe that has since moved. Stronger
    // ownership would need an installation marker in the command line.
    !name.is_empty() && name.eq_ignore_ascii_case(current_exe_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temporary directory that is removed after the test, so the
    /// existence checks exercise the real filesystem deterministically.
    struct TempDir {
        dir: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-test-autostart-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        /// A path inside the temp dir; the file only exists when `create` is
        /// set, so both the live-foreign and the moved-stale cases are
        /// controllable.
        fn file(&self, name: &str, create: bool) -> std::path::PathBuf {
            let path = self.dir.join(name);
            if create {
                std::fs::write(&path, b"not an exe").unwrap();
            }
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn exe() -> &'static Path {
        Path::new(r"C:\Program Files\WinGlance\WinGlance.exe")
    }

    #[test]
    fn exact_full_path_is_owned_with_any_arguments() {
        assert!(owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\Program Files\WinGlance\WinGlance.exe""#
        ));
        assert!(owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\Program Files\WinGlance\WinGlance.exe" --minimized"#
        ));
    }

    #[test]
    fn full_path_wins_over_a_different_file_name() {
        // The full path is identical even though the file name differs
        // (case-insensitive Path equality on Windows).
        assert!(owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\PROGRAM FILES\WINGLANCE\WINGLANCE.EXE""#
        ));
    }

    #[test]
    fn live_same_name_file_at_a_different_path_stays_foreign() {
        // A different installation with the same exe name exists: never ours.
        let guard = TempDir::new();
        let foreign = guard.file("WinGlance.exe", true);
        let stored = format!("\"{}\"", foreign.to_string_lossy());
        assert!(!owned_by(exe(), "WinGlance.exe", &stored));
    }

    #[test]
    fn moved_exe_is_still_ours_via_the_file_name_fallback() {
        // Stale entry from before the exe moved: the stored absolute path no
        // longer exists, so the file name decides.
        let guard = TempDir::new();
        let stale = guard.file("old/WinGlance.exe", false);
        let stored = format!("\"{}\" --minimized", stale.to_string_lossy());
        assert!(owned_by(exe(), "WinGlance.exe", &stored));
    }

    #[test]
    fn a_foreign_executable_is_never_owned() {
        assert!(!owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\Program Files\Other\other.exe" --x"#
        ));
        assert!(!owned_by(exe(), "WinGlance.exe", "notepad.exe"));
        assert!(!owned_by(exe(), "WinGlance.exe", ""));
    }

    #[test]
    fn relative_command_uses_the_file_name() {
        assert!(owned_by(exe(), "WinGlance.exe", "WinGlance.exe --silent"));
        assert!(!owned_by(exe(), "WinGlance.exe", "winthing.exe --x"));
    }
}
