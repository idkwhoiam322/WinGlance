# Configuration reference

The config file is `%APPDATA%\WinGlance\WinGlance\data\config.toml`. It is created
automatically with the defaults below on first run. `Config::normalize()`
clamps every value to the safe ranges listed; out-of-range values are
corrected silently but the file is not rewritten (see `docs/architecture.md`).

> **Known limitation:** a hand-edited `config.toml` is only read at startup.
> There is no live reload while the app is running — restart WinGlance after
> editing the file by hand. Settings changed from the tray menu or the
> Settings pane are applied immediately and persisted.

## [overlay]

| Key            | Default | Range   | Effect                                            |
|----------------|---------|---------|---------------------------------------------------|
| `duration_ms`  | `3000`  | 500–60000 | How long the pill stays visible before collapsing |
| `animation_ms` | `280`   | 100–500 | Expand/collapse animation length                  |
| `vertical`     | `"top"` | `top` \| `bottom` | Which monitor edge the pill anchors to |
| `horizontal`   | `"center"` | `left` \| `center` \| `right` | Horizontal anchor within the work area |
| `margin`       | `8`     | 0–500   | Distance from the chosen edge (logical px)        |
| `max_width`    | `340`   | 180–800 | Maximum pill width in logical pixels              |
| `position_x`   | *(unset)* | integer | Absolute X override (96-DPI logical px); set by *Adjust position…* |
| `position_y`   | *(unset)* | integer | Absolute Y override (96-DPI logical px); set by *Adjust position…* |

## [behavior]

| Key                             | Default | Range  | Effect                                  |
|---------------------------------|---------|--------|-----------------------------------------|
| `enable_track_change`           | `true`  | bool   | Show the pill when the track changes    |
| `enable_playback_state_change`  | `true`  | bool   | Show a small state pill on play/pause   |
| `notifications_enabled`         | `true`  | bool   | Master switch for pill notifications; mirrors the tray "Toggle notifications" item and is persisted so the toggle survives a restart |
| `debounce_ms`                   | `200`   | 150–250 | Coalescing window for bursty SMTC events |
| `start_in_tray`                 | `true`  | bool   | Start silently: no window, only the tray icon + pill |
| `start_on_login`                | `false` | bool   | Register a Windows startup entry to launch WinGlance at logon |
| `close_to_tray`                 | `true`  | bool   | Hide (instead of close) the window when its X is pressed |
| `media_sources`                 | `[]`    | list   | Media source apps (case-insensitive substrings) to allow; empty = all apps |

## [appearance]

| Key                | Default       | Range    | Effect                                  |
|--------------------|---------------|----------|-----------------------------------------|
| `background_color` | `[18, 20, 28, 235]` | RGBA 0–255 | Near-opaque dark-slate pill background (alpha 235 ≈ 92%; lower for more translucency) |
| `text_color`       | `[255, 255, 255, 255]` | RGBA 0–255 | Title and state-label color |
| `accent_color`     | `[240, 110, 155, 255]` | RGBA 0–255 | Playback symbols, music note, album-art rim; aura fallback when the artwork palette has no vibrant color |
| `corner_radius`    | `26.0`     | 4–48    | Corner rounding in logical pixels       |
| `padding`          | `15.0`      | 4–32    | Gap between pill edge and content       |
| `art_size`         | `48`       | 24–96   | Album-art square size (pill height is derived from this) |
| `font_size_title`  | `16.0`     | 8–32    | Track title font size                   |
| `font_size_artist` | `13.0`     | 8–28    | Artist (or source app) font size        |

Colors are `[R, G, B, A]` with 0–255 components. The alpha channel is used
for compositing; the default (235, ~92%) keeps the pill near-opaque while
letting a hint of the backdrop through. Text, album art and symbols stay
fully opaque regardless of the body alpha.

## Per-track theming (not configurable)

Beyond these knobs the pill derives its look from the playing track:

- **Palette** — two vibrant colors extracted from the album art (from the
  worker's decoded buffer, computed once per cover in the UI thread): a
  4-bit-per-channel histogram, filtered through
  a four-tier guard chain — vibrant scoring, strict guard (saturation ≥ 0.25,
  luminance 0.20–0.85), a relaxed fallback for dark covers (S ≥ 0.10,
  L ≥ 0.10),   and a monochrome tier for B&W and high-key covers (any pixel with Y ≥ 0.18)
  — so moody portraits and bright white covers get their own tint
  instead of the accent default. The primary recolors the playback
  symbols, the clock icon and the music note; a muted variant tints the
  artist and source-app rows; and the pill fill itself is tinted toward
  the primary at 16% weight, so the background picks up a hint of the
  cover's hue. When the artwork yields no qualifying color, the accent
  above is used.
- **Aura** — a soft C₁→C₂ glow around the pill boundary, brighter on the left
  where the art sits, fading exponentially over a ~6 px halo. Intensity is
  hardcoded, not in the config.
- **Edge highlight** — a subtle white stroke along the pill's own boundary,
  brighter at the top-left than the bottom-right, so the pill reads as a
  physical cut edge. Not configurable.
- **App icon** — the source app's icon (from its AUMID) renders next to the
  source-app name.
- **Album-art rim** — a thin accent stroke around the art square.

## Logging

Logging has no configuration. A single `log-Live.log` file in
`<data_dir>\logs` captures the current run; it is truncated at startup and
capped at 1 MiB during the run (a churn-heavy session cannot grow the file
without bound). No history is retained.

## [main window] and the system tray

WinGlance keeps a maximized tracking window alongside the WinGlance pill. The window shows
the current activity (art, state, title/artist/album) and the per-session history;
it is opened from the tray icon (double-click, or the **Open WinGlance** menu item).
It is never shown as a pop-up on launch.

The tray menu mirrors the `[behavior]` toggles in real time:

- **Open WinGlance** — restore the tracking window.
- **Toggle notifications** — enable/disable SMTC track-change + state-change events.
- **Start with Windows** — write/remove the `%APPDATA%\...\Run` registry entry.
- **Close window to tray** — on-off (mirrors `close_to_tray`).
- **Quit** — stop the process and remove the tray icon.

## Compact WinGlance defaults

The shipped defaults produce a slim pill: up to 340 px wide, height derived
from `art_size` (48 px) plus padding — about 110 px tall — anchored 8 px from
the top of the work area, near-opaque dark-slate background tinted per track with a
pink aura, 48 px
artwork with a palette-tinted glow and rim, and compact title/artist text with
per-track accent colors. All of these can be widened or recolored here.
