# WinGlance Feature Research

Research date: 2026-08-16. Scope: exhaustive, independent enhancement research
for WinGlance across UI/UX, featureset, security, performance and memory —
based on similar now-playing/SMTC software, the SMTC platform itself, and
community/ecosystem sources (GitHub, Reddit, Hacker News, Microsoft Learn,
Stack Overflow, blogs, issue trackers). This document is self-contained and
does not rely on `docs/research.md`; where the repo already implements an idea
it is marked [EXISTS ...] and can be ignored or revisited.

Repository facts cited below (src file:line) were read from the working tree
on 2026-08-16 and re-verified in a second review pass the same day; sections
2.2 #12, 2.3, 3, 4 and the roadmap reflect that review. GitHub star counts
were verified via the GitHub API on 2026-08-16.

How to read this document: each section lists a feature idea, the
evidence/source for it, an effort rating (S = <1 day, M = 2–5 days,
L = 1–3 weeks), expected impact, and implementation notes. The first section
is the requested Acrylic Material effort assessment.

---

## 1. Windows 11 Acrylic Material background blur — effort assessment (REQUESTED FIRST)

### 1.1 What Acrylic is

- Acrylic Material (Fluent Design): a translucent surface = background blur +
  colour tint + subtle noise texture. Used for transient surfaces (flyouts,
  menus, overlays).
- Windows 11 (build 22621+/22H2+): the documented way to get it in Win32 is
  the DWM system backdrop:
  - `DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMSBT_TRANSIENTWINDOW)` → Desktop Acrylic, "brightest variant"
    (Microsoft Learn `DWM_SYSTEMBACKDROP_TYPE`; "System backdrops
    (Mica/Acrylic)" article, ms.date 2026-07-06).
  - `DWMSBT_MAINWINDOW` = Mica (wallpaper-derived, NOT a live blur — not what
    is requested).
- Windows 10: no DWM system backdrop. The only API is the **undocumented**
  `SetWindowCompositionAttribute(ACCENT_ENABLE_ACRYLICBLURBEHIND)` —
  historically buggy/degraded on Win10 1903+ and unreliable on Win11
  (evidence: Avalonia issue #6465 "Acrylic Blur does not work on Windows 11",
  QuickLook #955; Avalonia maintainers recommend against it).

### 1.2 The core constraint: today's pill window is layered

- WinGlance's pill is a `WS_EX_LAYERED` window driven by `UpdateLayeredWindow`
  (per-pixel alpha, click-through, rounded pill + aura).
- **DWM system backdrops do not render for layered windows.** Layered windows
  bypass DWM's normal background pass (their pixels come entirely from the
  redirection surface), so `DWMWA_SYSTEMBACKDROP_TYPE` is a no-op on the
  current window. This is community-verified (Electron "transparent windows"
  reports; Stack Overflow "Apply blur behind window effect on layered window
  with UpdateLayeredWindow"; working implementations in MicaForEveryone /
  window-vibrancy all use non-layered windows).
- Therefore "just call DwmSetWindowAttribute" will do nothing today. The
  implementation must either change the window type (option A) or paint the
  blur itself (option B).

### 1.3 Option A — convert the pill to a non-layered window + DWM acrylic

Mechanics:
- Create the pill WITHOUT `WS_EX_LAYERED`; set
  `DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_TRANSIENTWINDOW` on it.
- Click-through is preserved by the existing `WM_NCHITTEST → HTTRANSPARENT`
  path (this already works without layering; `WS_EX_TRANSPARENT` can be
  dropped). `WS_EX_NOACTIVATE` stays.
- The 60 Hz `GetMessageW` UI loop is unchanged.
- The raw recipe is well-documented and works with the `windows` crate as
  WinGlance already uses: `DwmExtendFrameIntoClientArea(-1 margins)` then
  `DwmSetWindowAttribute` (microsoft/windows-rs issue #2189, sample by
  riverar; `DwmSetWindowAttribute` is exposed in
  `windows::Win32::Graphics::Dwm`, verified in windows 0.62 docs).

What breaks / needs rework:
- **Per-pixel alpha is lost.** Rounded corners must come from
  `DWMWA_WINDOW_CORNER_PREFERENCE` (system radius — NOT the configurable
  pill radius, which is larger) or `SetWindowRgn` (aliased, un-antialiased
  edges). The current configurable corner radius/rim look will change.
- **The aura glow (a soft halo outside the pill) is impossible** on a
  non-layered window. Workaround: a second, layered "glow-only" window that
  tracks the pill (two windows to keep in sync, extra complexity during
  morph/expand/position changes).
- Content painting over the translucent backdrop: GDI text/art/icons still
  draw normally, but the pill's palette-tinted fill is replaced by the
  acrylic surface; the tint must be re-expressed via the tint colour layer.

Cost: hardware-backed, essentially free CPU, official API. Best visual
"Windows 11 native" result with the OS's own glass.

### 1.4 Option B — keep the layered window, paint acrylic in software

Mechanics (pure Win32 GDI, no new dependencies):
1. While the pill is visible, `BitBlt` the screen region behind the pill from
   `GetDC(NULL)` at 2× downscale into a scratch buffer.
2. Run a separable Gaussian blur (small kernel, e.g. radius 6–12 px at
   downscaled resolution) — sub-millisecond at pill size in release builds.
3. Blend the palette-derived tint over the blurred backdrop (reuse the
   existing `[appearance] opacity`/accent pipeline → same palette aesthetic).
4. Optionally blend a precomputed 128×128 noise tile at ~2–4% alpha for the
   authentic Acrylic noise.
5. Composite into the existing per-pixel DIB and `UpdateLayeredWindow` as
   today.

Cadence: capture once on pill open/position change; if live desktop motion
under the pill matters, refresh at 10–30 fps (still cheap at pill size).

What is preserved: per-pixel alpha, rounded corners, rim, aura, click-through,
exact current architecture — **the whole visual pipeline stays**. Windows 10
gets the same blur (good cross-version symmetry).

Gotchas:
- The capture includes the pill's own pixels if the pill is on screen during
  capture — acceptable because the pill fill is composited at full opacity on
  top, and edges are blurred anyway (minor halo at sub-pixel edges).
- Respect Windows' "Transparency effects" setting (HKCU
  `Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\
  EnableTransparency`) — when off, fall back to the current solid tinted
  fill (this is what DWM does for real acrylic).
- Multi-monitor/per-monitor DPI: map logical pill rect → physical pixels for
  the capture rect.
- Memory: a few MB scratch buffer; release when hidden.

### 1.5 Option C — SetWindowCompositionAttribute (undocumented)

Not recommended: undocumented, historically broken/degraded on Win11
(Avalonia #6465 on 22000), no tint/alpha control, no roadmap guarantee. The
only legitimate use would be a Win10-only fallback, but Option B already
covers Win10 with a consistent look.

### 1.6 Palette-aesthetic compatibility

- The acrylic tint layer can be driven by the same C1/C2 palette extraction
  that today tints the fill: tint colour = existing effective background
  colour at ~15–25% alpha over the blurred backdrop. The aura, rim and text
  colours are untouched.
- Verify contrast: acrylic brightens the surface on light desktops; the
  existing title/artist colours may need a subtle text shadow or a slightly
  darker tint in Light theme.

### 1.7 Effort conclusion (the answer to the question)

- **Option B (software acrylic): S–M (≈2–4 focused days)**, zero new
  dependencies, no architecture change, preserves all current visuals,
  works on Win10 and Win11. Recommended as the primary implementation.
- **Option A (true DWM acrylic): M (≈3–5 days + regression pass)** for
  Win11 22H2+ only; loses per-pixel-alpha effects unless the two-window glow
  trick is added (adds ≈1–2 days). Best "native" look.
- **Both/combined:** implement B first; later add A behind a build-version
  gate (≥22621) and keep B as the Win10 fallback.

### 1.8 Reference implementations (verified 2026-08-16)

- `tauri-apps/window-vibrancy` (~1.0k★, API-verified): applies Mica/Acrylic
  on Windows via DWMWA_SYSTEMBACKDROP_TYPE (with older-build fallbacks);
  Tauri-ecosystem maintained.
- `microsoft/windows-rs` issue #2189: official Microsoft sample by riverar —
  `DwmExtendFrameIntoClientArea(-1 margins)` + `DWMWA_SYSTEMBACKDROP_TYPE`
  using the `windows` crate (`Win32_Graphics_Dwm`) — the canonical raw
  Win32/Rust recipe for this feature.
- `ALTaleX531/Win32Acrylic`: standalone Win32 Acrylic/Mica brush
  implementation (no Win2D), cited by Mozilla (bugzilla 1764822) as a
  reasonable reference implementation.
- `MicaForEveryone/MicaForEveryone` (~5.2k★, API-verified): applies
  backdrops to ANY window; its Win10 path uses composition/undocumented
  APIs — useful reference for the software fallback.
- `selastingeorge/Win32-Acrylic-Effect` (262★, API-verified): catalogues
  every approach (SWCA, composition, magnification, DWM private API).
- `The-MuffinDev/LyricsOnTheGo` (C# WPF): ships exactly this
  "acrylic-glass card" pattern for a media overlay.
- Note: earlier research mentioned a `window-vibrancy-rs` Rust crate; GitHub
  API lookup returns 404 for every plausible owner path, so treat that
  reference as unverified and rely on the raw `windows` crate calls instead.

---

## 2. Feature catalog from similar software

### 2.1 The direct cohort (now-playing / media overlays)

- **FluentFlyout** (`unchihugo/FluentFlyout`, 3785★, API-verified; the
  current leader, effectively the ModernFlyouts successor):
  - Media flyout: artwork, title/artist, seek slider (when the player
    supports position), shuffle/repeat/loop, play/pause/next/prev.
  - "Up Next" flyout (what plays next) on track end; taskbar media widget.
  - Mica/acrylic appearance options, light/dark mode, deep customisation
    (layouts, positions), per-app auto-open suppression + exclusion lists.
- **Music Island** (`redheadesign/music-island`, 163★, API-verified,
  Rust + Tauri):
  - Top-edge "island" that hovers to expand, hover transport controls,
    progress bar, **preferred SMTC source** selection, accent colour,
    width/scale options, open delay, tray, portable self-update.
  - Opt-in direct integration with a specific player (Yandex) for
    low-latency metadata, like/dislike, active-wave chip — a pattern to note.
- **ModernFlyouts** (`ModernFlyouts-Community/ModernFlyouts`, 4065★,
  API-verified, **archived=True**): flyout stacking direction
  (horizontal/vertical), custom timeout, background opacity, per-module
  enable/disable, pin-to-monitor, drag-to-reposition the flyout itself,
  light/dark mode, transport controls; GSMTC support matrix confirming
  browsers' app-info is incomplete; many users report 24H2 breakage
  (community issues #1381, #1486).
- **LyricsBlossom** (`Eplorr/LyricsBlossom`, 183★, API-verified, Skia
  GPU/Vulkan): word-synced lyrics overlay, dynamic album-cover background,
  animated backdrop, global timeline compensation (±50 ms), hotkeys
  (Space, ←/→), **HD artwork workaround** (SMTC art is a small thumbnail —
  fetches better art from source-specific APIs where possible).
- **windows-notch-overlay** (`Taxperia/windows-notch-overlay`, 15★,
  API-verified, Electron): top-center Dynamic-Island-style pill with quick
  controls — same archetype as WinGlance's pill.
- **MusicFloat** (`Adudumax/MusicFloat`, 2★, API-verified, WPF): lightweight
  SMTC overlay (customizable track info).
- **obs-smtc-music-overlay** (`plaksych/obs-smtc-music-overlay`): SMTC →
  streaming overlay for OBS — evidence of the streaming-audience use case.
- **NowPlaying.WebSocket** (`mrgogo7/NowPlaying.WebSocket`, 3★): real-time
  now-playing + audio visualizer data over WebSocket (JSON) for OBS.
- **OverlayMusic** (`Loretiks/OverlayMusic`, 1★): always-on-top music
  overlay for games, cover-derived theming, Discord Rich Presence.
- **desktop-lyric** (`Epi-Lo/desktop-lyric`, 1★): lyrics overlay with
  auto-translation, works with Tidal/Spotify/any SMTC player.

### 2.2 UI/UX ideas (idea — evidence — effort)

1. **Hover transport controls on the pill** (play/pause/prev/next, + seek
   when the session exposes it) — Music Island, FluentFlyout, notch-overlay.
   WinGlance is deliberately passive; make it an OPT-IN toggle that, on hover,
   disables HTTRANSPARENT for the pill area and hit-tests small control zones;
   leaving the pill restores pass-through. M. Biggest UX gap vs. the cohort.
2. **Drag-to-reposition the live pill** (ModernFlyouts, Music Island) —
   existing drag-to-place covers this partially; direct drag on the pill is
   more discoverable. S–M.
3. **Shuffle / repeat indicators** — FluentFlyout/ModernFlyouts expose
   shuffle/repeat state from SMTC playback info. Verify the exact GSMTC
   API surface in the `windows` crate (`PlaybackInfo` shuffle/repeat
   fields); WinGlance currently ignores these. S.
4. **Hover-expand with progress** (Music Island): the compact/persistent
   pill could show a thin progress bar. S.
5. **Light/dark/system theme** — FluentFlyout, ModernFlyouts; WinGlance has
   `theme = "auto"` in config already [EXISTS]; polish window chrome/settings
   for it. S.
6. **Background opacity control per style** — ModernFlyouts; WinGlance has
   global `opacity` [EXISTS]; per-layout opacity knob. S.
7. **Per-app appearance/styles** — FluentFlyout/ModernFlyouts per-app
   suppression; extend to per-source accent/theme. S–M.
8. **"Up Next" preview** — FluentFlyout; SMTC does NOT expose the queue for
   the generic session, so this needs a direct source integration (Music
   Island pattern) or is infeasible for SMTC-only. L / low feasibility.
9. **Global hotkeys (show/recall/dismiss pill)** — Music Island has hotkeys;
   WinGlance has CLI but no hotkeys. OS media keys already exist; add
   optional user hotkeys. S.
10. **Reduced-motion support** (respect Windows animation setting; also an
    app toggle) — accessibility win. S.
11. **High-contrast mode adjustments** — detect `SPI_GETHIGHCONTRAST`;
    switch to solid high-contrast palette. (WinGlance already colors the
    title bar under high contrast — see commit b4a5297.) S.
12. **Screen-reader (UIA/MSAA) name on the pill window** — expose the
    current track as an accessible name; no focus steal (no activate). S
    (verify with Narrator).
    Review note (2026-08-16): the repo already ships a full UIA provider for
    the Settings pane — named controls, toggle/invoke patterns, focus and
    property-change events (src/accessibility.rs; wiring and teardown in
    src/main_window.rs:4941–4959, 5641–5671, 6117–6148), with
    `Win32_UI_Accessibility` enabled (Cargo.toml:36). The overlay pill itself
    has no accessibility exposure (no WM_GETOBJECT path in `src/overlay/`),
    so the work is a minimal read-only name provider on the pill HWND that
    follows that pattern. Constraint: it must stay non-focusable and
    non-clickable (the pill is passive by architecture).
13. **Multi-session stacking** (show 2+ sessions stacked) — FluentFlyout
    can list several sessions; WinGlance currently shows one. M.
14. **Screensaver/lock-aware auto-hide** — minor robustness. S.
15. **On-demand "peek" mode** (hold-to-show while hidden; useful in
    fullscreen) — niche, S–M.
16. **Dismiss/queue polish**: current 500 ms cap + hover-dismiss exists
    [EXISTS]; add an optional "swipe/click outside to dismiss"? Not
    click-through-compatible — skip unless opt-in.
17. **Taskbar media widget** — FluentFlyout ships one; out of scope for a
    passive pill (different surface). Note only.

### 2.3 SMTC-capability-driven ideas

- Artwork is a small system thumbnail; plan the HD-art workaround
  (LyricsBlossom) as an OPT-IN network feature only (Spotify/OAuth etc.);
  keep the default fully offline.
- Session identity: only `SourceAppUserModelId` is exposed — the icon
  fallback behaviour WinGlance already implements [EXISTS] is the right
  design; no better platform signal exists.
- Timeline events arrive only periodically (~5 s cadence) — the existing
  interpolation [EXISTS] is correct; ensure bounds clamping on seek.
- Stop vs pause + stale-thumbnail guards [EXISTS] — keep; they match known
  SMTC quirks (artwork populates ~500 ms after title; identity-switch stale
  art).
- Multiple active sessions: already ranked/handled [EXISTS].
- **Preferred-source pinning à la Music Island — spec: persistent pill
  only; keep showing other sources' events, but after a dismiss return to
  the pinned source's track. M. Review note (2026-08-16):**
  - Nothing like it exists: selection is foreground-first / arrival-order
    (`prioritize_sessions`, src/smtc.rs:2278); config has `media_sources`,
    `auto_compact_sources` and `hide_for_auto_compact_sources`
    (src/config.rs:277–289) but no pinned-source key.
  - Supporting machinery is present: the per-source `track_cache` retains
    entries indefinitely (no expiry timer) with the decoded cover intact
    and raw bytes stripped (src/overlay/mod.rs:546–566, 1439–1463), which
    is enough to re-show the pinned track after a dismiss; the per-window
    pending queues / `show_next` and the resume-hold (`held_content`)
    paths already let transient events from other sources display on a
    persistent pill; the source-picker UI pattern exists twice in
    src/process_picker.rs (media + auto-compact) for a third variant.
  - Design points to settle first: precedence between "return to pinned
    source" and the existing foreground-hold resume
    (`hide_for_auto_compact_sources`), and behavior when the pinned
    source's session closes (follow the "swap only to sources still
    playing" cache discipline, commit f8b5e42).
  - Guardrail: implement UI-side only — the worker keeps emitting every
    source (required by "show others"); the overlay decides what the
    persistent pill displays.

### 2.4 Ecosystem risk: Windows 11 24H2/25H2

- ModernFlyouts broke for many users after 24H2; the system media quick
  settings UX changed; community alternatives (FluentFlyout) now lead.
- WinGlance consumes GSMTC directly (no shell-UI dependency), so the 24H2
  drift affected third-party flyouts more than raw session APIs — but keep
  a manual test matrix on 24H2/25H2 + Insider builds in the release
  checklist.

---

## 3. Security enhancements

- **Maintain the zero-network posture.** WinGlance currently does no I/O
  beyond local config/logs — the strongest possible security property for a
  passive overlay. Any networked feature (HD art, RPC, update check) must be
  opt-in, TLS-only, and avoid loading remote content into the UI surface
  automatically.
- **Artwork decode hardening [EXISTS — verified 2026-08-16, no action
  needed beyond keeping dependencies current].** The decode path is already
  defense-in-depth correct:
  - `decode_limited` (src/events.rs:356–365) sets `image::Limits`
    (max 2048×2048, `ART_MAX_DIM` events.rs:351) BEFORE `decode()` — the
    decompression-bomb guard runs pre-allocation, which is the recommended
    pattern.
  - Per-thumbnail byte cap 4 MiB (`MAX_THUMBNAIL_BYTES`, src/smtc.rs:164)
    and 64 MiB total retained raw-artwork budget
    (`MAX_RETAINED_ARTWORK_BYTES`, src/smtc.rs:165); when the total would be
    exceeded the artwork bytes are dropped while metadata is retained (a
    placeholder renders instead of a stale cover).
  - Decode runs only on the SMTC worker thread, once per emitted track, at a
    fixed 256² (`ARTWORK_DECODE`, events.rs:343; ≤256 KB per cover backing
    store).
  - Format allowlist already in place: `image` is pinned with
    `default-features = false` and only `jpeg`, `png` (Cargo.toml:46).
  - Remaining action: keep `image` (0.25) current (its decoders have a long
    CVE history) — cargo-audit/cargo-deny already run in CI.
- **Binary hardening [EXISTS — verified 2026-08-16]**: release profile is
  `lto = "fat"`, `strip = "symbols"`, `codegen-units = 1` (Cargo.toml:51–55).
- **Release signing (open)** — EV/OV code-sign removes SmartScreen friction;
  effort depends on cert availability; also consider `signtool /pa` +
  publisher attestation.
- **Supply chain** — `deny.toml` is present and cargo-audit/cargo-deny run
  in CI [EXISTS]; open items: Dependabot for `Cargo.lock`, and a bump
  cadence for `windows` (0.58 pinned; 0.62 exists) and `image` (0.25).
- **Single-instance** — already handled (per-session `Global\WinGlance`
  token + keep-alive heartbeat, src/main.rs) [EXISTS]; keep the session
  suffix (two users can run their own instance).
- **Config** — plain `config.toml`, no secrets; a byte-size cap
  (`MAX_CONFIG_BYTES`, src/config.rs:1648) and range normalization on load
  (:757) exist [EXISTS] — nothing more needed.
- **Hostile-source hygiene [EXISTS — verified 2026-08-16]** — icon-path
  sanitization rejects a hostile `source_app` (src/icon.rs:156); the
  session/source admission caps bound attacker-controlled registration
  (src/smtc.rs:147–155).
- **No admin rights / no service / no telemetry** — document as the
  security model in README.

---

## 4. Performance & memory enhancements

### 4.1 Rendering (GDI / layered window)

- **Dirty-frame skip [EXISTS — verified 2026-08-16, no action]**: `tick()`
  calls `render()` only when something can have changed — layout flip,
  animation, hover morph, marquee, ≥1 px progress-bar movement, or a
  persistent fade — so a settled pill never repaints on the 250 ms static
  tick (src/overlay/mod.rs:2563–2571; bar threshold :2196–2214).
- **Marquee band caching [EXISTS — verified 2026-08-16, no action]**: each
  scrolling line is rasterized once and cached (`MarqueeStrip`,
  src/overlay/mod.rs:253–265), rebuilt only on content/size/font/color
  change (src/overlay/render.rs:2466–2497), and scrolled by sampling the
  cached strip (:2571). Art/icon/palette rows render from cached buffers and
  state pills reuse the cached track's cover (render.rs:477–485).
- **Geometry-change guard [EXISTS — verified 2026-08-16, no action]**:
  layout re-check is cadenced to ≥1 s (`tick_layout_check`,
  src/overlay/mod.rs:2091–2107); a layout flip forces a render, otherwise
  `reposition()` moves the window without re-blitting (:3000–3006); the
  topmost re-assert is throttled to 1 Hz (:2222–2242); the tick cadence
  drops from refresh-rate to 250 ms when nothing animates
  (`sync_anim_timer`, :1003–1049).
- **Backdrop (Acrylic Option B) cost**: capture+blur at pill scale is
  <1 ms/frame; keep idle refresh ≤10 fps or event-driven — negligible.
- **DirectComposition as a future rewrite**: hardware composition, no CPU
  copy, better battery — but requires a D3D11 device + swapchain and an
  architecture change; only worth it if/when the pill becomes heavy
  (animated backdrop, album-art video). L, low priority.

### 4.2 Memory

- Existing bounds are sane (verified 2026-08-16): thumbnail read cap 4 MiB
  per image (`MAX_THUMBNAIL_BYTES`, src/smtc.rs:164), 64 MiB total retained
  raw art (`MAX_RETAINED_ARTWORK_BYTES`:165), fixed 256² decode (≤256 KB per
  cover), session/source caps (64 sessions / 32 sources:154–155), signal
  queue 256 (:145), output retry mailbox 256 (:170), per-window queues 256,
  1 MiB log cap [EXISTS]. The UI-side `track_cache` is LRU-capped at 3 and
  strips raw artwork bytes on insert, retaining only the decoded cover
  (typically 50–500 KB) per source (src/overlay/mod.rs:222–226, 1439–1463).
- **Transient-art queue bound (CHECK — narrowed 2026-08-16)**: the retained
  side is bounded (above); the residual question is in-flight depth — can a
  burst of art-bearing `TrackChanged` events transiently hold many × 4 MiB
  in the 256-entry queues before supersede/coalesce at receive? Audit the
  emit/receive burst path (worst case: startup or a session storm); if real,
  cap art-bearing pending events or decode to 256² before queueing.
  S (audit) / M (fix only if the audit finds a real case).
- Make retained-thumbnail budget configurable (users on 8 GB RAM). S.
- Release the backdrop scratch buffer when the pill hides (Option B). S.

### 4.3 Startup / power

- **SMTC worker starts at startup and runs permanently [EXISTS — verified
  2026-08-16]**: it keeps reading sessions even while notifications are off
  (src/smtc.rs:3363); do NOT lazy-start it (would miss early events). Only
  the icon worker is lazy — by design, with a capped job queue
  (src/icon.rs:168).
- **No refresh-rate timer while hidden [EXISTS — verified 2026-08-16]**:
  `hide()` deletes the animation timer (src/overlay/mod.rs:3036); `tick()`
  early-returns on `Phase::Hidden` except the auto-hide watchdog
  (:2147–2153), and that watchdog is a 1 Hz foreground-only poll that
  disarms itself (:2116–2138). A static shown pill runs at 250 ms
  (:1024–1026, :1038) and, per the 4.1 render gate, paints nothing.
- Icons: bounded worker [EXISTS] is correct; keep bounded cache.

---

## 5. SMTC platform notes (in general)

- Metadata surface: title, artist/subtitle, album, track number, thumbnail
  (small), playback type, timeline (position/duration/Rate), enabled
  controls, shuffle/repeat state. NO: lyrics, rating, queue, app display
  name/icon (only AUMID), full-size artwork, per-track quality info.
- Timeline events are coarse (~5 s); always interpolate.
- Session churn is high (browser tabs, players restarting); identity is the
  AUMID; some apps reemit the same track repeatedly (dedupe needed —
  WinGlance has guards [EXISTS]).
- Browsers (Chrome/Edge/Firefox) mark app info incomplete — the icon gap is
  a platform limitation, not WinGlance [EXISTS/known].
- Windows 11 24H2 changed the media quick-settings surface; GSMTC itself
  continues to work; watch future releases via Insider feedback.

---

## 6. Prioritised roadmap (suggested)

| Priority | Item | Section | Effort |
|---|---|---|---|
| P1 | Acrylic software path (Option B), version-gated | 1 | S–M |
| P1 | Artwork decode: keep `image`/`windows` current (guard already correct) | 3 | S |
| P1 | Transient-art in-flight queue audit (narrowed) | 4.2 | S–M |
| P1 | Shuffle/repeat indicators (verify API) | 2.2 #3 | S |
| P1 | Hover transport controls (opt-in) | 2.2 #1 | M |
| P2 | Acrylic DWM path (Option A) on 22621+ with glow window | 1 | M |
| P2 | Drag the live pill | 2.2 #2 | S–M |
| P2 | Light/dark/system theme polish; high-contrast; reduced-motion | 2.2 #5/10/11 | S–M |
| P2 | Preferred-source pinning (persistent pill: show others, return to pinned after dismiss) | 2.3 | M |
| P2 | Per-app styles | 2.2 #7 | S–M |
| P2 | UIA name on pill + Narrator check | 2.2 #12 | S |
| P3 | Multi-session stacking | 2.2 #13 | M–L |
| P3 | DirectComposition rewrite | 4.1 | L |
| P3 | HD-art/lyrics/queue via opt-in source integrations | 2.2 #8, 2.3 | L |

Already implemented in the current tree and therefore absent from the
roadmap: 4.1 dirty-frame skip / marquee band caching / geometry-change
guard, 4.3 startup and hidden-timer items, the artwork-decode hardening
(reduced to keeping dependencies current), and the single-instance / config
caps / icon-hygiene / binary-profile security items.

## 7. Sources

- Microsoft Learn: `DWM_SYSTEMBACKDROP_TYPE`; "System backdrops
  (Mica/Acrylic)" (ms.date 2026-07-06); `DwmSetWindowAttribute`;
  `GlobalSystemMediaTransportControlsSessionManager`;
  `GlobalSystemMediaTransportControlsSessionPlaybackInfo`.
- GitHub (all replies verified via GitHub API 2026-08-16):
  `unchihugo/FluentFlyout` (3785★), `ModernFlyouts-Community/ModernFlyouts`
  (4065★, archived; community issues #1381/#1486),
  `redheadesign/music-island` (163★), `Eplorr/LyricsBlossom` (183★),
  `Adudumax/MusicFloat` (2★), `Taxperia/windows-notch-overlay` (15★),
  `plaksych/obs-smtc-music-overlay`, `mrgogo7/NowPlaying.WebSocket` (3★),
  `Loretiks/OverlayMusic` (1★), `Epi-Lo/desktop-lyric` (1★),
  `The-MuffinDev/LyricsOnTheGo` (9★),
  `MicaForEveryone/MicaForEveryone` (~5.2k★),
  `tauri-apps/window-vibrancy` (~1.0k★),
  `selastingeorge/Win32-Acrylic-Effect` (262★),
  `ALTaleX531/Win32Acrylic`, microsoft/windows-rs issue #2189 (riverar
  sample).
- Issues/trackers: Avalonia #6465 (acrylic broken on Win11); QuickLook #955;
  Mozilla bugzilla 1764822 (Mica/acrylic for title bar, cites
  ALTaleX531/Win32Acrylic).
- Community: Reddit (r/Windows11, r/software flyout-recommendation threads);
  Stack Overflow (ULW performance Q 3169258; layered-window blur
  Q 55684127); windows-rs docs for `DwmSetWindowAttribute` (windows 0.62 /
  windows-sys 0.61).
- Microsoft docs for Windows 11 24H2 release-health; Electron transparent
  window docs (layered vs DWM backdrop).