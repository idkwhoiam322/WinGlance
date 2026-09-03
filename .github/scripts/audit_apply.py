from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

replace(
    "src/main.rs",
    '''/// Applies the public startup contract to the effective in-memory config.\n/// Legacy configs may still contain `start_in_tray = false`, and first-run\n/// config creation still records its transient discovery flag, but neither is\n/// allowed to raise a production window: every process launch is silent until\n/// the user explicitly opens WinGlance from the tray.\nfn enforce_silent_startup(config: &mut Config) {\n    if config.first_run || !config.behavior.start_in_tray {\n        info!(\n            "startup window request ignored: WinGlance always starts silently; open the tracking window from the tray"\n        );\n    }\n    config.first_run = false;\n    config.behavior.start_in_tray = true;\n}\n''',
    '''/// Applies the startup contract to the effective in-memory config.\n/// The first-run discovery flag is deliberately preserved so the tracking\n/// window opens once for initial setup. Legacy `start_in_tray = false` values\n/// remain parse-compatible but cannot make later launches, logon startup, or\n/// Settings-triggered restarts raise the window.\nfn enforce_startup_policy(config: &mut Config) {\n    if !config.behavior.start_in_tray {\n        info!(\n            "legacy start_in_tray=false ignored: only the first-ever launch opens the tracking window automatically"\n        );\n    }\n    config.behavior.start_in_tray = true;\n}\n''',
)
replace("src/main.rs", "    enforce_silent_startup(&mut config);\n", "    enforce_startup_policy(&mut config);\n")
replace(
    "src/main.rs",
    '''    #[test]\n    fn startup_policy_always_forces_silent_effective_config() {\n        let mut config = Config::default();\n        config.first_run = true;\n        config.behavior.start_in_tray = false;\n        enforce_silent_startup(&mut config);\n        assert!(!config.first_run, "first-run discovery must never raise a window");\n        assert!(\n            config.behavior.start_in_tray,\n            "legacy start_in_tray=false must not override the silent-start contract"\n        );\n    }\n''',
    '''    #[test]\n    fn startup_policy_preserves_first_run_but_silences_later_launches() {\n        let mut config = Config {\n            first_run: true,\n            behavior: crate::config::BehaviorConfig {\n                start_in_tray: false,\n                ..Default::default()\n            },\n            ..Default::default()\n        };\n        enforce_startup_policy(&mut config);\n        assert!(config.first_run, "the first launch must remain discoverable for setup");\n        assert!(\n            config.behavior.start_in_tray,\n            "legacy start_in_tray=false must not make later launches visible"\n        );\n    }\n''',
)
replace(
    "README.md",
    '''Download `WinGlance.exe` from the latest [release](../../releases), run it, and\nit starts silently in the tray. First run, normal launches, logon startup, and\nSettings-triggered restarts never raise the tracking window on their own.\nOpen the tracking window explicitly by clicking (or double-clicking) the tray\nicon: it shows the current activity and a per-source history on the **Now\nPlaying** pane, plus a **Settings** pane mirroring the tray menu\n(notifications, duration, start-on-login, close-to-tray, allowed apps, layout,\nposition, monitor, preferred source, logs). The pill appears when media plays.\n''',
    '''Download `WinGlance.exe` from the latest [release](../../releases) and run it.\nThe first-ever launch opens the tracking window once so you can review and adjust\nSettings. After that, normal launches, logon startup, and Settings-triggered\nrestarts stay silent in the tray. Open the tracking window again by clicking (or\ndouble-clicking) the tray icon. It shows the current activity and a per-source\nhistory on the **Now Playing** pane, plus a **Settings** pane mirroring the tray\nmenu (notifications, duration, start-on-login, close-to-tray, allowed apps,\nlayout, position, monitor, preferred source, logs).\n''',
)
replace(
    "config.example.toml",
    '''# Compatibility key retained for older configs. WinGlance always starts\n# silently; false is still accepted when parsing but is ignored for the\n# effective running config.\nstart_in_tray = true\n''',
    '''# Compatibility key retained for older configs. The first-ever launch opens\n# the tracking window once for setup; after that, launches are silent regardless\n# of this legacy key. `false` remains parse-compatible but no longer opens later launches.\nstart_in_tray = true\n''',
)
replace(
    "docs/configuration.md",
    '''| `start_in_tray`                 | `true`  | bool   | Backward-compatible key retained for existing configs. Startup is always silent; `false` is accepted when parsing but the effective running config is forced to `true`, so it no longer opens the tracking window |\n''',
    '''| `start_in_tray`                 | `true`  | bool   | Backward-compatible key retained for existing configs. The first-ever launch still opens the tracking window once for setup; later launches are silent, and `false` remains parse-compatible but no longer opens them |\n''',
)
replace(
    "docs/configuration.md",
    '''menu item). Every process launch stays silent: first run, normal launches, logon\nstartup, and the Settings **Restart app** action create the window hidden. The\nlegacy `start_in_tray` key remains readable for backward compatibility, but\n`false` is normalized to the effective silent policy and no longer opens the\nwindow. User interaction through the tray is the only startup-independent path\nthat raises the tracking window.\n''',
    '''menu item). The first-ever launch — the run that creates `config.toml` — opens\nthe tracking window once so the user can review Settings. Every later launch,\nlogon startup, and the Settings **Restart app** action create it hidden. The\nlegacy `start_in_tray` key remains readable for backward compatibility, but\n`false` is normalized to the effective silent policy and no longer opens later\nlaunches. After first-run setup, tray interaction is the explicit way to raise\nthe tracking window.\n''',
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", "src/main.rs", "README.md", "config.example.toml", "docs/configuration.md"], check=True)
subprocess.run(["git", "commit", "-m", "fix(startup): show setup window only on first launch"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
