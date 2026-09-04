from pathlib import Path
import subprocess

p = Path("src/overlay/mod.rs")
text = p.read_text(encoding="utf-8")
marker = "    // Colder control, discovery, accessibility, and retained-cache state follows.\n"
if text.count(marker) != 1:
    raise SystemExit("cold-state marker not found exactly once")
head, tail = text.split(marker, 1)
if "    config: Config,\n" not in tail:
    raise SystemExit("OverlayState config field missing after cold-state marker")
tail = tail.replace("    config: Config,\n", "    config: Box<Config>,\n", 1)
text = head + marker + tail
ctor = "            hwnd: HWND::default(),\n            config,\n            queue,\n"
if ctor not in text:
    raise SystemExit("OverlayState constructor config anchor not found")
text = text.replace(ctor, "            hwnd: HWND::default(),\n            config: Box::new(config),\n            queue,\n", 1)
p.write_text(text, encoding="utf-8")

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "perf(overlay): keep cold config off the hot state"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
