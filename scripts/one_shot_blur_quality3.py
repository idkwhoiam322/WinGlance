from pathlib import Path
import subprocess

BASE = "cc7e82dd8c6d8eaaf5375d6cd020d8ae8d68df8c"


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if text.count(old) != 1:
        raise SystemExit(f"{path}: expected exactly one marker, found {text.count(old)}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def replace_between(path: str, start: str, end: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if text.count(start) != 1:
        raise SystemExit(f"{path}: start marker count {text.count(start)} != 1")
    a = text.index(start)
    b = text.find(end, a + len(start))
    if b < 0:
        raise SystemExit(f"{path}: end marker missing")
    p.write_text(text[:a] + new + text[b:], encoding="utf-8")


# The prior automated refactor overreached while replacing a match range.
# Restore only main_window.rs to the known-good feature implementation commit,
# then make narrowly asserted replacements below. No glass feature work is lost.
restored = subprocess.check_output(["git", "show", f"{BASE}:src/main_window.rs"])
Path("src/main_window.rs").write_bytes(restored)

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

# Keep paint_settings at the pre-feature outer match-arm count by routing the
# two visual toggles through a small helper.
replace_between(
    "src/main_window.rs",
    "                        SettingId::GlassEffect => (",
    "                        SettingId::PinnedSource => (",
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
replace_once(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => if cfg.overlay.fade_persistent_pill { \"Yes\" } else { \"No\" }.into(),\n        SettingId::GlassEffect => on_off(cfg.overlay.glass_effect),\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => visual_setting_value(id, cfg),\n",
)

old_actions = '''        SettingAction::ToggleGlassEffect => {
            let new_value = !state.cfg().overlay.glass_effect;
            state.mutate_config(|cfg| cfg.overlay.glass_effect = new_value);
            set_glass_effect(state.overlay_hwnd, new_value);
            info!("glass effect set: {new_value}");
            state.invalidate();
        }
        SettingAction::ToggleFadePersistentPill => {
            let new_value = !state.cfg().overlay.fade_persistent_pill;
            state.mutate_config(|cfg| cfg.overlay.fade_persistent_pill = new_value);
            set_fade_persistent_pill(state.overlay_hwnd, new_value);
            info!("fade persistent pill set: {new_value}");
            state.invalidate();
        }
'''
new_actions = '''        action @ (SettingAction::ToggleGlassEffect | SettingAction::ToggleFadePersistentPill) => {
            perform_visual_toggle(state, action);
            state.invalidate();
        }
'''
replace_once("src/main_window.rs", old_actions, new_actions)

# Backdrop::sync already no-ops unless active. Removing this wrapper restores
# render_layered's pre-feature branch count without weakening behavior.
replace_once(
    "src/overlay/render.rs",
    '''    if state.backdrop.active() {
        state
            .backdrop
            .sync(state.hwnd, position.x + inset, position.y + inset, width, height);
    }
''',
    '''    state
        .backdrop
        .sync(state.hwnd, position.x + inset, position.y + inset, width, height);
''',
)

print("safe blur quality repair applied")
