from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:160]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def insert_after(path: str, marker: str, addition: str) -> None:
    replace(path, marker, marker + addition)

# Overlay-local status only: do not add an Idle transport event that could leak
# into SMTC, history, or main-window semantics.
replace(
    "src/overlay/mod.rs",
    "    content: Option<MediaEvent>,\n    /// The identity-stable palette",
    "    content: Option<MediaEvent>,\n    /// True only for the overlay-local no-media status card. The backing\n    /// `MediaEvent` is a private render sentinel and never enters the event\n    /// transport or history; this flag keeps status semantics out of SMTC.\n    idle_content: bool,\n    /// The identity-stable palette",
)
replace(
    "src/overlay/mod.rs",
    "            content: None,\n            content_palette: None,",
    "            content: None,\n            idle_content: false,\n            content_palette: None,",
)

insert_after(
    "src/overlay/mod.rs",
    "    fn reset_scroll(&mut self) {\n",
    "",
)
# Insert helpers immediately before reset_scroll for a compact, isolated state machine.
replace(
    "src/overlay/mod.rs",
    "    fn reset_scroll(&mut self) {\n",
    '''    fn idle_event() -> MediaEvent {\n        MediaEvent::TrackChanged(TrackInfo {\n            title: "No media playing".into(),\n            ..Default::default()\n        })\n    }\n\n    /// Shows the process-lifetime no-media status. It is deliberately static:\n    /// no dismiss deadline, progress, comet, marquee morph, or hover action, so\n    /// after the one initial layered-window upload the normal render gate stays\n    /// cold until a real event or placement/configuration change arrives.\n    fn show_idle(&mut self) {\n        unsafe {\n            let _ = kill_timer(self.hwnd, IDLE_BUFFER_TIMER_ID);\n        }\n        self.idle_content = true;\n        self.hidden_watchdog = false;\n        self.held_content = None;\n        self.current_source = None;\n        self.progress_anchor = None;\n        self.estimated_position_secs = None;\n        self.progress_duration_secs = None;\n        self.progress_rate = None;\n        self.progress_playing = false;\n        self.last_bar_fraction = None;\n        self.progress_track_key = None;\n        self.content_rev += 1;\n        self.content = Some(Self::idle_event());\n        self.content_palette = None;\n        self.palette = None;\n        self.layout = LayoutMode::Compact;\n        self.dismiss_at = None;\n        self.hover_dismiss_at = None;\n        self.hover_expand = None;\n        self.hover_expanded_once = false;\n        self.hover_leave_at = None;\n        self.persistent_faded = false;\n        self.persistent_collapse_on_dismiss = false;\n        self.content_fade = None;\n        self.resolve_pill_text();\n        self.reset_scroll();\n        self.phase = Phase::Shown;\n        self.sync_anim_timer();\n        unsafe {\n            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);\n        }\n        self.render();\n    }\n\n    fn reset_scroll(&mut self) {\n''',
)

# A real media update takes ownership of the surface immediately instead of
# queueing behind an idle card with no deadline.
replace(
    "src/overlay/mod.rs",
    '''                MediaEvent::TrackChanged(track) if self.config.behavior.enable_track_change => {\n                    // A metadata refresh for the track currently on screen\n''',
    '''                MediaEvent::TrackChanged(track) if self.config.behavior.enable_track_change => {\n                    if self.idle_content {\n                        self.current_source = Some(track.source_app.clone());\n                        self.last_track = Some(track.clone());\n                        self.cache_track(&track);\n                        self.show(MediaEvent::TrackChanged(track), true);\n                        continue;\n                    }\n                    // A metadata refresh for the track currently on screen\n''',
)
replace(
    "src/overlay/mod.rs",
    '''                MediaEvent::PlaybackStateChanged(state, source_app)\n                    if self.config.behavior.enable_playback_state_change =>\n                {\n                    // Persistent-compact: a Stopped that does not belong to\n''',
    '''                MediaEvent::PlaybackStateChanged(state, source_app)\n                    if self.config.behavior.enable_playback_state_change =>\n                {\n                    if self.idle_content {\n                        // A terminal state with no active content is still no\n                        // media; playing/paused state establishes real content\n                        // and therefore replaces the status immediately.\n                        if state != PlaybackState::Stopped {\n                            self.show(MediaEvent::PlaybackStateChanged(state, source_app), false);\n                        }\n                        continue;\n                    }\n                    // Persistent-compact: a Stopped that does not belong to\n''',
)

# Every actual show/refresh/preview exits idle before any layout decision.
replace(
    "src/overlay/mod.rs",
    '''    fn update_content(&mut self, event: MediaEvent, min_visible: Duration) {\n        // PersistentCompact auto-hide:''',
    '''    fn update_content(&mut self, event: MediaEvent, min_visible: Duration) {\n        self.idle_content = false;\n        // PersistentCompact auto-hide:''',
)
replace(
    "src/overlay/mod.rs",
    '''    fn show_with_duration(&mut self, event: MediaEvent, full_animation: bool, duration_ms: u64) {\n        if !self.enabled {\n            return;\n        }\n''',
    '''    fn show_with_duration(&mut self, event: MediaEvent, full_animation: bool, duration_ms: u64) {\n        if !self.enabled {\n            return;\n        }\n        self.idle_content = false;\n''',
)
replace(
    "src/overlay/mod.rs",
    '''    fn show_sample(&mut self) {\n        debug!("sample pill shown");\n''',
    '''    fn show_sample(&mut self) {\n        debug!("sample pill shown");\n        self.idle_content = false;\n''',
)

# Idle has no source identity and must never suppress a real worker re-report.
replace(
    "src/overlay/mod.rs",
    '''        let source = match &self.content {\n            Some(MediaEvent::TrackChanged(track)) => Some(track.source_app.clone()),\n''',
    '''        let source = if self.idle_content {\n            None\n        } else {\n            match &self.content {\n            Some(MediaEvent::TrackChanged(track)) => Some(track.source_app.clone()),\n''',
)
replace(
    "src/overlay/mod.rs",
    '''            _ => None,\n        };\n        // Always write, clearing on an empty source:''',
    '''            _ => None,\n            }\n        };\n        // Always write, clearing on an empty source:''',
)

# The idle status is compact and noninteractive regardless of configured media layout.
replace(
    "src/overlay/mod.rs",
    '''    fn refresh_layout(&mut self) {\n        let verdict = self.sample_foreground();\n''',
    '''    fn refresh_layout(&mut self) {\n        if self.idle_content {\n            self.layout = LayoutMode::Compact;\n            return;\n        }\n        let verdict = self.sample_foreground();\n''',
)
replace(
    "src/overlay/mod.rs",
    '''    fn content_playing(&self) -> bool {\n        match &self.content {\n''',
    '''    fn content_playing(&self) -> bool {\n        if self.idle_content {\n            return false;\n        }\n        match &self.content {\n''',
)
replace(
    "src/overlay/mod.rs",
    '''        let cursor_over = if self.config.overlay.dismiss_on_hover\n            || self.config.overlay.expand_compact_on_hover\n            || self.config.overlay.layout == LayoutMode::PersistentCompact\n        {\n''',
    '''        let cursor_over = if !self.idle_content\n            && (self.config.overlay.dismiss_on_hover\n                || self.config.overlay.expand_compact_on_hover\n                || self.config.overlay.layout == LayoutMode::PersistentCompact)\n        {\n''',
)

# Status naming is readable but does not masquerade as a new-track announcement.
replace(
    "src/overlay/mod.rs",
    '''            if changed && Self::announces_pill_name_change(&self.content) {\n''',
    '''            if changed && !self.idle_content && Self::announces_pill_name_change(&self.content) {\n''',
)

# Normal terminal hides return to idle. The existing persistent fullscreen/listed
# hold remains the one deliberate hidden state while real content is retained.
replace(
    "src/overlay/mod.rs",
    '''        // Advance the queue: the next pending notification shows as a fresh\n        // pill. show() checks `enabled`, so a toggle-off collapse stays hidden.\n        self.show_next();\n        // Auto-hide watchdog:''',
    '''        // Advance the queue: the next pending notification shows as a fresh\n        // pill. show() checks `enabled`, so a toggle-off collapse cannot show\n        // media, but the no-media status remains available below.\n        self.show_next();\n        if matches!(self.phase, Phase::Hidden) && self.held_content.is_none() {\n            self.show_idle();\n            return;\n        }\n        // Auto-hide watchdog:''',
)

# Settings changes should still preview a real notification over the idle status.
replace(
    "src/overlay/mod.rs",
    '''    fn preview_if_hidden(&mut self) -> bool {\n        if matches!(self.phase, Phase::Hidden) {\n''',
    '''    fn preview_if_hidden(&mut self) -> bool {\n        if matches!(self.phase, Phase::Hidden) || self.idle_content {\n''',
)

# Create the idle status only after the HWND is fully claimed and the foreground
# hook setup is complete, so its render/position paths see a normal live window.
replace(
    "src/overlay/mod.rs",
    '''            } else {\n                unsafe {\n                    (*state_ptr).hook = Some(hook);\n                }\n            }\n            Ok(hwnd)\n''',
    '''            } else {\n                unsafe {\n                    (*state_ptr).hook = Some(hook);\n                }\n            }\n            unsafe {\n                (*state_ptr).show_idle();\n            }\n            Ok(hwnd)\n''',
)

# The compact idle card uses the neutral placeholder and title, but no playback glyph.
replace(
    "src/overlay/render.rs",
    '''    if layer != RenderLayer::Foreground {\n        draw_symbol_pixels(\n            pixels,\n            width as usize,\n            symbol_right,\n            symbol_y,\n            symbol as f32,\n            playback,\n            playback_type,\n            accent,\n        );\n    }\n}\n\n/// Draws one morph frame's content:''',
    '''    if layer != RenderLayer::Foreground && !state.idle_content {\n        draw_symbol_pixels(\n            pixels,\n            width as usize,\n            symbol_right,\n            symbol_y,\n            symbol as f32,\n            playback,\n            playback_type,\n            accent,\n        );\n    }\n}\n\n/// Draws one morph frame's content:''',
)

# Regression tests for the pure/status invariants; no real HWND is launched.
replace(
    "src/overlay/mod.rs",
    '''    #[test]\n    fn needs_font_rebuild_is_true_only_when_the_target_dpi_differs() {\n''',
    '''    #[test]\n    fn idle_status_is_static_compact_and_nonplaying() {\n        let mut state = OverlayState::new(Config::default(), EventQueue::default());\n        state.idle_content = true;\n        state.content = Some(OverlayState::idle_event());\n        state.phase = Phase::Shown;\n        state.layout = LayoutMode::Compact;\n        state.dismiss_at = None;\n        assert!(state.idle_content);\n        assert!(matches!(state.phase, Phase::Shown));\n        assert_eq!(state.layout, LayoutMode::Compact);\n        assert!(state.dismiss_at.is_none());\n        assert!(!state.content_playing());\n        assert!(!state.orbiting());\n    }\n\n    #[test]\n    fn idle_status_has_truthful_accessible_name_and_no_source_identity() {\n        let mut state = OverlayState::new(Config::default(), EventQueue::default());\n        state.idle_content = true;\n        state.content = Some(OverlayState::idle_event());\n        state.pill_name = Some(Arc::new(Mutex::new(None)));\n        state.resolve_pill_text();\n        assert_eq!(\n            state.pill_name.as_ref().unwrap().lock().unwrap().as_deref(),\n            Some("No media playing")\n        );\n        assert!(!OverlayState::announces_pill_name_change(&None));\n    }\n\n    #[test]\n    fn needs_font_rebuild_is_true_only_when_the_target_dpi_differs() {\n''',
)

replace(
    "README.md",
    '''layout, position, monitor, preferred source, logs).\n\n### Tray menu\n''',
    '''layout, position, monitor, preferred source, logs). While no media is\nactive, the passive compact pill remains visible as **No media playing**; a real\nmedia notification temporarily replaces it and the idle status returns after the\nnotification settles. The idle pill is static, so it does not continuously\nrerender or animate.\n\n### Tray menu\n''',
)

# Architecture documentation: status is overlay-local and not an event-stream concern.
replace(
    "docs/architecture.md",
    "## Overlay\n",
    "## Overlay\n\nThe overlay owns one local no-media status state that is deliberately **not** a `MediaEvent`. On startup and after the last notification settles it renders a compact, passive `No media playing` pill with no dismiss deadline, progress animation, comet, or hover action. Real SMTC events replace that status immediately. Keeping this state local prevents synthetic status from entering history, deduplication, source-ledger, or worker transport semantics.\n\n",
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs", "src/overlay/render.rs", "README.md", "docs/architecture.md"], check=True)
subprocess.run(["git", "commit", "-m", "feat(overlay): keep a truthful no-media idle pill visible"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
