from pathlib import Path
import re
import subprocess

p = Path("src/overlay/mod.rs")
text = p.read_text(encoding="utf-8")
pattern = re.compile(
    r"\n    #\[test\]\n    fn an_indexed_monitor_keeps_its_device_across_a_reorder\(\) \{.*?\n    \}\n(?=\n    #\[test\])",
    re.S,
)
text, count = pattern.subn("", text, count=1)
if count != 1:
    raise SystemExit("obsolete sticky-monitor regression test not found exactly once")
p.write_text(text, encoding="utf-8")

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "test(monitor): replace obsolete sticky resolver coverage"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
