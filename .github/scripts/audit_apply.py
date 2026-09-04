from pathlib import Path
import subprocess

p = Path("src/overlay/mod.rs")
text = p.read_text(encoding="utf-8")
old = "    config: Config,\n"
new = "    config: Box<Config>,\n"
if text.count(old) != 1:
    raise SystemExit("OverlayState config field anchor not found exactly once")
text = text.replace(old, new, 1)
old = "            hwnd: HWND::default(),\n            config,\n            queue,\n"
new = "            hwnd: HWND::default(),\n            config: Box::new(config),\n            queue,\n"
if text.count(old) != 1:
    raise SystemExit("OverlayState constructor config anchor not found exactly once")
text = text.replace(old, new, 1)
p.write_text(text, encoding="utf-8")

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "perf(overlay): keep cold config off the hot state"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
