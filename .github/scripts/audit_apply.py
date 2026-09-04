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

p = "src/main_window.rs"
insert_before(
    p,
    "#[derive(Debug, Clone)]\nstruct HistoryEntry {\n",
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nenum HistoryDisposition {\n    /// This event actually reached the notification pill.\n    Shown,\n    /// Notifications were disabled when the event arrived.\n    NotificationsPaused,\n    /// The source repeated a state already represented by the pill.\n    Redundant,\n    /// The source/session was deliberately not tracked (allow-list or churn protection).\n    Rejected,\n    /// WinGlance's media worker stopped after exhausting its restart budget.\n    InternalFailure,\n}\n\nimpl HistoryDisposition {\n    fn shown(self) -> bool {\n        matches!(self, Self::Shown)\n    }\n\n    fn detail(self) -> Option<&'static str> {\n        match self {\n            Self::Shown => None,\n            Self::NotificationsPaused => Some("notifications paused"),\n            Self::Redundant => Some("redundant media update"),\n            Self::Rejected => Some("source not tracked (allowed-app filter or churn protection)"),\n            Self::InternalFailure => Some("internal media worker failure"),\n        }\n    }\n}\n\n''',
)
replace(
    p,
    '''    /// Whether the source session passed the `media_sources` filter.\n    /// Accepted entries are highlighted; rejected ones render muted so every\n    /// media source is visible in the history.\n    accepted: bool,\n''',
    '''    /// Why this event did or did not reach the notification surface.\n    /// Only `Shown` rows are highlighted; every other disposition stays\n    /// visible but muted with a truthful tooltip reason.\n    disposition: HistoryDisposition,\n''',
)
replace(
    p,
    "    fn push_history(&mut self, track: TrackInfo, state: PlaybackState, accepted: bool) -> u64 {\n",
    "    fn push_history(&mut self, track: TrackInfo, state: PlaybackState, disposition: HistoryDisposition) -> u64 {\n",
)
replace(p, "            accepted,\n        });\n", "            disposition,\n        });\n")
replace(
    p,
    '''        let reached = !redundant && self.cfg().behavior.notifications_enabled;\n        // Convert to the history's text-only form before the clone so the\n''',
    '''        let disposition = if redundant {\n            HistoryDisposition::Redundant\n        } else if self.cfg().behavior.notifications_enabled {\n            HistoryDisposition::Shown\n        } else {\n            HistoryDisposition::NotificationsPaused\n        };\n        // Convert to the history's text-only form before the clone so the\n''',
)
replace(p, "        self.push_history(track, state, reached);\n", "        self.push_history(track, state, disposition);\n")
replace(
    p,
    '''        state: PlaybackState,\n        accepted: bool,\n    ) {\n''',
    '''        state: PlaybackState,\n        _accepted: bool,\n    ) {\n''',
)
replace(p, "        self.push_history(track, state, accepted);\n", "        self.push_history(track, state, HistoryDisposition::Rejected);\n")
replace(
    p,
    "        self.push_history(track, PlaybackState::Stopped, false);\n",
    "        self.push_history(track, PlaybackState::Stopped, HistoryDisposition::InternalFailure);\n",
)
replace(
    p,
    '''        let reached = self.cfg().behavior.notifications_enabled;\n        let history_track = track.clone().into_history_text();\n        self.current_entry_id = Some(self.push_history(history_track, state, reached));\n''',
    '''        let disposition = if self.cfg().behavior.notifications_enabled {\n            HistoryDisposition::Shown\n        } else {\n            HistoryDisposition::NotificationsPaused\n        };\n        let history_track = track.clone().into_history_text();\n        self.current_entry_id = Some(self.push_history(history_track, state, disposition));\n''',
)
replace(p, "            let (row_color, bold) = if entry.accepted {\n", "            let (row_color, bold) = if entry.disposition.shown() {\n")
replace(
    p,
    '''    if !entry.accepted {\n        parts.push("(filtered by allowed apps)".to_string());\n    }\n''',
    '''    if let Some(reason) = entry.disposition.detail() {\n        parts.push(format!("({reason})"));\n    }\n''',
)
# SessionRejected remains a transport shape for now; the event kind itself is
# the truthful reason in history. The legacy boolean is deliberately ignored.
replace(
    p,
    '''                    accepted,\n                } => self.add_session(source_app, title, artist, state, accepted),\n''',
    '''                    accepted,\n                } => self.add_session(source_app, title, artist, state, accepted),\n''',
)

# Preserve signed 16-bit screen coordinates for TTM_TRACKPOSITION. Virtual
# desktops commonly place monitors left/above the primary, producing negative
# coordinates that must survive the LPARAM packing.
insert_before(
    p,
    "const fn colorref(r: u8, g: u8, b: u8) -> COLORREF {\n",
    '''fn pack_signed_point_lparam(x: i32, y: i32) -> isize {\n    let x = x.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16 as u32;\n    let y = y.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16 as u32;\n    (x | (y << 16)) as isize\n}\n\n#[cfg(test)]\nmod tooltip_coordinate_tests {\n    use super::pack_signed_point_lparam;\n\n    #[test]\n    fn track_position_packing_preserves_negative_virtual_desktop_coordinates() {\n        let packed = pack_signed_point_lparam(-1200, -300) as u32;\n        assert_eq!((packed as u16) as i16, -1200);\n        assert_eq!(((packed >> 16) as u16) as i16, -300);\n    }\n\n    #[test]\n    fn track_position_packing_clamps_only_beyond_message_range() {\n        let packed = pack_signed_point_lparam(i32::MIN, i32::MAX) as u32;\n        assert_eq!((packed as u16) as i16, i16::MIN);\n        assert_eq!(((packed >> 16) as u16) as i16, i16::MAX);\n    }\n}\n\n''',
)
replace(
    p,
    "            let packed = ((clamped.top.max(0) as isize) << 16) | (clamped.left.max(0) as isize & 0xFFFF);\n",
    "            let packed = pack_signed_point_lparam(clamped.left, clamped.top);\n",
)

# Clarify the retained transport field so future code does not repeat the old
# history mistake.
p = "src/events.rs"
replace(
    p,
    '''    /// sources are visible; `accepted` marks entries whose state reached the\n    /// pill — a tracked source's redundant re-report records grey.\n''',
    '''    /// sources are visible. `accepted` is retained for backward-compatible\n    /// transport/tests; the main window derives its history disposition from\n    /// the event kind and current notification state instead of treating this\n    /// boolean as a user-facing reason.\n''',
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", "src/main_window.rs", "src/events.rs"], check=True)
subprocess.run(["git", "commit", "-m", "fix(history): record truthful dispositions and signed tooltip positions"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
