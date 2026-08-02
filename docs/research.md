# Competitive research: Windows media overlay apps

Research date: 2026-08. Goal: find similar "now playing" overlays for Windows and
extract ideas for improving Notch. Sources: GitHub, the ModernFlyouts wiki and its
GSMTC support matrix.

## Apps analyzed

### ModernFlyouts (4.1k stars, archived Nov 2025)
The closest mainstream equivalent: a Fluent-Design flyout system for Windows that
replaces the Metro media/volume/brightness flyouts. Its media session module is the
reference implementation for an SMTC-driven overlay.

Strong features we do not have yet:
- **Draggable flyout with auto-save of position** (we have a drag-to-place
  positioner; ModernFlyouts lets the user drag the flyout itself and remembers it).
- **Custom timeout** (we have a Duration submenu; same idea).
- **Background opacity control** (we expose alpha in `[appearance]` only via config).
- **Multi-monitor selection** (we auto-follow the foreground monitor; they let the
  user pin a specific monitor).
- **Flyout stacking direction / alignment** (horizontal vs vertical content flow).
- **Media transport controls**: shuffle, repeat, stop — clickable from the flyout.
- **Timeline / progress bar** when the app exposes position info.
- **Per-module enable/disable** (volume, media, brightness independently).
- **Light/dark mode**.

Their GSMTC support matrix (docs/GSMTC-Support-And-Popular-Apps.md) is the most
useful artifact. Key takeaways:

- Browsers (Chrome, Edge, Chromium, Firefox) support thumbnail/title/artist but
  **App Info is marked incomplete** — i.e. even the reference app cannot reliably
  show the source-app icon/name for browsers. This confirms the app-icon gap is a
  platform limitation of SMTC, not a Notch bug. We already surface the readable
  source-app name.
- Timeline info (progress bar) exists only for a handful of apps (Spotify,
  Dopamine, FeelUOwn, Rise, Groove). Most players and all browsers report nothing.
- Shuffle/repeat are niche (Spotify, Groove, MusicBee, FeelUOwn); stop is common.
- MPV/VLC/Winamp/iTunes etc. need plugins (gen_smtc, MPV-SMTC, vlc-win10smtc) to
  expose SMTC at all; several popular players (AIMP, iTunes) are partial.

### Other projects in the "now playing" ecosystem
- **tauri-plugin-media**: SMTC plugin for Tauri (Rust, like us). Confirms SMTC is
  the correct integration point for Windows media metadata.
- **Deskband11Lib**: embeds WinUI/WPF widgets into the Windows 11 taskbar; a
  taskbar-hosting option for always-visible now-playing widgets.
- **Spotify-only overlays** (Nowify for OBS, various Discord RPC tools): a large
  part of the ecosystem is Spotify-specific via its local Web API. Being SMTC-based
  we cover every app, which is our differentiator.
- **macOS references** (MediaMate, NowPlaying menu-bar apps): macOS has first-class
  `MPNowPlayingInfoCenter`/`MRMediaRemote` metadata incl. app icon + artwork, which
  is why macOS overlays look "complete". Windows SMTC does not expose source-app
  icons directly.

## Improvement roadmap for Notch

Prioritized by impact/effort:

1. **Source-app icon** — obtain a real app icon from `SourceAppUserModelId`:
   resolve the AUMID to a display name + icon via
   `IShellItemImageFactory` / package manager. Uncertain results for browsers
   (matrix says incomplete) but should work for Store/UWP apps (Spotify, Groove).
   Fallback: draw a colored letter avatar (first letter of source-app name).
2. **Progress bar / timeline** — subscribe to `PlaybackInfoChanged` and render
   `Position/EndTime` when both are non-zero; hide the bar for apps that report
   none (most browsers). Requires a lightweight 1 Hz re-render while visible.
3. **Clickable pill controls** — play/pause, next/prev, stop via
   `GlobalSystemMediaTransportControlsSession::TryPlayAsync` etc. Requires making
   the pill hit-testable (it is currently `WS_EX_TRANSPARENT` / click-through);
   an opt-in mode.
4. **Monitor pinning** — setting to lock the pill to a specific monitor instead of
   the foreground one.
5. **Content stacking direction + alignment** — horizontal (art left, text right)
   vs vertical layout, mirroring ModernFlyouts.
6. **Light/dark appearance presets** — one-key `[appearance]` presets.
7. **Taskbar deskband / compact widget** — long term, a mini always-visible
   indicator using the Deskband11Lib approach.

## What we already do that matches or beats the reference

- Single self-contained exe, no runtime/framework dependency.
- Click-through, focus-free overlay (ModernFlyouts flyouts steal focus/input).
- Auto-follow of the foreground monitor's work area + snap-to-edge positioner.
- Content-only dedup + progressive-metadata merge (album/artwork arrive once ready;
  no duplicate notifications) — implemented after observing ModernFlyouts-era
  complaints about double flyouts.
- Full session history with status, sidebar panes, tray-driven settings.
