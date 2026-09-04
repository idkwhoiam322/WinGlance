from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:180]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

p = "src/overlay/mod.rs"

replace(
    p,
    '''        // Hiding the pill goes quiet: the name must not linger for a hidden\n        // window.\n        state.hide();\n        assert_eq!(*cell.lock().unwrap(), None);\n''',
    '''        // Retiring the media content falls back to the always-visible\n        // passive card; the accessible name must follow what is actually\n        // rendered rather than retaining the retired source.\n        state.hide();\n        assert!(state.idle_content);\n        assert_eq!(*cell.lock().unwrap(), Some("No media playing".to_string()));\n''',
)
replace(p, "    fn session_rejected_hides_the_pill_when_nothing_valid_remains() {\n", "    fn session_rejected_falls_back_to_idle_when_nothing_valid_remains() {\n")
replace(
    p,
    '''        assert!(state.content.is_none());\n        assert!(state.last_track.is_none());\n        assert!(matches!(state.phase, Phase::Hidden));\n''',
    '''        assert!(state.idle_content);\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(state.last_track.is_none());\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        assert!(state.content.is_none(), "the stale track pill must be hidden");\n\n        // Re-enable: the fast-path finds nothing to restore, so the pill\n        // stays hidden instead of resurrecting the settled source's track.\n        state.toggle_enabled();\n        assert!(state.enabled);\n        assert!(state.content.is_none(), "no stale track may be restored on re-enable");\n        assert!(matches!(state.phase, Phase::Hidden), "the pill must stay hidden");\n''',
    '''        assert!(state.idle_content, "the stale media must fall back to passive status");\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "Notifications paused"\n        ));\n\n        // Re-enable: the fast-path finds nothing to restore, so the passive\n        // card becomes the truthful no-media state instead of resurrecting\n        // the settled source's track.\n        state.toggle_enabled();\n        assert!(state.enabled);\n        assert!(state.idle_content);\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        assert!(state.content.is_none(), "the stale track pill must be dismissed");\n        assert!(state.last_track.is_none(), "the standby must die with its source");\n        assert!(matches!(state.phase, Phase::Hidden), "nothing valid remains to show");\n''',
    '''        assert!(state.idle_content, "the stale track pill must retire into passive status");\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(state.last_track.is_none(), "the standby must die with its source");\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        assert!(\n            state.content.is_none(),\n            "a paused survivor must not be announced as now playing"\n        );\n        assert!(state.last_track.is_none());\n        assert!(matches!(state.phase, Phase::Hidden));\n''',
    '''        assert!(state.idle_content, "a paused survivor must not be announced as now playing");\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(state.last_track.is_none());\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        assert!(\n            state.content.is_none(),\n            "with both sources settled there is nothing playing to announce"\n        );\n        assert!(matches!(state.phase, Phase::Hidden));\n''',
    '''        assert!(state.idle_content, "with both sources settled only passive status remains");\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        // Run the collapse past its animation length: it must complete into a\n        // hide, not back into a bright shown pill at idle opacity.\n        state.phase = Phase::Collapsing(Instant::now() - collapse_duration(&state.config) - Duration::from_millis(1));\n        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));\n        state.tick();\n        assert!(\n            matches!(state.phase, Phase::Hidden),\n            "the completed collapse must hide the stopped pill"\n        );\n''',
    '''        // Run the collapse past its animation length: the Stopped tombstone\n        // must disappear, then the always-visible no-media status takes over.\n        state.phase = Phase::Collapsing(Instant::now() - collapse_duration(&state.config) - Duration::from_millis(1));\n        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));\n        state.tick();\n        assert!(state.idle_content, "the completed tombstone must retire into passive status");\n        assert!(matches!(\n            state.content.as_ref(),\n            Some(MediaEvent::TrackChanged(track)) if track.title == "No media playing"\n        ));\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)
replace(
    p,
    '''        assert!(\n            state.current_source.is_none(),\n            "current_source must clear when the pill collapses"\n        );\n        assert!(matches!(state.phase, Phase::Hidden));\n''',
    '''        assert!(\n            state.current_source.is_none(),\n            "current_source must clear when media content retires"\n        );\n        assert!(state.idle_content);\n        assert!(matches!(state.phase, Phase::Shown));\n''',
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", p], check=True)
subprocess.run(["git", "commit", "-m", "feat(overlay): keep a truthful no-media idle pill visible"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
