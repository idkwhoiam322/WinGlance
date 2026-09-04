from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:220]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def insert_before(path: str, marker: str, addition: str) -> None:
    replace(path, marker, addition + marker)

# --- Additive config metadata for stable monitor identity -------------------
p = "src/config.rs"
replace(
    p,
    '''    /// Which display the pill is placed on (see `MonitorMode`).\n    pub monitor: MonitorMode,\n''',
    '''    /// Which display the pill is placed on (see `MonitorMode`).\n    pub monitor: MonitorMode,\n    /// Win32 monitor-interface path captured for an explicit `index-N` pick.\n    /// Managed by WinGlance, not required in hand-authored configs. The\n    /// companion index proves the identity belongs to the current monitor\n    /// value, so a manual `monitor = "index-M"` edit cannot inherit a stale\n    /// identity from the old index.\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub monitor_device_id: Option<String>,\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub monitor_device_index: Option<u32>,\n''',
)
replace(
    p,
    '''    /// Which display the Compact pill is placed on while it uses its own\n    /// position (see `MonitorMode`).\n    pub compact_monitor: MonitorMode,\n''',
    '''    /// Which display the Compact pill is placed on while it uses its own\n    /// position (see `MonitorMode`).\n    pub compact_monitor: MonitorMode,\n    /// Managed monitor-interface identity for the independent Compact slot.\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub compact_monitor_device_id: Option<String>,\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub compact_monitor_device_index: Option<u32>,\n''',
)
replace(
    p,
    '''            position_y: None,\n            monitor: MonitorMode::default(),\n            layout: LayoutMode::default(),\n''',
    '''            position_y: None,\n            monitor: MonitorMode::default(),\n            monitor_device_id: None,\n            monitor_device_index: None,\n            layout: LayoutMode::default(),\n''',
)
replace(
    p,
    '''            compact_position_y: None,\n            compact_monitor: MonitorMode::default(),\n            dismiss_on_hover: true,\n''',
    '''            compact_position_y: None,\n            compact_monitor: MonitorMode::default(),\n            compact_monitor_device_id: None,\n            compact_monitor_device_index: None,\n            dismiss_on_hover: true,\n''',
)
replace(
    p,
    '''/// `Index(n)` is resolved against the *current* enumeration of active\n/// displays every time the pill is placed; an index that is temporarily\n/// out of range (a display unplugged or reordered after the config was\n/// saved) falls back to the primary display at placement time while the\n/// configured value is preserved, so it becomes valid again automatically\n/// when the display comes back.\n''',
    '''/// `Index(n)` remains the human-readable fallback. When WinGlance can\n/// resolve that index it also records Windows' per-monitor device-interface\n/// path in additive managed fields; later runs prefer that identity, so an\n/// enumeration reorder does not silently move the pill to another physical\n/// monitor. If the remembered monitor is absent, placement falls back to the\n/// primary display without rewriting the user's index or identity.\n''',
)

# --- Enumerate and resolve a per-monitor interface identity -----------------
p = "src/overlay/fullscreen.rs"
replace(p, "use std::collections::HashMap;\n", "")
replace(p, "use std::sync::{Arc, LazyLock, Mutex};\n", "use std::sync::{Arc, Mutex};\n")
replace(
    p,
    '''use windows::Win32::Graphics::Gdi::{\n    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,\n    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,\n};\n''',
    '''use windows::Win32::Graphics::Gdi::{\n    DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW,\n    GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,\n};\n''',
)
replace(
    p,
    '''    /// The device name (`\\\\.\\DISPLAY1`), as reported by the system.\n    pub name: String,\n}\n''',
    '''    /// The GDI display device name (`\\\\.\\DISPLAY1`), useful for\n    /// diagnostics but not durable enough to persist as monitor identity.\n    pub name: String,\n    /// `GUID_DEVINTERFACE_MONITOR` device-interface path. Windows registers\n    /// this per monitor, so it can re-find the same monitor after enumeration\n    /// order changes across restarts.\n    pub stable_id: Option<String>,\n}\n''',
)
insert_before(
    p,
    "pub(crate) fn enumerate_displays() -> Vec<DisplayInfo> {\n",
    '''fn monitor_interface_id(gdi_name: &str) -> Option<String> {\n    let wide: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();\n    let mut device = DISPLAY_DEVICEW {\n        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,\n        ..Default::default()\n    };\n    // EDD_GET_DEVICE_INTERFACE_NAME = 0x1. It asks EnumDisplayDevicesW to\n    // place the GUID_DEVINTERFACE_MONITOR path in DISPLAY_DEVICE.DeviceID.\n    if !unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut device, 0x1) }.as_bool() {\n        return None;\n    }\n    let end = device\n        .DeviceID\n        .iter()\n        .position(|&ch| ch == 0)\n        .unwrap_or(device.DeviceID.len());\n    (end != 0).then(|| String::from_utf16_lossy(&device.DeviceID[..end]))\n}\n\n''',
)
replace(
    p,
    '''        let name_len = info\n            .szDevice\n            .iter()\n            .position(|&c| c == 0)\n            .unwrap_or(info.szDevice.len());\n        displays.push(DisplayInfo {\n            handle: monitor,\n            work: info.monitorInfo.rcWork,\n            monitor: info.monitorInfo.rcMonitor,\n            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,\n            name: String::from_utf16_lossy(&info.szDevice[..name_len]),\n        });\n''',
    '''        let name_len = info\n            .szDevice\n            .iter()\n            .position(|&c| c == 0)\n            .unwrap_or(info.szDevice.len());\n        let name = String::from_utf16_lossy(&info.szDevice[..name_len]);\n        let stable_id = monitor_interface_id(&name);\n        displays.push(DisplayInfo {\n            handle: monitor,\n            work: info.monitorInfo.rcWork,\n            monitor: info.monitorInfo.rcMonitor,\n            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,\n            name,\n            stable_id,\n        });\n''',
)
text = Path(p).read_text(encoding="utf-8")
old_start = text.index("/// Remembered device names for `MonitorMode::Index(n)` picks.")
old_end = text.index("/// Warns about a configured-but-unattached display index", old_start)
replacement = '''/// Resolves a monitor selection, preferring the persisted per-monitor identity\n/// when it belongs to the current explicit index. A known-but-missing monitor\n/// falls back to primary rather than letting the same numeric index silently\n/// select a different physical display. Configs without identity metadata keep\n/// the legacy index behavior.\npub(crate) fn resolve_target_persisted(\n    mode: MonitorMode,\n    stable_id: Option<&str>,\n    stable_index: Option<u32>,\n    displays: &[DisplayInfo],\n    foreground_nearest: Option<usize>,\n) -> Option<usize> {\n    let MonitorMode::Index(index) = mode else {\n        return resolve_target(mode, displays, foreground_nearest);\n    };\n    if stable_index == Some(index)\n        && let Some(id) = stable_id.filter(|id| !id.is_empty())\n    {\n        if let Some(pos) = displays.iter().position(|display| {\n            display\n                .stable_id\n                .as_deref()\n                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(id))\n        }) {\n            return Some(pos);\n        }\n        if displays.is_empty() {\n            return None;\n        }\n        return Some(displays.iter().position(|display| display.primary).unwrap_or(0));\n    }\n    resolve_target(mode, displays, foreground_nearest)\n}\n\nfn refresh_identity_slot(\n    mode: MonitorMode,\n    stable_id: &mut Option<String>,\n    stable_index: &mut Option<u32>,\n    displays: &[DisplayInfo],\n) -> bool {\n    let MonitorMode::Index(index) = mode else {\n        let changed = stable_id.is_some() || stable_index.is_some();\n        *stable_id = None;\n        *stable_index = None;\n        return changed;\n    };\n\n    // A matching pair is already authoritative. Preserve it while the monitor\n    // is temporarily absent so unplug/replug cannot erase the identity needed\n    // to recognize it when it returns.\n    if *stable_index == Some(index) && stable_id.as_deref().is_some_and(|id| !id.is_empty()) {\n        return false;\n    }\n    let next_id = displays.get(index as usize).and_then(|display| display.stable_id.clone());\n    let changed = *stable_index != Some(index) || *stable_id != next_id;\n    *stable_index = Some(index);\n    *stable_id = next_id;\n    changed\n}\n\n/// Captures managed monitor identities for explicit-index selections. This is\n/// an additive migration: old configs keep working if Windows cannot expose a\n/// device-interface path, and existing remembered identities survive an\n/// unplug/replug cycle.\npub(crate) fn refresh_monitor_identities(config: &mut Config) -> bool {\n    let displays = enumerate_displays_cached();\n    let expanded = refresh_identity_slot(\n        config.overlay.monitor,\n        &mut config.overlay.monitor_device_id,\n        &mut config.overlay.monitor_device_index,\n        &displays,\n    );\n    let compact = refresh_identity_slot(\n        config.overlay.compact_monitor,\n        &mut config.overlay.compact_monitor_device_id,\n        &mut config.overlay.compact_monitor_device_index,\n        &displays,\n    );\n    expanded || compact\n}\n\n#[cfg(test)]\nmod persisted_monitor_tests {\n    use super::*;\n    use std::ffi::c_void;\n\n    fn display(handle: usize, primary: bool, id: Option<&str>) -> DisplayInfo {\n        DisplayInfo {\n            handle: HMONITOR(handle as *mut c_void),\n            work: RECT::default(),\n            monitor: RECT::default(),\n            primary,\n            name: format!("display-{handle}"),\n            stable_id: id.map(str::to_string),\n        }\n    }\n\n    #[test]\n    fn persisted_identity_survives_enumeration_reorder() {\n        let displays = vec![\n            display(1, true, Some("monitor-b")),\n            display(2, false, Some("monitor-a")),\n        ];\n        assert_eq!(\n            resolve_target_persisted(MonitorMode::Index(0), Some("monitor-a"), Some(0), &displays, None),\n            Some(1)\n        );\n    }\n\n    #[test]\n    fn missing_known_monitor_uses_primary_without_forgetting_identity() {\n        let displays = vec![display(1, true, Some("monitor-b")), display(2, false, Some("monitor-c"))];\n        assert_eq!(\n            resolve_target_persisted(MonitorMode::Index(1), Some("monitor-a"), Some(1), &displays, None),\n            Some(0)\n        );\n        let mut id = Some("monitor-a".to_string());\n        let mut saved_index = Some(1);\n        assert!(!refresh_identity_slot(\n            MonitorMode::Index(1),\n            &mut id,\n            &mut saved_index,\n            &displays\n        ));\n        assert_eq!(id.as_deref(), Some("monitor-a"));\n    }\n\n    #[test]\n    fn manual_index_change_rebinds_managed_identity() {\n        let displays = vec![display(1, true, Some("monitor-a")), display(2, false, Some("monitor-b"))];\n        let mut id = Some("monitor-a".to_string());\n        let mut saved_index = Some(0);\n        assert!(refresh_identity_slot(\n            MonitorMode::Index(1),\n            &mut id,\n            &mut saved_index,\n            &displays\n        ));\n        assert_eq!(saved_index, Some(1));\n        assert_eq!(id.as_deref(), Some("monitor-b"));\n    }\n}\n\n'''
Path(p).write_text(text[:old_start] + replacement + text[old_end:], encoding="utf-8")

# --- Resolve identity for expanded vs independent compact placement ----------
p = "src/overlay/mod.rs"
replace(
    p,
    "pub(crate) use fullscreen::{enumerate_displays_cached, invalidate_display_cache};\n",
    "pub(crate) use fullscreen::{enumerate_displays_cached, invalidate_display_cache, refresh_monitor_identities};\n",
)
replace(p, "refresh_period_ms, resolve_target_sticky, window_is_fullscreen,\n", "refresh_period_ms, resolve_target_persisted, window_is_fullscreen,\n")
replace(
    p,
    '''    resolve_target_sticky(position.monitor, &displays, foreground_nearest)\n        .map(|index| monitor_dpi(displays[index].handle))\n        .unwrap_or(96)\n''',
    '''    let (stable_id, stable_index) = if compact && state.config.overlay.compact_position_separate {\n        (\n            state.config.overlay.compact_monitor_device_id.as_deref(),\n            state.config.overlay.compact_monitor_device_index,\n        )\n    } else {\n        (\n            state.config.overlay.monitor_device_id.as_deref(),\n            state.config.overlay.monitor_device_index,\n        )\n    };\n    resolve_target_persisted(position.monitor, stable_id, stable_index, &displays, foreground_nearest)\n        .map(|index| monitor_dpi(displays[index].handle))\n        .unwrap_or(96)\n''',
)
replace(
    p,
    '''        let index = resolve_target_sticky(self.active_pos().monitor, displays, foreground_nearest)?;\n        let display = &displays[index];\n''',
    '''        let compact_slot = self.effective_compact() && self.config.overlay.compact_position_separate;\n        let (stable_id, stable_index) = if compact_slot {\n            (\n                self.config.overlay.compact_monitor_device_id.as_deref(),\n                self.config.overlay.compact_monitor_device_index,\n            )\n        } else {\n            (\n                self.config.overlay.monitor_device_id.as_deref(),\n                self.config.overlay.monitor_device_index,\n            )\n        };\n        let index = resolve_target_persisted(\n            self.active_pos().monitor,\n            stable_id,\n            stable_index,\n            displays,\n            foreground_nearest,\n        )?;\n        let display = &displays[index];\n''',
)
replace(
    p,
    '''            primary,\n            name: format!("display-{handle}"),\n        }\n''',
    '''            primary,\n            name: format!("display-{handle}"),\n            stable_id: Some(format!("monitor-{handle}")),\n        }\n''',
)

# Main-window config mutations refresh the managed identity before saving.
p = "src/main_window.rs"
replace(
    p,
    '''            mutate(&mut cfg);\n            cfg.clone()\n''',
    '''            mutate(&mut cfg);\n            crate::overlay::refresh_monitor_identities(&mut cfg);\n            cfg.clone()\n''',
)
replace(
    p,
    '''                    cfg.overlay.compact_position_y = cfg.overlay.position_y;\n                    cfg.overlay.compact_monitor = cfg.overlay.monitor;\n''',
    '''                    cfg.overlay.compact_position_y = cfg.overlay.position_y;\n                    cfg.overlay.compact_monitor = cfg.overlay.monitor;\n                    cfg.overlay.compact_monitor_device_id = cfg.overlay.monitor_device_id.clone();\n                    cfg.overlay.compact_monitor_device_index = cfg.overlay.monitor_device_index;\n''',
)

# Startup performs the additive best-effort migration after DPI awareness is set.
p = "src/main.rs"
replace(
    p,
    '''    unsafe {\n        if let Err(error) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {\n            warn!("per-monitor DPI awareness unavailable: {error}");\n        }\n    }\n\n    let (event_tx, event_rx) = mpsc::sync_channel::<Arc<MediaEvent>>(EVENT_CHANNEL_CAP);\n''',
    '''    unsafe {\n        if let Err(error) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {\n            warn!("per-monitor DPI awareness unavailable: {error}");\n        }\n    }\n    if overlay::refresh_monitor_identities(&mut config) {\n        match config.save_checked() {\n            Ok(config::SaveOutcome::Saved(revision)) => {\n                config.revision = Some(revision);\n                debug!("captured stable monitor identity metadata");\n            }\n            Ok(config::SaveOutcome::Conflict) => {\n                warn!("config changed while capturing monitor identity; keeping the in-memory identity only");\n            }\n            Ok(config::SaveOutcome::PersistenceDisabled) => {}\n            Err(error) => warn!("could not persist monitor identity metadata: {error:#}"),\n        }\n    }\n\n    let (event_tx, event_rx) = mpsc::sync_channel::<Arc<MediaEvent>>(EVENT_CHANNEL_CAP);\n''',
)

# --- User-facing config docs -------------------------------------------------
p = "config.example.toml"
replace(
    p,
    '''# window's monitor, default) | "primary" | "index-N" (the (N+1)-th display\n# in Windows' enumeration order).\nmonitor = "active-window"\n''',
    '''# window's monitor, default) | "primary" | "index-N". For index picks,\n# WinGlance records managed monitor_device_* metadata so the same monitor is\n# preferred after restart even when Windows reorders display enumeration.\nmonitor = "active-window"\n# monitor_device_id / monitor_device_index are managed by WinGlance; omit them.\n''',
)
replace(
    p,
    "compact_monitor = \"active-window\"\n",
    "compact_monitor = \"active-window\"\n# compact_monitor_device_id / compact_monitor_device_index are managed by WinGlance.\n",
)

p = "docs/configuration.md"
replace(
    p,
    '''| `monitor`      | `"active-window"` | string | Which display the pill is placed on (see below) |\n''',
    '''| `monitor`      | `"active-window"` | string | Which display the pill is placed on (see below) |\n| `monitor_device_id` / `monitor_device_index` | *(managed)* | string / integer | WinGlance-managed identity metadata for an explicit `index-N` selection; normally omit these keys |\n''',
)
replace(
    p,
    '''| `compact_monitor` | `"active-window"` | string | Which display the Compact layout is placed on     |\n''',
    '''| `compact_monitor` | `"active-window"` | string | Which display the Compact layout is placed on     |\n| `compact_monitor_device_id` / `compact_monitor_device_index` | *(managed)* | string / integer | Managed identity metadata for the independent Compact monitor slot |\n''',
)
replace(
    p,
    '''The index is resolved against the *current* display layout each time the pill\nis placed. If the configured display is temporarily unplugged or reordered,\nthe pill falls back to the primary display while the setting stays untouched,\nso it reapplies automatically when the display returns. Custom\n''',
    '''On the first run where an `index-N` selection can be resolved, WinGlance\nrecords Windows' per-monitor device-interface path beside the numeric index.\nFuture runs prefer that identity, so a display-enumeration reorder does not\nsilently move the pill to a different physical monitor. The numeric index stays\nas the backward-compatible fallback for old configs or systems where Windows\ndoes not expose an interface path. If a remembered monitor is temporarily\nunplugged, the pill uses the primary display without erasing the identity; when\nthe monitor returns it is selected again even at a different enumeration\nposition. The `*_monitor_device_*` keys are managed metadata and normally should\nnot be hand-edited. Custom\n''',
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/config.rs", "src/overlay/fullscreen.rs", "src/overlay/mod.rs", "src/main_window.rs", "src/main.rs", "config.example.toml", "docs/configuration.md"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "fix(monitor): persist stable display identity"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
