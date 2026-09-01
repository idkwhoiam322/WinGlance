use crate::winapi::reg_set_value;
use crate::winutil::wide;
use anyhow::{Context, Result};
use log::{info, warn};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_BINARY, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyW,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "WinGlance";

/// The Task-Manager-managed companion key: its `WinGlance` value overrides
/// the Run entry's presence (a disabled bit here means Windows never launches
/// the app at logon even though the Run value exists).
const STARTUP_APPROVED_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";

/// The ownership marker written into this app's Run command:
/// `"<exact exe>" --winglance-autostart`. The token is accepted and ignored
/// by `main` (which only scans for `--winglance-restart-nonce` and
/// `--reload-config`), so an autostart launch behaves exactly like a plain
/// one. Ownership rules:
///
/// - A stored command naming this installation's exact executable is ours
///   (unmarked legacy entries included; enabling upgrades them to the marked
///   form).
/// - Any other command — a different live path, a deleted path, a relative
///   command — is ours only when it carries this exact token AND its
///   executable shares our file name: the marker alone among foreign-named
///   commands proves nothing, and the file name alone proves nothing.
pub(crate) const AUTOSTART_MARKER: &str = "--winglance-autostart";

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
    // The command carries the ownership marker (see `AUTOSTART_MARKER`).
    let target = format!("\"{}\" {AUTOSTART_MARKER}", exe.to_string_lossy());
    let target_wide = wide(&target);
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
                        // Repair the stale command (an unmarked legacy entry,
                        // or one from before the exe moved). The value is
                        // ours, so rewriting it cannot clobber another
                        // program's entry.
                        let data = std::slice::from_raw_parts(target_wide.as_ptr().cast::<u8>(), target_wide.len() * 2);
                        reg_set_value(key, PCWSTR(value.as_ptr()), REG_SZ, Some(data))
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
                reg_set_value(key, PCWSTR(value.as_ptr()), REG_SZ, Some(data))
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
            info!("start-on-login state applied: enabled={enabled}");
            log_startup_approved_drift(enabled);
            Ok(())
        } else {
            anyhow::bail!("updating the start-on-login registry entry failed: {error:?}")
        }
    }
}

/// Reads the Task-Manager-managed enablement for this app's Run value:
/// `Some(true)` = explicitly enabled there, `Some(false)` = the user
/// disabled it there, `None` = unreadable or never written (no drift to
/// report). The value's byte layout is undocumented but stable: the first
/// byte's low bit set means "disabled", a clear low bit (0x02) means
/// "enabled". Read-only — this sync never writes to StartupApproved.
fn startup_approved_enabled() -> Option<bool> {
    unsafe {
        let key_name = wide(STARTUP_APPROVED_RUN_KEY);
        let value = wide(VALUE_NAME);
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut len: u32 = 0;
        let rc = RegQueryValueExW(key, PCWSTR(value.as_ptr()), None, Some(&mut ty), None, Some(&mut len));
        let state = if rc == WIN32_ERROR(0) && ty == REG_BINARY && len as usize >= std::mem::size_of::<u8>() {
            let mut first = 0u8;
            let mut got = 1u32;
            let rc = RegQueryValueExW(
                key,
                PCWSTR(value.as_ptr()),
                None,
                None,
                Some(&mut first),
                Some(&mut got),
            );
            if rc == WIN32_ERROR(0) {
                // Low bit clear = enabled, set = user-disabled.
                Some(first & 0x01 == 0)
            } else {
                None
            }
        } else {
            None
        };
        let _ = RegCloseKey(key);
        state
    }
}

/// Logs the StartupApproved drift: Task Manager's Startup toggle lives in
/// `StartupApproved`, which this app does not write — so after every sync,
/// a mismatch between that toggle and our configured state is called out
/// instead of leaving the Settings row silently lying.
fn log_startup_approved_drift(enabled: bool) {
    if let Some(approved) = startup_approved_enabled()
        && approved != enabled
    {
        warn!(
            "start-on-login drift: StartupApproved marks WinGlance as {} while start_on_login is {} \
             — Windows follows StartupApproved, so re-toggle it in Task Manager's Startup list",
            if approved { "enabled" } else { "disabled" },
            if enabled { "on" } else { "off" },
        );
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
    let mut units: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    match String::from_utf16(&units) {
        Ok(value) => RunValue::Ours(value),
        Err(_) => RunValue::Foreign,
    }
}

/// Whether a stored Run value belongs to this installation.
///
/// - The stored command's executable token naming this installation's exact
///   path decides first (case-insensitive): an entry naming this exact
///   installation is ours no matter its arguments — legacy unmarked entries
///   included, and enabling upgrades them to the marked form.
/// - Everything else is ours only when the command carries the exact
///   `--winglance-autostart` token AND its executable shares our file name
///   (case-insensitive): a marked entry from before the exe moved, or a
///   second WinGlance installation. The file name alone never decides (a
///   deleted foreign same-name entry is left untouched), and the marker
///   alone never decides (a foreign command that merely embeds our token is
///   left untouched). Lookalike tokens (`--winglance-autostart2`,
///   `--winglance-autostart=x`) do not count: the argument must be exactly
///   the marker.
fn owned_by(current_exe: &Path, current_exe_name: &str, stored: &str) -> bool {
    let stored = stored.trim();
    let (token, rest) = if let Some(after_quote) = stored.strip_prefix('"') {
        // Quoted command: take up to the closing quote, so a path with
        // spaces is not split.
        let end = after_quote.find('"').unwrap_or(after_quote.len());
        let token = &after_quote[..end];
        let rest = after_quote.get(end + 1..).unwrap_or("");
        (token, rest)
    } else {
        // Unquoted command: take up to the first space.
        let end = stored.find(' ').unwrap_or(stored.len());
        (&stored[..end], &stored[end..])
    };
    if token.is_empty() {
        return false;
    }
    let stored_path = Path::new(token);
    if stored_path.is_absolute()
        && stored_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&current_exe.to_string_lossy())
    {
        return true;
    }
    // Not our exact path: the marker plus our file name must both be present.
    let marked = rest.split_whitespace().any(|arg| arg == AUTOSTART_MARKER);
    let name = stored_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    marked && !name.is_empty() && name.eq_ignore_ascii_case(current_exe_name)
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
        // (case-insensitive path comparison on Windows).
        assert!(owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\PROGRAM FILES\WINGLANCE\WINGLANCE.EXE""#
        ));
    }

    #[test]
    fn live_same_name_file_at_a_different_path_stays_foreign_without_the_marker() {
        // A different installation with the same exe name exists: without the
        // marker it is never ours (the file name alone never
        // decides).
        let guard = TempDir::new();
        let foreign = guard.file("WinGlance.exe", true);
        let stored = format!("\"{}\"", foreign.to_string_lossy());
        assert!(!owned_by(exe(), "WinGlance.exe", &stored));
        // With the marker it is a WinGlance installation (ours to repair).
        let stored = format!("\"{}\" {AUTOSTART_MARKER}", foreign.to_string_lossy());
        assert!(owned_by(exe(), "WinGlance.exe", &stored));
    }

    #[test]
    fn deleted_foreign_same_name_path_is_not_owned_anymore() {
        // The old basename fallback owned a deleted same-name entry; the
        // marker rule closes that hole — an unmarked stale entry of any
        // origin is left untouched.
        let guard = TempDir::new();
        let stale = guard.file("old/WinGlance.exe", false);
        let stored = format!("\"{}\" --minimized", stale.to_string_lossy());
        assert!(!owned_by(exe(), "WinGlance.exe", &stored));
    }

    #[test]
    fn moved_marked_path_is_still_ours() {
        // Stale entry from before the exe moved, carrying the marker: ours.
        let guard = TempDir::new();
        let stale = guard.file("old/WinGlance.exe", false);
        let stored = format!("\"{}\" {AUTOSTART_MARKER}", stale.to_string_lossy());
        assert!(owned_by(exe(), "WinGlance.exe", &stored));
    }

    #[test]
    fn exact_old_unmarked_path_is_owned_and_upgrades() {
        // Legacy pre-marker entry naming the current executable: owned, and
        // the enable path rewrites it into the marked form (the target
        // string includes the marker, so `current != target` triggers the
        // rewrite).
        let legacy = r#""C:\Program Files\WinGlance\WinGlance.exe""#;
        assert!(owned_by(exe(), "WinGlance.exe", legacy));
        let upgraded = format!("\"{}\" {AUTOSTART_MARKER}", exe().to_string_lossy());
        assert_ne!(legacy, upgraded);
        assert!(owned_by(exe(), "WinGlance.exe", &upgraded));
    }

    #[test]
    fn lookalike_marker_arguments_do_not_count() {
        let guard = TempDir::new();
        let stale = guard.file("old/WinGlance.exe", false);
        for lookalike in [
            "--winglance-autostart2",
            "--winglance-autostart=x",
            "--winglance-autostarts",
            "-winglance-autostart",
        ] {
            let stored = format!("\"{}\" {lookalike}", stale.to_string_lossy());
            assert!(!owned_by(exe(), "WinGlance.exe", &stored), "{lookalike} must not own");
        }
        // The exact token among other arguments still counts.
        let stored = format!("\"{}\" --minimized {AUTOSTART_MARKER} --other", stale.to_string_lossy());
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
        // The marker alone never decides: a foreign-named command carrying
        // our exact token stays foreign.
        assert!(!owned_by(
            exe(),
            "WinGlance.exe",
            r#""C:\Program Files\Other\other.exe" --winglance-autostart"#
        ));
    }

    #[test]
    fn relative_command_needs_the_marker_and_our_file_name() {
        assert!(!owned_by(exe(), "WinGlance.exe", "WinGlance.exe --silent"));
        assert!(owned_by(exe(), "WinGlance.exe", "WinGlance.exe --winglance-autostart"));
        assert!(!owned_by(exe(), "WinGlance.exe", "winthing.exe --winglance-autostart"));
    }
}
