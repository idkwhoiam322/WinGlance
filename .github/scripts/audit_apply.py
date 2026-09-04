from pathlib import Path
import subprocess


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if text.count(old) != 1:
        raise SystemExit(f"{label} anchor count={text.count(old)}")
    return text.replace(old, new, 1)

p = Path("src/overlay/mod.rs")
text = p.read_text(encoding="utf-8")
text = replace_once(
    text,
    "    /// When the cursor hovers over the pill, the dismiss deadline is\n    /// shortened to 500ms. The arm is one-way: the pill dismisses 500ms\n    /// after the hover is first detected even if the cursor leaves before\n    /// then. The flag also stops the tick from re-arming (which would keep\n    /// pushing the deadline forward while the cursor stays put).\n    hover_dismiss_at: Option<Instant>,\n",
    "    /// When an Expanded pill is hovered, the dismiss deadline is capped\n    /// to 500 ms. The arm timestamp prevents re-arming while the cursor stays\n    /// put; leaving before that cap fires can restore the pre-hover deadline.\n    hover_dismiss_at: Option<Instant>,\n    /// Deadline that was in force immediately before hover-dismiss armed.\n    /// Used only to undo the hover cap; if another subsystem changes the\n    /// deadline after arming, that newer deadline wins instead.\n    hover_dismiss_resume_at: Option<Instant>,\n",
    "hover field",
)
text = replace_once(
    text,
    "            dismiss_at: None,\n            hover_dismiss_at: None,\n            hover_expand: None,\n",
    "            dismiss_at: None,\n            hover_dismiss_at: None,\n            hover_dismiss_resume_at: None,\n            hover_expand: None,\n",
    "constructor hover fields",
)
reset = "        self.hover_dismiss_at = None;\n"
if text.count(reset) != 4:
    raise SystemExit(f"hover reset count={text.count(reset)}")
text = text.replace(reset, reset + "        self.hover_dismiss_resume_at = None;\n")

helper_anchor = "/// Whether the pill's fonts must be rebuilt because the resolved target\n"
if text.count(helper_anchor) != 1:
    raise SystemExit("hover helper insertion anchor missing")
helpers = '''/// Apply the Expanded-layout hover cap without ever extending an earlier\n/// deadline. `None` means the pill previously had no deadline, so hover alone\n/// supplies the 500 ms cap.\nfn hover_capped_deadline(original: Option<Instant>, armed_at: Instant) -> Option<Instant> {\n    let early = armed_at + Duration::from_millis(EARLY_EXIT_MS);\n    Some(original.map_or(early, |deadline| deadline.min(early)))\n}\n\n/// Undo a hover cap only when it is still the deadline hover installed. A\n/// queue/content update that changed the deadline after arming is authoritative\n/// and must not be rolled back when the pointer leaves.\nfn hover_restored_deadline(\n    original: Option<Instant>,\n    armed_at: Instant,\n    current: Option<Instant>,\n) -> Option<Instant> {\n    if current == hover_capped_deadline(original, armed_at) {\n        original\n    } else {\n        current\n    }\n}\n\n#[cfg(test)]\nmod hover_deadline_tests {\n    use super::*;\n\n    #[test]\n    fn leaving_before_hover_cap_restores_the_original_deadline() {\n        let armed = Instant::now();\n        let original = Some(armed + Duration::from_secs(5));\n        let capped = hover_capped_deadline(original, armed);\n        assert_eq!(capped, Some(armed + Duration::from_millis(EARLY_EXIT_MS)));\n        assert_eq!(hover_restored_deadline(original, armed, capped), original);\n    }\n\n    #[test]\n    fn leaving_never_revives_an_already_expired_deadline() {\n        let armed = Instant::now();\n        let original = Some(armed - Duration::from_millis(1));\n        let capped = hover_capped_deadline(original, armed);\n        assert_eq!(capped, original);\n        assert_eq!(hover_restored_deadline(original, armed, capped), original);\n    }\n\n    #[test]\n    fn a_newer_non_hover_deadline_wins_when_the_pointer_leaves() {\n        let armed = Instant::now();\n        let original = Some(armed + Duration::from_secs(5));\n        let newer = Some(armed + Duration::from_secs(2));\n        assert_ne!(newer, hover_capped_deadline(original, armed));\n        assert_eq!(hover_restored_deadline(original, armed, newer), newer);\n    }\n\n    #[test]\n    fn hovering_a_deadline_free_pill_is_fully_reversible() {\n        let armed = Instant::now();\n        let capped = hover_capped_deadline(None, armed);\n        assert_eq!(capped, Some(armed + Duration::from_millis(EARLY_EXIT_MS)));\n        assert_eq!(hover_restored_deadline(None, armed, capped), None);\n    }\n}\n\n'''
text = text.replace(helper_anchor, helpers + helper_anchor, 1)

engaged = "        let engaged = hover_engaged(cursor_over, self.hover_leave_at, now);\n"
if text.count(engaged) != 1:
    raise SystemExit("engaged anchor missing")
text = text.replace(
    engaged,
    engaged
    + "        // Expanded-layout hover dismissal is forgiving: once the debounced\n"
      "        // leave is real, undo only the deadline installed by hover. If a\n"
      "        // newer queue/content decision changed the deadline meanwhile, keep it.\n"
      "        if !engaged && self.hover_expand.is_none()\n"
      "            && let Some(armed_at) = self.hover_dismiss_at.take()\n"
      "        {\n"
      "            let original = self.hover_dismiss_resume_at.take();\n"
      "            self.dismiss_at = hover_restored_deadline(original, armed_at, self.dismiss_at);\n"
      "            debug!(\"pill hover-dismiss cancelled on leave\");\n"
      "        }\n",
    1,
)

old_arm = '''                    HoverStep::ArmDismiss => {\n                        self.hover_dismiss_at = Some(now);\n                        // The arm caps the remaining time at 500ms; it must\n                        // never extend an already-sooner deadline (e.g. an\n                        // earlier hover arm or the queued-notification cap).\n                        let early = now + Duration::from_millis(EARLY_EXIT_MS);\n                        self.dismiss_at = Some(self.dismiss_at.map_or(early, |d| d.min(early)));\n                        debug!("pill hover-dismiss armed");\n                    }\n'''
new_arm = '''                    HoverStep::ArmDismiss => {\n                        let original = self.dismiss_at;\n                        self.hover_dismiss_resume_at = original;\n                        self.hover_dismiss_at = Some(now);\n                        self.dismiss_at = hover_capped_deadline(original, now);\n                        debug!("pill hover-dismiss armed");\n                    }\n'''
text = replace_once(text, old_arm, new_arm, "shown hover arm")
old_entrance = '''                if self.layout == LayoutMode::Expanded {\n                    self.hover_dismiss_at = Some(now);\n                    let early = now + Duration::from_millis(EARLY_EXIT_MS);\n                    self.dismiss_at = Some(self.dismiss_at.map_or(early, |d| d.min(early)));\n                    debug!("pill hover-dismiss armed");\n                }\n'''
new_entrance = '''                if self.layout == LayoutMode::Expanded {\n                    let original = self.dismiss_at;\n                    self.hover_dismiss_resume_at = original;\n                    self.hover_dismiss_at = Some(now);\n                    self.dismiss_at = hover_capped_deadline(original, now);\n                    debug!("pill hover-dismiss armed");\n                }\n'''
text = replace_once(text, old_entrance, new_entrance, "entrance hover arm")
text = text.replace("the one-way 500ms\n        // hover-dismiss", "the reversible 500ms\n        // hover-dismiss")
text = text.replace("the one-way dismiss", "the active hover dismiss")
text = text.replace("one-way arm", "hover arm")
p.write_text(text, encoding="utf-8")

p = Path("config.example.toml")
text = p.read_text(encoding="utf-8")
text = replace_once(
    text,
    "# Hovering a pill in the Expanded layout arms its dismissal (remaining time\n# capped at 500 ms, one-way). For Compact-layout pills it makes the second\n",
    "# Hovering a pill in the Expanded layout arms its dismissal (remaining time\n# capped at 500 ms). Leaving before that cap fires cancels the hover dismissal\n# and restores the pre-hover deadline. For Compact-layout pills it makes the second\n",
    "example hover docs",
)
p.write_text(text, encoding="utf-8")

p = Path("docs/configuration.md")
text = p.read_text(encoding="utf-8")
text = text.replace(
    '| `dismiss_on_hover` | `true` | bool | Hovering a pill in the Expanded layout arms its dismissal (remaining time capped at 500 ms, one-way). For Compact pills it makes the second hover dismiss (see below) |',
    '| `dismiss_on_hover` | `true` | bool | Hovering a pill in the Expanded layout arms a 500 ms dismissal cap; leaving before it fires cancels that hover cap and restores the prior deadline. For Compact pills it makes the second hover dismiss (see below) |',
)
text = replace_once(
    text,
    '- **Expanded layout, `dismiss_on_hover = true`** — hovering arms the\n  dismissal: the remaining time is capped at 500 ms, one-way (leaving before\n  that does not cancel it). The countdown is never deferred for the cursor.\n',
    '- **Expanded layout, `dismiss_on_hover = true`** — hovering arms the\n  dismissal: the remaining time is capped at 500 ms. Leaving before the\n  debounced hover cap fires cancels that hover-only dismissal and restores the\n  deadline that existed before the hover. Staying hovered still dismisses on\n  the cap, and leaving never revives a deadline that had already expired.\n',
    "configuration hover section",
)
p.write_text(text, encoding="utf-8")

p = Path("README.md")
text = p.read_text(encoding="utf-8")
text = replace_once(
    text,
    "- **Hover interaction** — hover a compact pill and it expands in place so\n  you can read it (a second hover dismisses it); hover an expanded pill and\n  it dismisses within 500 ms instead of waiting out its full duration.\n",
    "- **Hover interaction** — hover a compact pill and it expands in place so\n  you can read it (a second hover dismisses it); hover an expanded pill and\n  it dismisses within 500 ms instead of waiting out its full duration. Leave\n  before that hover cap fires and the original remaining lifetime is restored.\n",
    "readme hover wording",
)
p.write_text(text, encoding="utf-8")

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "add", "src/overlay/mod.rs", "config.example.toml", "docs/configuration.md", "README.md"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "commit", "-m", "fix(overlay): make hover dismissal reversible"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
