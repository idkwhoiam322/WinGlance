from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:180]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

replace(
    "src/overlay/mod.rs",
    '''    fn idle_event() -> MediaEvent {\n        MediaEvent::TrackChanged(TrackInfo {\n            title: "No media playing".into(),\n            ..Default::default()\n        })\n    }\n''',
    '''    fn idle_status_title(&self) -> &'static str {\n        if !self.enabled\n            || (!self.config.behavior.enable_track_change\n                && !self.config.behavior.enable_playback_state_change)\n        {\n            "Notifications paused"\n        } else {\n            "No media playing"\n        }\n    }\n\n    fn idle_event(&self) -> MediaEvent {\n        MediaEvent::TrackChanged(TrackInfo {\n            title: self.idle_status_title().into(),\n            ..Default::default()\n        })\n    }\n''',
)
replace(
    "src/overlay/mod.rs",
    "        self.content = Some(Self::idle_event());\n",
    "        self.content = Some(self.idle_event());\n",
)
replace(
    "src/overlay/mod.rs",
    '''            } else if let Some(track) = self.last_track.clone() {\n                self.show(MediaEvent::TrackChanged(track), true);\n            }\n            // If none is available, the worker's re-show read surfaces the\n            // current track through the normal receive_events path.\n''',
    '''            } else if let Some(track) = self.last_track.clone() {\n                self.show(MediaEvent::TrackChanged(track), true);\n            } else {\n                // No restoreable live content: refresh the passive card so\n                // the just-enabled state says "No media playing" instead of\n                // retaining the disabled-state "Notifications paused" label.\n                self.show_idle();\n            }\n            // If no live content was available, the worker's re-show read\n            // will replace the passive card through the normal event path.\n''',
)
replace(
    "src/overlay/mod.rs",
    '''        state.idle_content = true;\n        state.content = Some(OverlayState::idle_event());\n        state.phase = Phase::Shown;\n''',
    '''        state.idle_content = true;\n        state.content = Some(state.idle_event());\n        state.phase = Phase::Shown;\n''',
)
replace(
    "src/overlay/mod.rs",
    '''        state.idle_content = true;\n        state.content = Some(OverlayState::idle_event());\n        state.pill_name = Some(Arc::new(Mutex::new(None)));\n''',
    '''        state.idle_content = true;\n        state.content = Some(state.idle_event());\n        state.pill_name = Some(Arc::new(Mutex::new(None)));\n''',
)
replace(
    "src/overlay/mod.rs",
    '''        assert!(!OverlayState::announces_pill_name_change(&None));\n    }\n\n    #[test]\n    fn needs_font_rebuild_is_true_only_when_the_target_dpi_differs() {\n''',
    '''        assert!(!OverlayState::announces_pill_name_change(&None));\n    }\n\n    #[test]\n    fn idle_status_reports_paused_notifications_truthfully() {\n        let mut state = OverlayState::new(Config::default(), EventQueue::default());\n        state.enabled = false;\n        assert_eq!(state.idle_status_title(), "Notifications paused");\n        state.enabled = true;\n        state.config.behavior.enable_track_change = false;\n        state.config.behavior.enable_playback_state_change = false;\n        assert_eq!(state.idle_status_title(), "Notifications paused");\n        state.config.behavior.enable_track_change = true;\n        assert_eq!(state.idle_status_title(), "No media playing");\n    }\n\n    #[test]\n    fn needs_font_rebuild_is_true_only_when_the_target_dpi_differs() {\n''',
)
replace(
    "README.md",
    '''notification settles. The idle pill is static, so it does not continuously\nrerender or animate.\n''',
    '''notification settles. When notifications are disabled, the same passive card\nreads **Notifications paused** instead of incorrectly claiming that no media is\nplaying. The passive card is static, so it does not continuously rerender or\nanimate.\n''',
)
replace(
    "docs/architecture.md",
    '''The overlay owns one local no-media status state that is deliberately **not** a `MediaEvent`. On startup and after the last notification settles it renders a compact, passive `No media playing` pill with no dismiss deadline, progress animation, comet, or hover action. Real SMTC events replace that status immediately. Keeping this state local prevents synthetic status from entering history, deduplication, source-ledger, or worker transport semantics.\n''',
    '''The overlay owns one local passive status state that is deliberately **not** a `MediaEvent`. On startup and after the last notification settles it renders a compact `No media playing` pill; when notifications are disabled (or both notification event types are disabled) the same card reads `Notifications paused` instead. The status has no dismiss deadline, progress animation, comet, or hover action. Real SMTC events replace the active no-media status immediately. Keeping this state local prevents synthetic status from entering history, deduplication, source-ledger, or worker transport semantics.\n''',
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs", "README.md", "docs/architecture.md"], check=True)
subprocess.run(["git", "commit", "-m", "feat(overlay): keep a truthful no-media idle pill visible"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
