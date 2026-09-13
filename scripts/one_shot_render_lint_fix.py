from pathlib import Path

path = Path("src/overlay/render.rs")
text = path.read_text(encoding="utf-8")
old = "pub(super) fn blend_frames(to: &mut [u8], from: &[u8], weight: f32) {"
new = "#[cfg(test)]\npub(super) fn blend_frames(to: &mut [u8], from: &[u8], weight: f32) {"
if old not in text:
    raise SystemExit("expected blend_frames signature not found")
if "#[cfg(test)]\n" + old in text:
    raise SystemExit("blend_frames already test-only")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
