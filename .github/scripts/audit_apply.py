from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:220]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")

p = "src/main_window.rs"
replace(p, "            state: PlaybackState::NowPlaying,\n            accepted: true,\n", "            state: PlaybackState::NowPlaying,\n            disposition: HistoryDisposition::Shown,\n")
replace(p, "            state: PlaybackState::Paused,\n            accepted: true,\n", "            state: PlaybackState::Paused,\n            disposition: HistoryDisposition::Shown,\n")
replace(p, "                state: PlaybackState::Playing,\n                accepted: true,\n", "                state: PlaybackState::Playing,\n                disposition: HistoryDisposition::Shown,\n")
replace(
    p,
    "    fn history_keeps_accepted_flag_with_newest_first() {\n",
    "    fn history_keeps_disposition_with_newest_first() {\n",
)
replace(p, "            state: PlaybackState::Playing,\n            accepted: true,\n", "            state: PlaybackState::Playing,\n            disposition: HistoryDisposition::Shown,\n")
replace(p, "            state: PlaybackState::Paused,\n            accepted: false,\n", "            state: PlaybackState::Paused,\n            disposition: HistoryDisposition::Rejected,\n")
replace(
    p,
    "        // Newest first, and the accepted flag travels with its entry.\n        assert_eq!(entries[0].track.title, \"Track B\");\n        assert!(!entries[0].accepted);\n        assert_eq!(entries[1].track.title, \"Track A\");\n        assert!(entries[1].accepted);\n",
    "        // Newest first, and the explicit disposition travels with its entry.\n        assert_eq!(entries[0].track.title, \"Track B\");\n        assert_eq!(entries[0].disposition, HistoryDisposition::Rejected);\n        assert_eq!(entries[1].track.title, \"Track A\");\n        assert_eq!(entries[1].disposition, HistoryDisposition::Shown);\n",
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/main_window.rs"] , check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "fix(history): update disposition regression fixtures"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
