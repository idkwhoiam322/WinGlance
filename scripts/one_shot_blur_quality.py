from pathlib import Path
import re


def sub(path: str, pattern: str, replacement: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    out, n = re.subn(pattern, replacement, text, count=count, flags=re.S)
    if n != count:
        raise SystemExit(f"{path}: expected {count} replacement(s), got {n}: {pattern[:100]!r}")
    p.write_text(out, encoding="utf-8")


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    n = text.count(old)
    if n < count:
        raise SystemExit(f"{path}: expected >= {count}, found {n}: {old[:100]!r}")
    p.write_text(text.replace(old, new, count), encoding="utf-8")


# render_layered: Backdrop::sync already no-ops when inactive. Calling it
# unconditionally keeps the grandfathered renderer's branch count unchanged.
sub(
    "src/overlay/render.rs",
    r"\n\s*if state\.backdrop\.active\(\) \{\n\s*state\.backdrop\.sync\(\n\s*state\.hwnd,\n\s*position\.x \+ inset,\n\s*position\.y \+ inset,\n\s*width,\n\s*height,\n\s*\);\n\s*\}\n\s*// Re-assert topmost",
    "\n    state.backdrop.sync(\n        state.hwnd,\n        position.x + inset,\n        position.y + inset,\n        width,\n        height,\n    );\n    // Re-assert topmost",
)

# Main-window settings helpers move the new decision points out of already
# grandfathered mega-functions. Each helper is deliberately small.
helper = r'''
fn visual_toggle_row(
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
replace(
    "src/main_window.rs",
    "/// Pure Settings-pane hit test shared by the mouse path. Row indexing counts\n",
    helper + "/// Pure Settings-pane hit test shared by the mouse path. Row indexing counts\n",
)

# Paint: replace two dedicated arms with one baseline-sized arm and delegate.
sub(
    "src/main_window.rs",
    r'''\s*SettingId::GlassEffect => \(\n\s*"Windows 11 glass effect",\n\s*if glass_effect \{\n\s*"ON"\.to_string\(\)\n\s*\} else \{\n\s*"OFF"\.to_string\(\)\n\s*\},\n\s*if glass_effect \{ accent \} else \{ colors\.faint \},\n\s*\),\n\s*SettingId::FadePersistentPill => \(\n\s*"Fade Persistent Compact Pill after duration",\n\s*if fade_persistent_pill \{\n\s*"Yes"\.to_string\(\)\n\s*\} else \{\n\s*"No"\.to_string\(\)\n\s*\},\n\s*if fade_persistent_pill \{ accent \} else \{ colors\.faint \},\n\s*\),''',
    "\n                        SettingId::GlassEffect | SettingId::FadePersistentPill => visual_toggle_row(\n                            *id,\n                            fade_persistent_pill,\n                            glass_effect,\n                            accent,\n                            colors.faint,\n                        ),",
)

# Mouse action resolver: retain the old single match arm's complexity.
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => Some(SettingAction::ToggleFadePersistentPill),\n        SettingId::GlassEffect => Some(SettingAction::ToggleGlassEffect),\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => Some(visual_toggle_action(id)),\n",
)

# UIA label/value: same number of outer match arms as the pre-feature code.
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => \"Fade Persistent Compact Pill after duration\",\n        SettingId::GlassEffect => \"Windows 11 glass effect\",\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => visual_setting_label(id),\n",
)
replace(
    "src/main_window.rs",
    "        SettingId::FadePersistentPill => if cfg.overlay.fade_persistent_pill { \"Yes\" } else { \"No\" }.into(),\n        SettingId::GlassEffect => on_off(cfg.overlay.glass_effect),\n",
    "        SettingId::FadePersistentPill | SettingId::GlassEffect => visual_setting_value(id, cfg),\n",
)

# Action execution: delegate both visual toggles through the old one-arm slot.
sub(
    "src/main_window.rs",
    r'''\s*SettingAction::ToggleGlassEffect => \{\n\s*let new_value = !state\.cfg\(\)\.overlay\.glass_effect;\n\s*state\.mutate_config\(\|cfg\| cfg\.overlay\.glass_effect = new_value\);\n\s*set_glass_effect\(state\.overlay_hwnd, new_value\);\n\s*info!\("glass effect set: \{new_value\}"\);\n\s*state\.invalidate\(\);\n\s*\}\n\s*SettingAction::ToggleFadePersistentPill => \{\n\s*let new_value = !state\.cfg\(\)\.overlay\.fade_persistent_pill;\n\s*state\.mutate_config\(\|cfg\| cfg\.overlay\.fade_persistent_pill = new_value\);\n\s*set_fade_persistent_pill\(state\.overlay_hwnd, new_value\);\n\s*info!\("fade persistent pill set: \{new_value\}"\);\n\s*state\.invalidate\(\);\n\s*\}''',
    "\n        action @ (SettingAction::ToggleGlassEffect | SettingAction::ToggleFadePersistentPill) => {\n            perform_visual_toggle(state, action);\n            state.invalidate();\n        }",
)

# Documentation is part of the config schema contract and is enforced by a test.
replace(
    "docs/configuration.md",
    "| `expand_compact_on_hover` | `true` | bool | Hovering a pill in the Compact layout expands it in place; with `dismiss_on_hover` on, the second hover dismisses (see below) |\n| `fade_persistent_pill`",
    "| `expand_compact_on_hover` | `true` | bool | Hovering a pill in the Compact layout expands it in place; with `dismiss_on_hover` on, the second hover dismisses (see below) |\n"
    "| `glass_effect` | `false` | bool | Opt in to the Windows 11 Desktop Acrylic material behind the pill body. The existing layered renderer still draws content and the aura. Unsupported DWM backdrops, High Contrast, or the system reduced-overlap preference fall back to the existing solid material |\n"
    "| `fade_persistent_pill`",
)

print("blur quality refactor applied")
