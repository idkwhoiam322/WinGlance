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
| `position`     | `"top-center"` | `top-center` \| `top-right` \| `top-left` | Horizontal placement within the monitor work area |
| `max_width`    | `240`   | 180–800 | Maximum pill width in logical pixels              |
| `margin_top`   | `8`     | 0–500   | Distance from the top edge of the work area       |

## [behavior]

| Key                             | Default | Range  | Effect                                  |
|---------------------------------|---------|--------|-----------------------------------------|
| `enable_track_change`           | `true`  | bool   | Show the pill when the track changes    |
| `enable_playback_state_change`  | `true`  | bool   | Show a small state pill on play/pause   |
| `debounce_ms`                   | `200`   | 150–250 | Coalescing window for bursty SMTC events |

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

## Compact notch defaults

The shipped defaults produce a slim pill: 240 px wide, height derived from
`art_size` (32 px) plus padding — about 52 px tall — anchored 8 px below the
top of the work area, near-black background, small 32 px artwork, and compact
two-line title/artist text. All of these can be widened or recolored here.
