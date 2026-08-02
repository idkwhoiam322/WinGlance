# Configuration reference

The config file is `%APPDATA%\notch\notch\data\config.toml`. It is created
automatically with the defaults below on first run. `Config::normalize()`
clamps every value to the safe ranges listed; out-of-range values are
corrected silently but the file is not rewritten (see `docs/architecture.md`).

## [overlay]

| Key            | Default | Range   | Effect                                            |
|----------------|---------|---------|---------------------------------------------------|
| `duration_ms`  | `3000`  | 500–60000 | How long the pill stays visible before collapsing |
| `animation_ms` | `200`   | 100–500 | Expand/collapse animation length                  |
| `vertical`     | `"top"` | `top` \| `bottom` | Which monitor edge the pill anchors to |
| `horizontal`   | `"center"` | `left` \| `center` \| `right` | Horizontal anchor within the work area |
| `margin`       | `8`     | 0–500   | Distance from the chosen edge (logical px)        |
| `max_width`    | `240`   | 180–800 | Maximum pill width in logical pixels              |
| `position_x`   | *(unset)* | integer | Absolute X override (96-DPI logical px); set by *Adjust position…* |
| `position_y`   | *(unset)* | integer | Absolute Y override (96-DPI logical px); set by *Adjust position…* |

## [behavior]

| Key                             | Default | Range  | Effect                                  |
|---------------------------------|---------|--------|-----------------------------------------|
| `enable_track_change`           | `true`  | bool   | Show the pill when the track changes    |
| `enable_playback_state_change`  | `true`  | bool   | Show a small state pill on play/pause   |
| `debounce_ms`                   | `200`   | 150–250 | Coalescing window for bursty SMTC events |
| `start_in_tray`                 | `true`  | bool   | Start silently: no window, only the tray icon + pill |
| `start_on_login`                | `false` | bool   | Register a Windows startup entry to launch notch at logon |
| `close_to_tray`                 | `true`  | bool   | Hide (instead of close) the window when its X is pressed |

## [appearance]

| Key                | Default       | Range    | Effect                                  |
|--------------------|---------------|----------|-----------------------------------------|
| `background_color` | `[0, 0, 0, 230]` | RGBA 0–255 | Pill background (near-black by default) |
| `text_color`       | `[255, 255, 255, 255]` | RGBA 0–255 | Title and state-label color |
| `accent_color`     | `[0, 212, 170, 255]` | RGBA 0–255 | Placeholder circle when no artwork |
| `corner_radius`    | `16.0`     | 4–48    | Corner rounding in logical pixels       |
| `padding`          | `8.0`      | 4–32    | Gap between pill edge and content       |
| `art_size`         | `32`       | 24–96   | Album-art square size (pill height is derived from this) |
| `font_size_title`  | `12.0`     | 8–32    | Track title font size                   |
| `font_size_artist` | `10.0`     | 8–28    | Artist (or source app) font size        |

Colors are `[R, G, B, A]` with 0–255 components. The alpha channel is used
for compositing; a value below 255 makes the pill slightly translucent.

## Logging

Logging has no configuration. A single `log-Live.log` file in
`<data_dir>\logs` captures the current run and is truncated at startup; no
history is retained.

## [main window] and the system tray

Notch keeps a maximized tracking window alongside the notch pill. The window shows
the current activity (art, state, title/artist/album) and the per-session history;
it is opened from the tray icon (double-click, or the **Open Notch** menu item).
It is never shown as a pop-up on launch.

The tray menu mirrors the `[behavior]` toggles in real time:

- **Open Notch** — restore the tracking window.
- **Toggle notifications** — enable/disable SMTC track-change + state-change events.
- **Start with Windows** — write/remove the `%APPDATA%\...\Run` registry entry.
- **Close window to tray** — on-off (mirrors `close_to_tray`).
- **Quit** — stop the process and remove the tray icon.

## Compact notch defaults

The shipped defaults produce a slim pill: 240 px wide, height derived from
`art_size` (32 px) plus padding — about 52 px tall — anchored 8 px below the
top of the work area, near-black background, small 32 px artwork, and compact
two-line title/artist text. All of these can be widened or recolored here.
