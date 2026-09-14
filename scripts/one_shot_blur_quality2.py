from pathlib import Path


def replace_once(path, old, new):
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"{path}: marker missing: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_range(path, start, end, new):
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    a = text.find(start)
    if a < 0:
        raise SystemExit(f"{path}: start marker missing: {start!r}")
    b = text.find(end, a)
    if b < 0:
        raise SystemExit(f"{path}: end marker missing: {end!r}")
    p.write_text(text[:a] + new + text[b:], encoding="utf-8")


# Backdrop::sync is already a no-op unless the material is active. Avoid a new
# decision point in the grandfathered render_layered hot function.
p = Path("src/overlay/render.rs")
text = p.read_text(encoding="utf-8")
old = """    if state.backdrop.active() {
        state.backdrop.sync(
            state.hwnd,
            position.x + inset,
            position.y + inset,
            width,
            height,
        );
    }
"""
if old in text:
    text = text.replace(
        old,
        """    state.backdrop.sync(
        state.hwnd,
        position.x + inset,
        position.y + inset,
        width,
        height,
    );
""",
        1,
    )
    p.write_text(text, encoding="utf-8")
else:
    print("render_layered backdrop sync wrapper not found; leaving renderer unchanged")

helper = '''fn visual_toggle_row(
    id: SettingId,
    fade_persistent_pill: bool,
    glass_effect: bool,
    accent: [u8; 4],
    faint: [u8; 4],
) -> (&'static str, String, [u8; 4]) {
    match id {
        SettingId::GlassEffect => (
            "Windows 11 glass effect",
            if glass_effect { "ON" } else { "OFF" }.to_string(),
            if glass_effect { accent } else { faint },
        ),
        _ => (
            "Fade Persistent Compact Pill after duration",
            if fade_persistent_pill { "Yes" } else { "No" }.to_string(),
            if fade_persistent_pill { accent } else { faint },
        ),
    }
}

fn visual_toggle_action(id: SettingId) -> SettingAction {
    if id == SettingId::GlassEffect {
        SettingAction::ToggleGlassEffect
    } else {
        SettingAction::ToggleFadePersistentPill
    }
}

fn visual_setting_label(id: SettingId) -> &'static str {
    if id == SettingId::GlassEffect {
        "Windows 11 glass effect"
    } else {
        "Fade Persistent Compact Pill after duration"
    }
}

fn visual_setting_value(id: SettingId, cfg: &Config) -> String {
    if id == SettingId::GlassEffect {
        on_off(cfg.overlay.glass_effect)
    } else if cfg.overlay.fade_persistent_pill {
        "Yes".into()
    } else {
        "No".into()
    }
}

fn perform_visual_toggle(state: &mut MainWindowState, action: SettingAction) {
    match action {
        SettingAction::ToggleGlassEffect => {
            let new_value = !state.cfg().overlay.glass_effect;
            state.mutate_config(|cfg| cfg.overlay.glass_effect = new_value);
            set_glass_effect(state.overlay_hwnd, new_value);
            info!("glass effect set: {new_value}");
        }
        _ => {
            let new_value = !state.cfg().overlay.fade_persistent_pill;
            state.mutate_config(|cfg| cfg.overlay.fade_persistent_pill = new_value);
            set_fade_persistent_pill(state.overlay_hwnd, new_value);
            info!("fade persistent pill set: {new_value}");
        }
    }
}

'''
replace_once(
    "src/main_window.rs",
    "/// Pure Settings-pane hit test shared by the mouse path. Row indexing counts\n",
    helper + "/// Pure Settings-pane hit test shared by the mouse path. Row indexing counts\n",
)

# Paint-settings: Glass + Fade are adjacent and Pinned follows them. Keep one
# outer match arm (the same count as the original Fade-only implementation).
replace_range(
    "src/main_window.rs",
    "                        SettingId::GlassEffect => (",
    "                        SettingId::PinnedSource =>",
    '''                        SettingId::GlassEffect | SettingId::FadePersistentPill => visual_toggle_row(
                            *id,
                            fade_persistent_pill,
                            glass_effect,
                            accent,
                            colors.faint,
                        ),
''',
)

replace_once(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => Some(SettingAction::ToggleFadePersistentPill),\n        SettingId::GlassEffect => Some(SettingAction::ToggleGlassEffect),\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => Some(visual_toggle_action(id)),\n",
)
replace_once(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => \"Fade Persistent Compact Pill after duration\",\n        SettingId::GlassEffect => \"Windows 11 glass effect\",\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => visual_setting_label(id),\n",
)
replace_range(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => if cfg.overlay.fade_persistent_pill",
    "        // No pin is spelled out",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => visual_setting_value(id, cfg),\n",
)
replace_range(
    "src/main_window.rs",
    "        SettingAction::ToggleGlassEffect => {",
    "        SettingAction::ToggleSeparateCompact => {",
    '''        action @ (SettingAction::ToggleGlassEffect | SettingAction::ToggleFadePersistentPill) => {
            perform_visual_toggle(state, action);
            state.invalidate();
        }
''',
)

replace_once(
    "docs/configuration.md",
    "| `expand_compact_on_hover` | `true` | bool | Hovering a pill in the Compact layout expands it in place; with `dismiss_on_hover` on, the second hover dismisses (see below) |\n| `fade_persistent_pill`",
    "| `expand_compact_on_hover` | `true` | bool | Hovering a pill in the Compact layout expands it in place; with `dismiss_on_hover` on, the second hover dismisses (see below) |\n"
    "| `glass_effect` | `false` | bool | Opt in to the Windows 11 Desktop Acrylic material behind the pill body. The existing layered renderer still draws content and the aura. Unsupported DWM backdrops, High Contrast, or the system reduced-overlap preference fall back to the existing solid material |\n"
    "| `fade_persistent_pill`",
)

print("blur quality refactor v2 applied")
