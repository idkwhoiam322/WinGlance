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
| `duration_ms`  | `5000`  | 500–60000 | How long the pill stays visible before collapsing |

> **Progress bar.** When the playing app reports timeline position via SMTC
> (Spotify, Groove, MusicBee, and a few others — not browsers), the pill draws
> a thin accent bar along its bottom edge that advances with playback, freezes
> while paused, and re-bases on a seek. The `⏱` clock glyph in the meta row is
> suppressed in favor of the bar while it is visible (the `m:ss` text remains).
> Apps that do not report position show no bar and behave exactly as before.
| `animation_ms` | `500`   | 100–1000 | Expand/collapse animation length                  |
| `layout`       | `"expanded"` | `expanded` \| `compact` \| `auto` \| `persistent-compact` | Which pill layout is used (see below) |
| `vertical`     | `"top"` | `top` \| `bottom` | Which monitor edge the pill anchors to |
| `horizontal`   | `"center"` | `left` \| `center` \| `right` | Horizontal anchor within the work area |
| `margin`       | `8`     | 0–500   | Distance from the chosen edge (logical px)        |
| `max_width`    | `340`   | 180–800 | Maximum pill width in logical pixels              |
| `position_x`   | *(unset)* | integer | Absolute X override (96-DPI logical px); set by *Adjust position…* |
| `position_y`   | *(unset)* | integer | Absolute Y override (96-DPI logical px); set by *Adjust position…* |
| `max_tick_hz`  | `60`    | 60–1000 | Animation tick-rate cap in Hz (config.toml only; see below) |
| `monitor`      | `"active-window"` | string | Which display the pill is placed on (see below) |
| `compact_position_separate` | `false` | bool | Give the Compact layout its own position (see below). The settings toggle displays the *inverse* polarity: ON = Compact follows Expanded (`false`), OFF = independent (`true`) |
| `compact_vertical` | `"top"` | `top` \| `bottom` | Compact layout's vertical anchor (only consulted while `compact_position_separate` is on) |
| `compact_horizontal` | `"center"` | `left` \| `center` \| `right` | Compact layout's horizontal anchor (same condition) |
| `compact_margin` | `8`   | 0–500   | Compact layout's edge distance (same condition)   |
| `compact_position_x` | *(unset)* | integer | Absolute X override for the Compact layout; set by the compact *Adjust position…* |
| `compact_position_y` | *(unset)* | integer | Absolute Y override for the Compact layout        |
| `compact_monitor` | `"active-window"` | string | Which display the Compact layout is placed on     |
| `dismiss_on_hover` | `true` | bool | Hovering a pill in the Expanded layout arms its dismissal (remaining time capped at 500 ms, one-way). For Compact pills it makes the second hover dismiss (see below) |
| `expand_compact_on_hover` | `true` | bool | Hovering a pill in the Compact layout expands it in place; with `dismiss_on_hover` on, the second hover dismisses (see below) |

`layout` accepts one of:

- `"expanded"` — the classic pill: title, artist, source app and a small
  album-art square.
- `"compact"` — a slim pill: album art, single title line, app icon and the
  playback symbol, no artist/source rows.
- `"auto"` — per-foreground resolution: the pill compacts while the
  foreground app is fullscreen or its executable matches an
  `auto_compact_sources` pattern, and expands otherwise. The verdict is
  re-sampled at every show and whenever the foreground window changes.
- `"persistent-compact"` — the pill is always shown, but in the Compact
  layout (a slim, lower-profile pill). Useful for a permanent status
  indicator that never auto-hides.

While `compact_position_separate` is `false`, the Compact layout follows the
live Expanded position — changing `vertical`/`horizontal`/`margin`/`monitor`
or dragging a custom placement moves the compact pill with it, and the
`compact_*` fields are retained but ignored. The settings row and the tray
submenu stay fully editable in this state: edits land in the `compact_*`
fields and take visible effect once the toggle is turned off (independent)
and the pill is actually Compact (or Auto resolving to Compact). They are
never lost: the first switch to independent copies the current Expanded
position into still-default `compact_*` fields, while later switches restore
the previously customized values.

`monitor` accepts one of:

- `"active-window"` — the display of the foreground window (the historical
  behavior; also the default for configs written before this key existed).
- `"primary"` — the display Windows marks as primary.
- `"index-N"` — the (N+1)-th display in Windows' enumeration order, so
  `"index-0"` is the first display, `"index-1"` the second, and so on.

The index is resolved against the *current* display layout each time the pill
is placed. If the configured display is temporarily unplugged or reordered,
the pill falls back to the primary display while the setting stays untouched,
so it reapplies automatically when the display returns. Custom
`position_x`/`position_y` values keep their existing semantics: absolute
virtual-screen coordinates in 96-DPI logical pixels — not relative to the
selected display — and the resulting position is clamped into the selected
display's work area.

`max_tick_hz` caps how often the pill's animation refreshes; on higher-refresh
monitors the UI thread is throttled down to it. Motion stays time-based — the
cap only limits repaint frequency. Values at or below 60 keep the default 60 Hz
cap, and values above 1000 are clamped to 1000. It is configurable only via
`config.toml`, not the Settings UI.

Hovering behavior follows the pill's *effective* layout (for `"auto"`, the
layout currently in effect — an Auto pill in the expanded layout follows the
Expanded rules, in the compact layout the Compact rules):

- **Expanded layout, `dismiss_on_hover = true`** — hovering arms the
  dismissal: the remaining time is capped at 500 ms, one-way (leaving before
  that does not cancel it). The countdown is never deferred for the cursor.
  With `dismiss_on_hover = false`, hovering does nothing.
- **Compact layout, `expand_compact_on_hover = true`** — the first hover of
  a showing expands the pill in place and resets the countdown to the full
  duration. The expanded state is an interaction: while the cursor stays on
  it, the countdown is deferred and the pill is never dismissed. Leaving
  collapses it back to compact and resets the countdown again. With
  `dismiss_on_hover` enabled (default), later hovers over the compact pill
  dismiss instead — the second hover dismisses (one-way 500 ms arm);
  without it, every hover re-expands and resets, and the pill leaves only
  when the countdown expires with no hover interaction.
- **Compact layout, `expand_compact_on_hover = false`** — the pill behaves
  exactly like an Expanded one: `dismiss_on_hover` applies.

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
| `auto_compact_sources`          | `[]`    | list   | Apps (case-insensitive substrings) that force the Compact layout while `layout` is `"auto"`; empty = only fullscreen compacts |
| `hide_for_auto_compact_sources` | `true`  | bool   | When `layout` is `"persistent-compact"`, hide the pill while a fullscreen window or a listed `auto_compact_sources` app is the foreground window; resumes when the foreground clears |

## [appearance]

| Key                | Default       | Range    | Effect                                  |
|--------------------|---------------|----------|-----------------------------------------|
| `background_color` | `[18, 20, 28, 235]` | RGBA 0–255 | Near-opaque dark-slate pill background (alpha 235 ≈ 92%; lower for more translucency) |
| `text_color`       | `[255, 255, 255, 255]` | RGBA 0–255 | Title and state-label color |
| `accent_color`     | `[240, 110, 155, 255]` | RGBA 0–255 | Playback symbols, music note, album-art rim; aura fallback when the artwork palette has no vibrant color |
| `corner_radius`    | `26.0`     | 4–48    | Corner rounding in logical pixels       |
| `compact_corner_radius` | `12.0` | 4–48    | Corner rounding of the Compact layout, in logical pixels (independent of `corner_radius`; see below) |
| `padding`          | `15.0`      | 4–32    | Gap between pill edge and content       |
| `art_size`         | `48`       | 24–96   | Album-art square size (pill height is derived from this) |
| `font_size_title`  | `16.0`     | 8–32    | Track title font size                   |
| `font_size_artist` | `13.0`     | 8–28    | Artist (or source app) font size        |

Colors are `[R, G, B, A]` with 0–255 components. The alpha channel is used
for compositing; the default (235, ~92%) keeps the pill near-opaque while
letting a hint of the backdrop through. Text, album art and symbols stay
fully opaque regardless of the body alpha.

`compact_corner_radius` rounds the Compact layout independently of
`corner_radius` (which keeps controlling Expanded). The default of `12.0`
makes the slim Compact pill a moderately rounded media card rather than a
capsule; values near `6.0` read as a nearly square rectangle. Setting both
keys to the same value makes the layouts look alike — e.g. both `26.0` —
though on the shorter Compact pill a high radius visually approaches a
capsule (the render clamps it to half the smaller pill dimension). Both
radii use the same DPI scaling, aura, fill, border and clipping pipeline —
only the corner amount differs.

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
- **Content type** — the type SMTC reports for the session (music, video,
  image) does two things, with no config knob:
  - Track-change pills from sources that report `Video` swap the music note
    for a video-player glyph. State pills (play/pause/stop) never change.
  - `Image` content (slideshows, photo apps) is suppressed entirely: no
    track or state pill fires while the image session is current. The
    worker logs one `pill suppressed | reason=image-content` line per
    transition.

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
- **Monitor** — which display the pill is placed on (Active window, Primary, or
  a numbered Display).
- **Duration** — 2 s / 3 s / 5 s / 10 s.
- **Layout** — Expanded / Compact / Auto (mirrors `overlay.layout`).
  Pill placement — the edge anchor, the custom coordinates, and the per-layout
  Compact position — is edited in the **Settings** pane, not the tray: the
  **Position** row offers edge anchors and **Adjust position…** opens the drag
  sample; the **Compact Position follows Expanded Position** toggle sets the
  `compact_position_separate` polarity (ON = follow).
- **Quit** — stop the process and remove the tray icon.

## Compact WinGlance defaults

The shipped `[overlay]`/`[appearance]` defaults (`config.rs`:
`OverlayConfig::default`/`AppearanceConfig::default`) produce a slim card-sized
pill:

- **Width** is capped at `max_width` (340 logical px). The compact body is one
  title row wide — art tile + title viewport (half `max_width`, floored at 180)
  + app icon + playback symbol + padding — so the pill never exceeds `max_width`
  and a long title marquees in place rather than truncating.
- **Height** is one title row (`font_size_title` × `ROW_HEIGHT`, 16 × 1.35 ≈ 22
  px) plus padding top and bottom (15 px each) — about 52 px at the shipped
  defaults. (The 48 px `art_size` tile clamps to the row-band height on the
  default font, so the art does not set the compact height.)
- **Anchor** `vertical = top`, `horizontal = center`, `margin = 8`, on the
  `monitor` display (`active-window` by default). With
  `compact_position_separate = false`, Compact tracks the live Expanded
  placement; enabling separation copies the current Expanded position in and
  lets the compact anchor diverge.
- **Visuals**: rounded rect at `compact_corner_radius` (12 px), filled with
  `background_color` (`[0x12, 0x14, 0x1C]`, alpha 235 ≈ 92 % opaque) so a hint of
  the backdrop shows through; a palette aura ring in the DIB margin (the cover's
  primary/secondary hues, falling back to the hardcoded pink `accent_color`
  `[240, 110, 155]` when no palette is extracted) brighter on the artwork side,
  and a white top-left→bottom-right directional edge highlight. Title and meta
  text render white with per-track accent coloring.

All of these can be widened or recolored here.
