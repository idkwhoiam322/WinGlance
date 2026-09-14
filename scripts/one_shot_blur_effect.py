from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    found = text.count(old)
    if found < count:
        raise SystemExit(f"{path}: expected at least {count} occurrence(s), found {found}: {old[:100]!r}")
    text = text.replace(old, new, count)
    p.write_text(text, encoding="utf-8")


# --- config: opt-in, backwards-compatible, default OFF ---
replace(
    "src/config.rs",
    "    pub fade_persistent_pill: bool,\n    /// Unknown keys under `[overlay]`, preserved across saves.\n",
    "    pub fade_persistent_pill: bool,\n"
    "    /// Optional Windows 11 translucent material behind the pill. The\n"
    "    /// existing layered renderer remains responsible for all content; a\n"
    "    /// companion DWM backdrop is created lazily only while this is true.\n"
    "    /// Unsupported systems and accessibility modes fall back to the\n"
    "    /// existing solid fill. Default: false.\n"
    "    pub glass_effect: bool,\n"
    "    /// Unknown keys under `[overlay]`, preserved across saves.\n",
)
replace(
    "src/config.rs",
    "            fade_persistent_pill: true,\n            unknown: toml::Table::new(),\n",
    "            fade_persistent_pill: true,\n            glass_effect: false,\n            unknown: toml::Table::new(),\n",
)

# --- overlay: companion material lifetime and geometry ---
replace("src/overlay/mod.rs", "mod fullscreen;\nmod morph;\nmod render;\n", "mod backdrop;\nmod fullscreen;\nmod morph;\nmod render;\n")
replace(
    "src/overlay/mod.rs",
    "    render_layer: render::RenderLayer,\n    /// Whether the persistent-compact pill is currently in the faded (idle)\n",
    "    render_layer: render::RenderLayer,\n"
    "    /// Lazily-created DWM material window that sits below the layered pill.\n"
    "    backdrop: backdrop::Backdrop,\n"
    "    /// Whether the persistent-compact pill is currently in the faded (idle)\n",
)
replace(
    "src/overlay/mod.rs",
    "            render_layer: render::RenderLayer::Full,\n            last_tick: Instant::now(),\n",
    "            render_layer: render::RenderLayer::Full,\n            backdrop: backdrop::Backdrop::default(),\n            last_tick: Instant::now(),\n",
)
replace(
    "src/overlay/mod.rs",
    "    fn hide(&mut self) {\n        debug!(\"pill hidden\");\n",
    "    fn hide(&mut self) {\n        self.backdrop.hide();\n        debug!(\"pill hidden\");\n",
)
replace(
    "src/overlay/mod.rs",
    "    fn reposition(&mut self) {\n        if matches!(self.phase, Phase::Hidden) {\n            return;\n        }\n",
    "    fn reposition(&mut self) {\n        if matches!(self.phase, Phase::Hidden) {\n            return;\n        }\n        // The companion backdrop has its own HWND, so the geometry-only fast\n        // path would move just the layered foreground. Re-render while glass\n        // is active; render_layered moves both windows from one placement.\n        if self.backdrop.active() {\n            self.render();\n            return;\n        }\n",
)
replace(
    "src/overlay/mod.rs",
    "    fn reposition_current_upload(&mut self) {\n        if self.last_upload_w <= self.aura_inset * 2 || self.last_upload_h <= self.aura_inset * 2 {\n",
    "    fn reposition_current_upload(&mut self) {\n        if self.backdrop.active() {\n            self.render();\n            return;\n        }\n        if self.last_upload_w <= self.aura_inset * 2 || self.last_upload_h <= self.aura_inset * 2 {\n",
)
# Add the live setting push beside the existing persistent-fade push.
marker = "pub(crate) fn set_fade_persistent_pill(hwnd: HWND, enabled: bool) {"
p = Path("src/overlay/mod.rs")
text = p.read_text(encoding="utf-8")
idx = text.find(marker)
if idx < 0:
    raise SystemExit("set_fade_persistent_pill marker missing")
# Insert before the fade function so no brace matching is required.
insert = '''/// Pushes the optional glass-material toggle to the live overlay. The\n/// renderer keeps ownership of content and input behavior; this only changes\n/// the body material underneath it.\npub(crate) fn set_glass_effect(hwnd: HWND, enabled: bool) {\n    if hwnd.0.is_null() {\n        return;\n    }\n    unsafe {\n        let state_ptr = window_state::<OverlayState>(hwnd);\n        if state_ptr.is_null() {\n            return;\n        }\n        let state = &mut *state_ptr;\n        state.config.overlay.glass_effect = enabled;\n        if !enabled {\n            state.backdrop.hide();\n        }\n        info!("overlay glass_effect set to {enabled}");\n        if !matches!(state.phase, Phase::Hidden | Phase::Collapsing(_)) {\n            state.render();\n        }\n    }\n}\n\n'''
text = text[:idx] + insert + text[idx:]
p.write_text(text, encoding="utf-8")

# Prepare material before painting so the fill only becomes translucent when
# DWM actually accepted the backdrop, then keep the body-only window in the
# same placement as the layered upload.
replace(
    "src/overlay/render.rs",
    ") -> Result<()> {\n    let inset = state.aura_inset;\n",
    ") -> Result<()> {\n    let glass_requested = state.config.overlay.glass_effect;\n    state.backdrop.prepare(glass_requested);\n    let inset = state.aura_inset;\n",
)
replace(
    "src/overlay/render.rs",
    "    // Re-assert topmost on every upload (a foreground fullscreen window can\n",
    "    if state.backdrop.active() {\n"
    "        state.backdrop.sync(\n"
    "            state.hwnd,\n"
    "            position.x + inset,\n"
    "            position.y + inset,\n"
    "            width,\n"
    "            height,\n"
    "        );\n"
    "    }\n"
    "    // Re-assert topmost on every upload (a foreground fullscreen window can\n",
)
replace(
    "src/overlay/render.rs",
    "    match state.palette {\n        Some(palette) => tinted_fill(\n            state.config.appearance.background_color,\n            palette.primary,\n            FILL_TINT_WEIGHT,\n        ),\n        None => state.config.appearance.background_color,\n    }\n}\n",
    "    let mut fill = match state.palette {\n"
    "        Some(palette) => tinted_fill(\n"
    "            state.config.appearance.background_color,\n"
    "            palette.primary,\n"
    "            FILL_TINT_WEIGHT,\n"
    "        ),\n"
    "        None => state.config.appearance.background_color,\n"
    "    };\n"
    "    if state.backdrop.active() {\n"
    "        // Keep enough tint for text contrast while allowing the system\n"
    "        // material to read through. The final alpha is also multiplied by\n"
    "        // the normal frame/persistent-fade alpha later in the pipeline.\n"
    "        fill[3] = fill[3].min(176);\n"
    "    }\n"
    "    fill\n"
    "}\n",
)

# --- Settings UI: one opt-in toggle, with UIA parity ---
replace(
    "src/main_window.rs",
    "    set_expand_compact_on_hover, set_fade_persistent_pill, set_hide_for_auto_compact_sources, set_layout,\n",
    "    set_expand_compact_on_hover, set_fade_persistent_pill, set_glass_effect, set_hide_for_auto_compact_sources, set_layout,\n",
)
replace("src/main_window.rs", "    FadePersistentPill,\n    PinnedSource,\n", "    FadePersistentPill,\n    GlassEffect,\n    PinnedSource,\n")
replace("src/main_window.rs", "    ToggleFadePersistentPill,\n    ToggleSeparateCompact,\n", "    ToggleFadePersistentPill,\n    ToggleGlassEffect,\n    ToggleSeparateCompact,\n")
replace(
    "src/main_window.rs",
    "        let fade_persistent_pill = cfg.overlay.fade_persistent_pill;\n        let display_count",
    "        let fade_persistent_pill = cfg.overlay.fade_persistent_pill;\n        let glass_effect = cfg.overlay.glass_effect;\n        let display_count",
)
# Insert a row immediately before the existing fade row.
replace(
    "src/main_window.rs",
    "        natural.push(SettingsItem::Row {\n            id: SettingId::FadePersistentPill,\n",
    "        natural.push(SettingsItem::Row {\n"
    "            id: SettingId::GlassEffect,\n"
    "            rect: RECT {\n"
    "                left,\n"
    "                top: y,\n"
    "                right,\n"
    "                bottom: y + row_h,\n"
    "            },\n"
    "        });\n"
    "        y += row_h + gap;\n"
    "        natural.push(SettingsItem::Row {\n"
    "            id: SettingId::FadePersistentPill,\n",
)
replace(
    "src/main_window.rs",
    "                        SettingId::FadePersistentPill => (\n",
    "                        SettingId::GlassEffect => (\n"
    "                            \"Windows 11 glass effect\",\n"
    "                            if glass_effect { \"ON\".to_string() } else { \"OFF\".to_string() },\n"
    "                            if glass_effect { accent } else { colors.faint },\n"
    "                        ),\n"
    "                        SettingId::FadePersistentPill => (\n",
)
replace(
    "src/main_window.rs",
    "                        | SettingId::FadePersistentPill\n                        | SettingId::PinnedSource\n",
    "                        | SettingId::FadePersistentPill\n                        | SettingId::GlassEffect\n                        | SettingId::PinnedSource\n",
)
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => Some(SettingAction::ToggleFadePersistentPill),\n",
    "        SettingId::FadePersistentPill => Some(SettingAction::ToggleFadePersistentPill),\n"
    "        SettingId::GlassEffect => Some(SettingAction::ToggleGlassEffect),\n",
)
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => \"Fade Persistent Compact Pill after duration\",\n",
    "        SettingId::FadePersistentPill => \"Fade Persistent Compact Pill after duration\",\n"
    "        SettingId::GlassEffect => \"Windows 11 glass effect\",\n",
)
replace(
    "src/main_window.rs",
    "            | SettingId::FadePersistentPill\n    )\n",
    "            | SettingId::FadePersistentPill\n            | SettingId::GlassEffect\n    )\n",
)
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => if cfg.overlay.fade_persistent_pill { \"Yes\" } else { \"No\" }.into(),\n",
    "        SettingId::FadePersistentPill => if cfg.overlay.fade_persistent_pill { \"Yes\" } else { \"No\" }.into(),\n"
    "        SettingId::GlassEffect => on_off(cfg.overlay.glass_effect),\n",
)
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => cfg.overlay.fade_persistent_pill,\n        _ => false,\n",
    "        SettingId::FadePersistentPill => cfg.overlay.fade_persistent_pill,\n"
    "        SettingId::GlassEffect => cfg.overlay.glass_effect,\n"
    "        _ => false,\n",
)
replace(
    "src/main_window.rs",
    "            SettingId::FadePersistentPill,\n            SettingId::PinnedSource,\n",
    "            SettingId::FadePersistentPill,\n            SettingId::GlassEffect,\n            SettingId::PinnedSource,\n",
)
replace(
    "src/main_window.rs",
    "        SettingAction::ToggleFadePersistentPill => {\n",
    "        SettingAction::ToggleGlassEffect => {\n"
    "            let new_value = !state.cfg().overlay.glass_effect;\n"
    "            state.mutate_config(|cfg| cfg.overlay.glass_effect = new_value);\n"
    "            set_glass_effect(state.overlay_hwnd, new_value);\n"
    "            info!(\"glass effect set: {new_value}\");\n"
    "            state.invalidate();\n"
    "        }\n"
    "        SettingAction::ToggleFadePersistentPill => {\n",
)

# --- example config / user contract ---
replace(
    "config.example.toml",
    "expand_compact_on_hover = true\n# With layout = \"persistent-compact\", fade the pill to idle opacity after\n",
    "expand_compact_on_hover = true\n"
    "# Optional Windows 11 Desktop Acrylic material behind the pill body. Off by\n"
    "# default. High Contrast, the system's reduced-overlap preference, or an\n"
    "# unsupported Windows build automatically keeps the existing solid material.\n"
    "glass_effect = false\n"
    "# With layout = \"persistent-compact\", fade the pill to idle opacity after\n",
)

print("blur-effect source patches applied")
