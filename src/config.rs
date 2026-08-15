use log::{debug, info, warn};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Upper bound on the config file size, in bytes. A file above this bound is
/// treated as unreadable: defaults apply in memory, the file is left
/// untouched, and persistence is disabled — so a hostile or corrupt
/// oversized file can neither be parsed nor be overwritten by a save.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Byte-identical snapshot of `config.toml` as it was loaded (or as it was
/// last written). `save_checked` re-reads the current file and compares it
/// against this snapshot, so an external edit — a hand-edit, or another
/// instance that saved after us — is detected as a conflict instead of being
/// silently overwritten. The bytes are kept as-is: no hash, so no
/// collision risk and no new dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRevision {
    bytes: Arc<[u8]>,
}

impl ConfigRevision {
    /// Captures the exact file bytes. `file_bytes` must already be bounded by
    /// `MAX_CONFIG_BYTES`; an unbounded buffer is never captured here.
    fn captured(file_bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: file_bytes.into(),
        }
    }

    fn matches(&self, file_bytes: &[u8]) -> bool {
        &*self.bytes == file_bytes
    }
}

/// The verdict of a `save_checked` call. `Saved` carries the revision of the
/// bytes just written, so the caller can install it as the new shared
/// revision only after the write actually succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved(ConfigRevision),
    /// The file changed on disk since it was loaded: nothing was written and
    /// the in-memory change applies for this session only.
    Conflict,
    /// The startup file was invalid, unreadable, or oversized: it is left
    /// untouched and nothing is ever persisted this run.
    PersistenceDisabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub overlay: OverlayConfig,
    pub behavior: BehaviorConfig,
    pub appearance: AppearanceConfig,
    /// Anything in config.toml this build does not know about (written by a
    /// newer version or hand-edited). Captured and re-emitted on save so a
    /// settings change never silently deletes unknown fields from disk.
    #[serde(flatten)]
    pub unknown: toml::Table,
    /// Whether `save()` may write config.toml. False only when the existing
    /// file was invalid or unreadable and was left untouched: the user's file
    /// must never be overwritten with defaults, so settings apply in memory
    /// for that run and nothing is persisted. Never serialized.
    #[serde(skip)]
    pub persistable: bool,
    /// The byte snapshot the last load or save was based on (absent when
    /// persistence is disabled). `save_checked` compares the live file against
    /// it and refuses to write on a mismatch. Never serialized.
    #[serde(skip)]
    pub revision: Option<ConfigRevision>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            overlay: OverlayConfig::default(),
            behavior: BehaviorConfig::default(),
            appearance: AppearanceConfig::default(),
            unknown: toml::Table::new(),
            persistable: true,
            revision: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub duration_ms: u64,
    pub animation_ms: u64,
    pub vertical: VerticalPosition,
    pub horizontal: HorizontalPosition,
    pub margin: i32,
    pub max_width: u32,
    /// Caps the overlay's animation tick rate to this many Hz. The pill animates
    /// at most at this refresh; on higher-refresh monitors the UI thread is
    /// throttled down to it. The cap only limits repaint frequency — motion
    /// stays time-based (see `overlay::sync_anim_timer`). Normalized into the
    /// range [60, 1000]: values at or below 60 keep the default 60 Hz cap, and
    /// values above 1000 are clamped to 1000. Configurable only via config.toml,
    /// not the Settings UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tick_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_y: Option<i32>,
    /// Which display the pill is placed on (see `MonitorMode`).
    pub monitor: MonitorMode,
    /// Which pill layout is used (see `LayoutMode`).
    pub layout: LayoutMode,
    /// Whether the Compact layout uses its own independent position
    /// (`compact_*` below). While `false`, the Compact layout always follows
    /// the Expanded position (`vertical`/`horizontal`/`margin`/`position_x`/
    /// `position_y`/`monitor`) in both effective behavior and the settings
    /// UI; the independent fields are retained for later restoration but
    /// never consulted. See `compact_effective`.
    pub compact_position_separate: bool,
    /// Independent Compact position, used only while
    /// `compact_position_separate` is `true` and the effective layout is
    /// Compact. When separation is first enabled and these fields are still
    /// at their defaults (never customized), the UI initializes them from
    /// the current Expanded position — Compact never starts from a
    /// hard-coded spot.
    pub compact_vertical: VerticalPosition,
    pub compact_horizontal: HorizontalPosition,
    pub compact_margin: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_position_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_position_y: Option<i32>,
    /// Which display the Compact pill is placed on while it uses its own
    /// position (see `MonitorMode`).
    pub compact_monitor: MonitorMode,
    /// Hovering a pill in the *Expanded* layout arms its dismissal: the
    /// remaining time is capped at 500 ms, one-way (see `EARLY_EXIT_MS`).
    /// For pills in the Compact layout it makes the second hover dismiss
    /// (see `expand_compact_on_hover`): the first hover expands, later
    /// hovers dismiss. While off, no hover ever dismisses a pill.
    pub dismiss_on_hover: bool,
    /// Hovering a pill in the *Compact* layout expands it in place (see the
    /// hover morph): the countdown resets to the full duration, the expanded
    /// state is held while the cursor stays on it (it is an interaction,
    /// never dismissed mid-read), and leaving collapses it back to compact
    /// and resets the countdown again. With `dismiss_on_hover` enabled, the
    /// first hover of a showing expands and later hovers dismiss (the second
    /// hover dismisses); without it, every hover re-expands and resets.
    /// While off, a Compact pill behaves exactly like an Expanded one.
    pub expand_compact_on_hover: bool,
    /// When `layout = "persistent-compact"`, fade the pill to 25% idle
    /// opacity after `duration_ms` elapses. Off: the pill stays at full
    /// opacity while media is playing (no idle fade), and hides after
    /// `duration_ms` when nothing is playing (paused or stopped) instead of
    /// lingering. Fullscreen/listed-foreground hiding
    /// (`hide_for_auto_compact_sources`) applies either way. Default: `true`.
    pub fade_persistent_pill: bool,
    /// Unknown keys under `[overlay]`, preserved across saves.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerticalPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HorizontalPosition {
    #[default]
    Center,
    Left,
    Right,
}

/// Which pill layout is in effect. Serialized as a string so a hand-edited
/// `config.toml` stays readable:
///
/// ```toml
/// layout = "expanded"           # the full four-row pill (default)
/// layout = "compact"            # single-line pill: small art, title, app icon,
///                               # playback symbol
/// layout = "auto"               # compact while a configured source app is the
///                               # foreground app or a genuine fullscreen window is
///                               # foreground; expanded otherwise
/// layout = "persistent-compact" # always-visible compact pill while media plays;
///                               # fades to idle opacity after the dismiss timeout;
///                               # optionally hides over fullscreen / listed apps
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    #[default]
    Expanded,
    Compact,
    Auto,
    PersistentCompact,
}

/// Which display the overlay pill is placed on. Serialized as a string so a
/// hand-edited `config.toml` stays readable and unambiguous:
///
/// ```toml
/// monitor = "active-window"   # monitor of the foreground window (default)
/// monitor = "primary"         # the display marked primary in Windows
/// monitor = "index-2"         # the third active display (zero-based)
/// ```
///
/// `Index(n)` is resolved against the *current* enumeration of active
/// displays every time the pill is placed; an index that is temporarily
/// out of range (a display unplugged or reordered after the config was
/// saved) falls back to the primary display at placement time while the
/// configured value is preserved, so it becomes valid again automatically
/// when the display comes back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MonitorMode {
    /// Preserves the behavior of configs written before the field existed.
    #[default]
    ActiveWindow,
    Primary,
    Index(u32),
}

impl Serialize for MonitorMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = match self {
            Self::ActiveWindow => "active-window".to_string(),
            Self::Primary => "primary".to_string(),
            Self::Index(index) => format!("index-{index}"),
        };
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for MonitorMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.as_str() {
            "active-window" => Ok(Self::ActiveWindow),
            "primary" => Ok(Self::Primary),
            _ => text
                .strip_prefix("index-")
                .and_then(|digits| digits.parse::<u32>().ok())
                .map(Self::Index)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid monitor mode {text:?}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub enable_track_change: bool,
    pub enable_playback_state_change: bool,
    /// Whether the pill shows notifications. Persisted so a toggle survives a
    /// restart; the main window is the owner, the overlay mirrors it.
    pub notifications_enabled: bool,
    pub debounce_ms: u64,
    pub start_on_login: bool,
    pub start_in_tray: bool,
    pub close_to_tray: bool,
    /// Source apps (substrings, case-insensitive, matched against the AUMID and
    /// its derived label) whose SMTC sessions are followed. When empty, all
    /// non-cooldown sources are allowed (default). When non-empty, only matching
    /// sources generate pill notifications.
    pub media_sources: Vec<String>,
    /// Source apps (same form, identity and matching rules as `media_sources`)
    /// that force the pill into the Compact layout while `layout = "auto"` and
    /// one of them is the foreground app, or hide the pill while
    /// `layout = "persistent-compact"` and `hide_for_auto_compact_sources` is
    /// enabled.
    pub auto_compact_sources: Vec<String>,
    /// When `layout = "persistent-compact"`, hide the pill while a fullscreen
    /// window or a listed `auto_compact_sources` app is the foreground window,
    /// and resume the held content when the foreground clears. Default: `true`.
    pub hide_for_auto_compact_sources: bool,
    /// Unknown keys under `[behavior]`, preserved across saves.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub background_color: [u8; 4],
    pub text_color: [u8; 4],
    pub accent_color: [u8; 4],
    pub corner_radius: f32,
    /// Corner rounding of the Compact layout, in logical pixels. Independent
    /// of `corner_radius` (which keeps controlling the Expanded layout), so
    /// the two layouts can be rounded differently — the shipped default makes
    /// Compact a moderately rounded media card rather than a capsule.
    pub compact_corner_radius: f32,
    pub padding: f32,
    pub art_size: u32,
    pub font_size_title: f32,
    pub font_size_artist: f32,
    /// Unknown keys under `[appearance]`, preserved across saves.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            duration_ms: 5000,
            animation_ms: 500,
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Center,
            margin: 8,
            max_width: 340,
            max_tick_hz: Some(60),
            position_x: None,
            position_y: None,
            monitor: MonitorMode::default(),
            layout: LayoutMode::default(),
            compact_position_separate: false,
            compact_vertical: VerticalPosition::Top,
            compact_horizontal: HorizontalPosition::Center,
            compact_margin: 8,
            compact_position_x: None,
            compact_position_y: None,
            compact_monitor: MonitorMode::default(),
            dismiss_on_hover: true,
            expand_compact_on_hover: true,
            fade_persistent_pill: true,
            unknown: toml::Table::new(),
        }
    }
}

/// The resolved position the Compact layout actually uses: the independent
/// `compact_*` fields while separation is enabled, otherwise the current
/// Expanded position. Single source of truth shared by the overlay placement
/// and every settings/tray preview, so the UI can never show a position the
/// pill would not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactPosition {
    pub vertical: VerticalPosition,
    pub horizontal: HorizontalPosition,
    pub margin: i32,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub monitor: MonitorMode,
}

impl OverlayConfig {
    /// Whether the independent Compact position fields are still at their
    /// defaults — i.e. never customized. The settings UI uses this to decide
    /// whether a first enable of `compact_position_separate` may initialize
    /// them from the current Expanded position (Compact never starts from a
    /// hard-coded spot); once any field deviates — including edits made from
    /// the compact position row or tray submenu while the follow toggle is
    /// still ON — re-enabling separation restores the previously customized
    /// values instead.
    pub fn compact_is_default(&self) -> bool {
        // Derived from the struct's own defaults rather than hardcoded
        // literals, so a future default change cannot silently break the
        // copy-on-first-enable decision that leans on this.
        let defaults = Self::default();
        self.compact_vertical == defaults.compact_vertical
            && self.compact_horizontal == defaults.compact_horizontal
            && self.compact_margin == defaults.compact_margin
            && self.compact_position_x == defaults.compact_position_x
            && self.compact_position_y == defaults.compact_position_y
            && self.compact_monitor == defaults.compact_monitor
    }

    /// The position the Compact layout resolves to, per the separation rule:
    /// independent fields when `compact_position_separate` is set, otherwise
    /// the live Expanded position.
    pub fn compact_effective(&self) -> CompactPosition {
        if self.compact_position_separate {
            CompactPosition {
                vertical: self.compact_vertical,
                horizontal: self.compact_horizontal,
                margin: self.compact_margin,
                x: self.compact_position_x,
                y: self.compact_position_y,
                monitor: self.compact_monitor,
            }
        } else {
            CompactPosition {
                vertical: self.vertical,
                horizontal: self.horizontal,
                margin: self.margin,
                x: self.position_x,
                y: self.position_y,
                monitor: self.monitor,
            }
        }
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            enable_track_change: true,
            enable_playback_state_change: true,
            notifications_enabled: true,
            debounce_ms: 200,
            start_on_login: false,
            start_in_tray: true,
            close_to_tray: true,
            media_sources: Vec::new(),
            auto_compact_sources: Vec::new(),
            hide_for_auto_compact_sources: true,
            unknown: toml::Table::new(),
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            // Near-opaque fill (235 = ~92%): text, album art and symbols
            // composite fully opaque over it, so only the body lets a hint of
            // the backdrop through. Keep the alpha at ~224 (88%) or above —
            // below that the accent-colored meta text drops under WCAG AA
            // 4.5:1 over bright backdrops.
            background_color: [0x12, 0x14, 0x1C, 0xEB],
            text_color: [0xFF, 0xFF, 0xFF, 0xFF],
            // Single hardcoded pink accent, used across the pill, the sidebar
            // highlights and the window title bar. Deliberately not derived
            // from the Windows theme: arbitrary accent colors clash with the
            // pill's white text.
            accent_color: [240, 110, 155, 255],
            corner_radius: 26.0,
            // Moderately rounded so the slim Compact pill reads as a small
            // media card, not a capsule (a 26 px radius would nearly round
            // it end to end at the Compact height).
            compact_corner_radius: 12.0,
            padding: 15.0,
            art_size: 48,
            font_size_title: 16.0,
            font_size_artist: 13.0,
            unknown: toml::Table::new(),
        }
    }
}

impl AppearanceConfig {
    /// The corner radius (logical px) for the effective pill layout: the
    /// Compact layout's own radius when the pill is Compact, the Expanded
    /// radius otherwise. The caller passes the already-resolved effective
    /// layout — Auto has been decided into Expanded/Compact before any
    /// rendering — so the selected radius always matches what is drawn.
    pub fn effective_corner_radius(&self, compact: bool) -> f32 {
        if compact {
            self.compact_corner_radius
        } else {
            self.corner_radius
        }
    }
}

/// Reads the config file, retrying a bounded number of times so a transient
/// read failure (a momentary AV/indexer lock) does not count as a corrupt
/// config. Returns the last error when every attempt fails.
fn read_config_with_retry(config_path: &Path) -> std::io::Result<String> {
    let mut error = None;
    for attempt in 0..Config::READ_RETRIES {
        match std::fs::read_to_string(config_path) {
            Ok(content) => return Ok(content),
            Err(err) => {
                error = Some(err);
                if attempt + 1 < Config::READ_RETRIES {
                    std::thread::sleep(Config::READ_RETRY_DELAY);
                }
            }
        }
    }
    Err(error.unwrap_or_else(|| std::io::Error::other("read retries exhausted")))
}

impl Config {
    /// How often an unreadable config is re-read before it is treated as
    /// unreadable. A transient file lock (AV scan, indexer) usually clears
    /// within a few attempts; only a read that fails every retry counts.
    const READ_RETRIES: u32 = 3;
    /// Delay between read retries.
    const READ_RETRY_DELAY: Duration = Duration::from_millis(50);

    pub fn load() -> anyhow::Result<Self> {
        Self::load_from_path(&Self::config_path()?)
    }

    /// Rejects a config path whose declared length exceeds `MAX_CONFIG_BYTES`
    /// without buffering the file, so the bound also limits load-time memory
    /// (the post-read length check stays as the authoritative gate against a
    /// file that grows between the metadata query and the read). A missing
    /// file or an undetectable length falls through to the read, which
    /// reports its own error.
    fn exceeds_size_bound(config_path: &Path) -> bool {
        std::fs::metadata(config_path)
            .map(|meta| meta.len() > MAX_CONFIG_BYTES as u64)
            .unwrap_or(false)
    }

    /// Loads the config from `config_path`. When the file does not exist, a
    /// fresh default is written there. When it exists but cannot be read
    /// (after retries), parsed, or fits within `MAX_CONFIG_BYTES`, the file
    /// is left completely untouched and defaults apply in memory for this run
    /// with persistence disabled — an existing user config must never be
    /// moved, overwritten, or replaced with defaults, not even under a backup
    /// name.
    fn load_from_path(config_path: &Path) -> anyhow::Result<Self> {
        if !config_path.exists() {
            let mut config = Config::default();
            config.normalize();
            let bytes = config.serialized()?;
            Self::write_temp_and_rename(config_path, &bytes)?;
            config.revision = Some(ConfigRevision::captured(bytes));
            info!("no config at {config_path:?}; wrote defaults");
            return Ok(config);
        }
        if Self::exceeds_size_bound(config_path) {
            return Ok(Self::defaults_in_memory(&format!(
                "exceeds the {MAX_CONFIG_BYTES} byte size bound"
            )));
        }
        let content = match read_config_with_retry(config_path) {
            Ok(content) => content,
            Err(error) => {
                return Ok(Self::defaults_in_memory(&format!(
                    "could not be read after {} attempts ({error})",
                    Self::READ_RETRIES
                )));
            }
        };
        if content.len() > MAX_CONFIG_BYTES {
            return Ok(Self::defaults_in_memory(&format!(
                "exceeds the {MAX_CONFIG_BYTES} byte size bound"
            )));
        }
        match toml::from_str::<Config>(&content) {
            Ok(mut config) => {
                // The revision snapshots the exact bytes this load was based
                // on, so `save_checked` can prove the file was not edited
                // between now and the next save.
                config.revision = Some(ConfigRevision::captured(content.into_bytes()));
                // Report anything normalize() clamped: a value the user's
                // config.toml declares must never differ from the value in
                // effect without a visible log line.
                let before = config.clone();
                config.normalize();
                for change in Self::normalized_changes(&before, &config) {
                    warn!("{change}");
                }
                debug!("config loaded from {config_path:?}");
                Ok(config)
            }
            Err(error) => Ok(Self::defaults_in_memory(&format!("is not valid TOML ({error})"))),
        }
    }

    /// Defaults that apply in memory only: the existing config file is left
    /// untouched and `persistable` is cleared so `save()` can never write
    /// over it.
    fn defaults_in_memory(reason: &str) -> Self {
        warn!("config.toml {reason}; leaving it untouched and applying defaults for this run only");
        let mut config = Config::default();
        config.normalize();
        config.persistable = false;
        config
    }

    /// Persists the config only when the on-disk file still matches the
    /// revision this load/save cycle captured. A mismatch — anyone
    /// edited the file after we read it — is a `Conflict`: nothing is written
    /// and the in-memory change applies for this run only. `PersistenceDisabled`
    /// mirrors the legacy `persistable` guard: the startup file was invalid,
    /// unreadable, or oversized and is left untouched. On `Saved`, the shared
    /// revision is updated with the bytes just written.
    pub fn save_checked(&mut self) -> anyhow::Result<SaveOutcome> {
        let config_path = Self::config_path()?;
        self.save_checked_to(&config_path)
    }

    /// `save_checked` against an explicit path (shared by the tests, which run
    /// against temp dirs instead of real `%APPDATA%`).
    pub fn save_checked_to(&mut self, config_path: &Path) -> anyhow::Result<SaveOutcome> {
        let Some(revision) = self.revision.clone() else {
            warn!(
                "config.toml is not persistable this run (it was invalid or unreadable and was left untouched); settings apply until the app exits"
            );
            return Ok(SaveOutcome::PersistenceDisabled);
        };
        // Re-read the current file (bounded, retried like load). A re-read
        // failure means the file could not be verified — treat it as a
        // conflict rather than write blind. A file that has grown past the
        // size bound is also a conflict (it changed), detected here without
        // buffering it.
        if Self::exceeds_size_bound(config_path) {
            warn!(
                "config.toml has grown beyond the {MAX_CONFIG_BYTES} byte size bound; keeping the change in memory and NOT saving"
            );
            return Ok(SaveOutcome::Conflict);
        }
        let current = match read_config_with_retry(config_path) {
            Ok(content) => content,
            Err(error) => {
                warn!(
                    "could not verify config.toml before writing ({error}); keeping the change in memory and NOT saving"
                );
                return Ok(SaveOutcome::Conflict);
            }
        };
        if current.len() > MAX_CONFIG_BYTES || !revision.matches(current.as_bytes()) {
            warn!(
                "config.toml changed on disk since it was loaded (or the file has grown); keeping the change in memory and NOT saving"
            );
            return Ok(SaveOutcome::Conflict);
        }
        let bytes = self.serialized()?;
        Self::write_temp_and_rename(config_path, &bytes)?;
        let saved = ConfigRevision::captured(bytes);
        self.revision = Some(saved.clone());
        Ok(SaveOutcome::Saved(saved))
    }

    /// The deterministic serialized form `save` writes; kept separate so the
    /// freshly written bytes can also seed the next revision.
    fn serialized(&self) -> anyhow::Result<Vec<u8>> {
        Ok(toml::to_string_pretty(self)?.into_bytes())
    }

    /// Writes `config.toml` via a co-located temp file + same-volume rename,
    /// so a crash mid-write cannot leave a truncated config behind (the
    /// rename atomically replaces an existing file). The file is synced
    /// before the rename so a power loss cannot lose the settings change.
    /// A08 replaces this primitive with the handle-verified writer; the
    /// `save_checked` protocol around it is unchanged.
    fn write_temp_and_rename(config_path: &Path, content: &[u8]) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = config_path.with_extension("toml.tmp");
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            use std::io::Write;
            f.write_all(content)?;
            f.sync_all()?;
        }
        if let Err(e) = std::fs::rename(&tmp_path, config_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    }

    /// The on-disk location of `config.toml`, resolved the same way `save()`
    /// writes it. Exposed so the Settings "Open config" button can hand the
    /// exact file to the shell without guessing the data dir itself.
    pub fn config_path() -> anyhow::Result<PathBuf> {
        Ok(Self::data_dir()?.join("config.toml"))
    }

    pub fn data_dir() -> anyhow::Result<PathBuf> {
        let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("Could not find the Windows app-data directory"))?;
        Ok(base.join("WinGlance").join("WinGlance").join("data"))
    }

    pub fn logs_dir(&self) -> PathBuf {
        Self::data_dir().unwrap_or_else(|_| PathBuf::from("data")).join("logs")
    }

    fn normalize(&mut self) {
        self.overlay.duration_ms = self.overlay.duration_ms.clamp(500, 60_000);
        self.overlay.animation_ms = self.overlay.animation_ms.clamp(100, 1000);
        self.overlay.max_width = self.overlay.max_width.clamp(180, 800);
        self.overlay.max_tick_hz = self.overlay.max_tick_hz.map(|hz| hz.clamp(60, 1000));
        self.overlay.margin = self.overlay.margin.clamp(0, 500);
        self.overlay.compact_margin = self.overlay.compact_margin.clamp(0, 500);
        self.behavior.debounce_ms = self.behavior.debounce_ms.clamp(150, 250);
        self.appearance.corner_radius = self.appearance.corner_radius.clamp(4.0, 48.0);
        self.appearance.compact_corner_radius = self.appearance.compact_corner_radius.clamp(4.0, 48.0);
        self.appearance.padding = self.appearance.padding.clamp(4.0, 32.0);
        self.appearance.art_size = self.appearance.art_size.clamp(24, 96);
        self.appearance.font_size_title = self.appearance.font_size_title.clamp(8.0, 32.0);
        self.appearance.font_size_artist = self.appearance.font_size_artist.clamp(8.0, 28.0);
    }

    /// Logs every setting in effect for this run, one line per section in the
    /// same on-disk form a hand-edited `config.toml` uses, plus a warning for
    /// each unknown key the file contained (top level or nested under a
    /// section). Unknown keys are preserved across saves (see `unknown`) and
    /// must never pass silently. Called once at startup, right after load.
    pub fn log_settings(&self) {
        info!("config in effect for this run (persistable={})", self.persistable);
        info!("config [overlay] {}", toml_line(&self.overlay));
        info!("config [behavior] {}", toml_line(&self.behavior));
        info!("config [appearance] {}", toml_line(&self.appearance));
        warn_unknown_keys("(top level)", &self.unknown);
        warn_unknown_keys("[overlay]", &self.overlay.unknown);
        warn_unknown_keys("[behavior]", &self.behavior.unknown);
        warn_unknown_keys("[appearance]", &self.appearance.unknown);
    }

    /// Strings describing every numeric setting that `normalize()` moved off
    /// the value the user wrote, one per clamped field. Pure (no logging), so
    /// callers control the emission and tests can assert the report.
    fn normalized_changes(before: &Self, after: &Self) -> Vec<String> {
        fn diff<T: PartialEq + std::fmt::Debug>(key: &str, before: T, after: T) -> Option<String> {
            (before != after)
                .then(|| format!("config {key} was outside its allowed range; normalized {before:?} -> {after:?}"))
        }
        [
            diff(
                "overlay.duration_ms",
                before.overlay.duration_ms,
                after.overlay.duration_ms,
            ),
            diff(
                "overlay.animation_ms",
                before.overlay.animation_ms,
                after.overlay.animation_ms,
            ),
            diff("overlay.max_width", before.overlay.max_width, after.overlay.max_width),
            diff(
                "overlay.max_tick_hz",
                before.overlay.max_tick_hz,
                after.overlay.max_tick_hz,
            ),
            diff("overlay.margin", before.overlay.margin, after.overlay.margin),
            diff(
                "overlay.compact_margin",
                before.overlay.compact_margin,
                after.overlay.compact_margin,
            ),
            diff(
                "behavior.debounce_ms",
                before.behavior.debounce_ms,
                after.behavior.debounce_ms,
            ),
            diff(
                "appearance.corner_radius",
                before.appearance.corner_radius,
                after.appearance.corner_radius,
            ),
            diff(
                "appearance.compact_corner_radius",
                before.appearance.compact_corner_radius,
                after.appearance.compact_corner_radius,
            ),
            diff(
                "appearance.padding",
                before.appearance.padding,
                after.appearance.padding,
            ),
            diff(
                "appearance.art_size",
                before.appearance.art_size,
                after.appearance.art_size,
            ),
            diff(
                "appearance.font_size_title",
                before.appearance.font_size_title,
                after.appearance.font_size_title,
            ),
            diff(
                "appearance.font_size_artist",
                before.appearance.font_size_artist,
                after.appearance.font_size_artist,
            ),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// One-line rendering of a config section in its on-disk form, for the
/// startup dump. Sections serialize through their own serde impls, so the
/// dump shows exactly the keys and spellings a user's config.toml uses.
fn toml_line<T: Serialize>(value: &T) -> String {
    toml::to_string(value)
        .map(|text| text.replace('\n', " ").trim_end().to_string())
        .unwrap_or_else(|_| "<unserializable>".to_string())
}

/// Warns once per unknown key a config file section contained. The keys were
/// captured into the section's `unknown` table and are preserved on save, but
/// they must not pass silently at startup.
fn warn_unknown_keys(section: &str, unknown: &toml::Table) {
    for key in unknown.keys() {
        warn!("config {section} holds unknown field {key:?}; it is ignored and preserved on save");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn invalid_display_values_are_bounded() {
        let mut config = Config::default();
        config.overlay.max_width = u32::MAX;
        config.appearance.art_size = 0;
        config.behavior.debounce_ms = 1;
        config.normalize();
        assert_eq!(config.overlay.max_width, 800);
        assert_eq!(config.appearance.art_size, 24);
        assert_eq!(config.behavior.debounce_ms, 150);
    }

    #[test]
    fn normalized_changes_reports_only_clamped_values() {
        let mut raw = Config::default();
        raw.overlay.max_width = 1000; // clamps to 800
        raw.behavior.debounce_ms = 1; // clamps to 150
        raw.appearance.art_size = 0; // clamps to 24
        raw.overlay.duration_ms = 12_000; // in range: must not be reported
        let after = {
            let mut config = raw.clone();
            config.normalize();
            config
        };
        let changes = Config::normalized_changes(&raw, &after);
        assert_eq!(changes.len(), 3, "{changes:#?}");
        assert!(
            changes
                .iter()
                .any(|c| c.contains("overlay.max_width") && c.contains("1000") && c.contains("800")),
            "{changes:#?}"
        );
        assert!(
            changes
                .iter()
                .any(|c| c.contains("behavior.debounce_ms") && c.contains("150")),
            "{changes:#?}"
        );
        assert!(
            changes
                .iter()
                .any(|c| c.contains("appearance.art_size") && c.contains("24")),
            "{changes:#?}"
        );
        assert!(
            !changes.iter().any(|c| c.contains("duration_ms")),
            "an in-range value must not be reported: {changes:#?}"
        );
    }

    #[test]
    fn max_tick_hz_defaults_to_60_and_normalizes_to_bounds() {
        // Default is the 60 Hz cap requested; it is always present.
        let config = Config::default();
        assert_eq!(config.overlay.max_tick_hz, Some(60));

        // Below the floor clamps up to 60 (including 0, which must not disable
        // the cap). Above the ceiling clamps down to 1000.
        let mut low = Config::default();
        low.overlay.max_tick_hz = Some(0);
        low.normalize();
        assert_eq!(low.overlay.max_tick_hz, Some(60));

        let mut high = Config::default();
        high.overlay.max_tick_hz = Some(5000);
        high.normalize();
        assert_eq!(high.overlay.max_tick_hz, Some(1000));

        // A value inside the band is preserved.
        let mut mid = Config::default();
        mid.overlay.max_tick_hz = Some(144);
        mid.normalize();
        assert_eq!(mid.overlay.max_tick_hz, Some(144));
    }

    #[test]
    fn session_behaviors_default_to_silent_tray() {
        let config = Config::default();
        assert!(!config.behavior.start_on_login);
        assert!(config.behavior.start_in_tray);
        assert!(config.behavior.close_to_tray);
    }

    #[test]
    fn unknown_config_fields_survive_a_save_round_trip() {
        // Simulates a config.toml written by a newer build or by hand: unknown
        // keys at every level must survive load → save → reload. In-memory
        // strings stand in for save()/load(), which touch the real %APPDATA%
        // config path.
        let source = r#"
future_feature = true

[overlay]
duration_ms = 4000
nested_overlay = "kept"

[behavior]
start_in_tray = false
nested_behavior = 42

[appearance]
art_size = 64
nested_appearance = [1, 2, 3]
"#;
        let mut config: Config = toml::from_str(source).unwrap();
        // A real settings change must not delete the unknown fields either.
        config.overlay.duration_ms = 5000;
        let saved = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(
            reloaded.unknown.get("future_feature").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            reloaded.overlay.unknown.get("nested_overlay").and_then(|v| v.as_str()),
            Some("kept")
        );
        assert_eq!(
            reloaded
                .behavior
                .unknown
                .get("nested_behavior")
                .and_then(|v| v.as_integer()),
            Some(42)
        );
        assert!(reloaded.appearance.unknown.contains_key("nested_appearance"));
        assert_eq!(reloaded.overlay.duration_ms, 5000);
    }

    /// A uniquely-named temporary directory removed on drop, so config tests
    /// can exercise the real filesystem without touching %APPDATA%.
    struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-test-{tag}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sibling_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn invalid_config_file_is_left_untouched() {
        let guard = TempDir::new("invalid-config");
        let config_path = guard.dir.join("config.toml");
        let original = b"this is not valid toml {{{ [unclosed".to_vec();
        std::fs::write(&config_path, &original).unwrap();

        let mut config = Config::load_from_path(&config_path).unwrap();
        // Defaults apply in memory only...
        assert!(!config.persistable);
        // ...and the user's file is byte-identical, with no backup or temp
        // file created next to it.
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"]);
        // The non-persistable guard makes save_checked report disabled
        // without touching the file and without erroring.
        assert_eq!(
            config.save_checked_to(&config_path).unwrap(),
            SaveOutcome::PersistenceDisabled
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
    }

    #[test]
    fn unreadable_config_path_is_left_untouched() {
        let guard = TempDir::new("unreadable-config");
        let config_path = guard.dir.join("config.toml");
        // A directory cannot be read as a file, so every retry fails.
        std::fs::create_dir(&config_path).unwrap();

        let config = Config::load_from_path(&config_path).unwrap();
        assert!(!config.persistable);
        assert!(config_path.is_dir(), "the path must be left exactly as it was");
    }

    #[test]
    fn save_replaces_an_existing_config_file() {
        let guard = TempDir::new("save-replace");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 1000\n").unwrap();

        let mut config = Config::load_from_path(&config_path).unwrap();
        config.overlay.duration_ms = 5000;
        assert!(
            matches!(config.save_checked_to(&config_path).unwrap(), SaveOutcome::Saved(_)),
            "first save against the freshly loaded revision must succeed"
        );
        // Saving twice in a row must replace, never append or corrupt, and the
        // revision must track the newest written bytes (own consecutive saves
        // update the revision).
        config.overlay.duration_ms = 7000;
        assert!(
            matches!(config.save_checked_to(&config_path).unwrap(), SaveOutcome::Saved(_)),
            "a second save with no external edit must still succeed"
        );

        let reloaded: Config = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(reloaded.overlay.duration_ms, 7000);
        assert_eq!(
            sibling_names(&guard.dir),
            vec!["config.toml"],
            "no temp file may remain after the rename"
        );
    }

    #[test]
    fn default_monitor_mode_is_active_window() {
        let config = Config::default();
        assert_eq!(config.overlay.monitor, MonitorMode::ActiveWindow);
    }

    #[test]
    fn save_checked_conflicts_on_an_external_known_value_edit() {
        // An external writer changes a KNOWN field after we loaded: the save
        // must refuse and the external version must stay on disk.
        let guard = TempDir::new("conflict-known");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 4000\n").unwrap();
        let mut config = Config::load_from_path(&config_path).unwrap();
        config.overlay.duration_ms = 5000;
        // The external edit lands after our load.
        std::fs::write(&config_path, "overlay.duration_ms = 9000\n").unwrap();

        assert_eq!(
            config.save_checked_to(&config_path).unwrap(),
            SaveOutcome::Conflict,
            "a hand-edited file must never be silently overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "overlay.duration_ms = 9000\n"
        );
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"]);
    }

    #[test]
    fn save_checked_conflicts_on_an_external_unknown_field_edit() {
        // An external writer adds an UNKNOWN key (a newer build or a hand
        // edit). The save must refuse and keep the unknown key on disk; the
        // in-memory config already preserves it via the unknown table, and a
        // blind save would still have been a silent clobber of the edit.
        let guard = TempDir::new("conflict-unknown");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 4000\n").unwrap();
        let mut config = Config::load_from_path(&config_path).unwrap();
        config.overlay.duration_ms = 5000;
        std::fs::write(&config_path, "overlay.duration_ms = 4000\nfuture_key = 1\n").unwrap();

        assert_eq!(config.save_checked_to(&config_path).unwrap(), SaveOutcome::Conflict);
        assert!(
            std::fs::read_to_string(&config_path)
                .unwrap()
                .contains("future_key = 1")
        );
    }

    #[test]
    fn oversized_config_is_left_untouched_and_nonpersistable() {
        // A config file beyond the size bound is never parsed, never captured
        // into a revision, and never overwritten.
        let guard = TempDir::new("oversized-config");
        let config_path = guard.dir.join("config.toml");
        let blob = vec![b'x'; MAX_CONFIG_BYTES + 1];
        std::fs::write(&config_path, &blob).unwrap();

        let mut config = Config::load_from_path(&config_path).unwrap();
        assert!(!config.persistable);
        assert!(config.revision.is_none());
        assert_eq!(std::fs::read(&config_path).unwrap(), blob);
        assert_eq!(
            config.save_checked_to(&config_path).unwrap(),
            SaveOutcome::PersistenceDisabled
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), blob);
    }

    #[test]
    fn replacement_failure_leaves_the_original() {
        // The rename cannot replace a directory with a file: the write must
        // fail, clean up its temp, and leave the original directory entry
        // exactly as it was.
        let guard = TempDir::new("replace-fail");
        let config_path = guard.dir.join("config.toml");
        std::fs::create_dir(&config_path).unwrap();
        std::fs::write(guard.dir.join("other"), b"keep").unwrap();

        assert!(Config::write_temp_and_rename(&config_path, b"[overlay]\n").is_err());
        assert!(config_path.is_dir(), "the target must be left as-is");
        assert_eq!(
            sibling_names(&guard.dir),
            vec!["config.toml", "other"],
            "no temp file may remain after a failed replace"
        );
    }

    #[test]
    fn failed_write_leaves_no_temp_state() {
        // The parent of the target is a regular FILE, so the earliest
        // write-through step (creating the parent directory, then the temp
        // next to it) fails: whatever step of the write-through path fails,
        // no partial state may remain behind.
        let guard = TempDir::new("write-fail");
        let blocker = guard.dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let config_path = blocker.join("config.toml");

        assert!(Config::write_temp_and_rename(&config_path, b"[overlay]\n").is_err());
        assert_eq!(
            sibling_names(&guard.dir),
            vec!["blocker"],
            "no temp file may exist after the failed write"
        );
    }

    #[test]
    fn deleted_config_is_reported_as_conflict() {
        // The file vanishes between load and save: the verification re-read
        // fails, which must surface as a conflict (never a blind write) and
        // must not re-create the file.
        let guard = TempDir::new("deleted-config");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 4000\n").unwrap();
        let mut config = Config::load_from_path(&config_path).unwrap();
        config.overlay.duration_ms = 5000;
        std::fs::remove_file(&config_path).unwrap();

        assert_eq!(config.save_checked_to(&config_path).unwrap(), SaveOutcome::Conflict);
        assert!(!config_path.exists(), "a deleted config must not be re-created");
        assert_eq!(sibling_names(&guard.dir), Vec::<String>::new());
    }

    #[test]
    fn grown_config_at_save_conflicts_and_is_untouched() {
        // The file is replaced by one past the size bound after load: the
        // save must refuse to touch it (the metadata pre-check avoids even
        // buffering it) and report a conflict.
        let guard = TempDir::new("grown-config");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 4000\n").unwrap();
        let mut config = Config::load_from_path(&config_path).unwrap();
        config.overlay.duration_ms = 5000;
        let blob = vec![b'x'; MAX_CONFIG_BYTES + 1];
        std::fs::write(&config_path, &blob).unwrap();

        assert_eq!(config.save_checked_to(&config_path).unwrap(), SaveOutcome::Conflict);
        assert_eq!(std::fs::read(&config_path).unwrap(), blob);
    }

    #[test]
    fn config_without_a_monitor_field_deserializes_to_active_window() {
        // A config.toml written by a build before the monitor setting existed
        // has no `[overlay] monitor` key and must keep today's behavior.
        let config: Config = toml::from_str("[overlay]\nduration_ms = 4000\n").unwrap();
        assert_eq!(config.overlay.monitor, MonitorMode::ActiveWindow);
        assert_eq!(config.overlay.duration_ms, 4000);
    }

    #[test]
    fn monitor_mode_round_trips_through_toml() {
        // Round-trip direction: within a table, a mode serializes to a plain
        // string value and parses back to the same variant.
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            m: MonitorMode,
        }
        for mode in [
            MonitorMode::ActiveWindow,
            MonitorMode::Primary,
            MonitorMode::Index(0),
            MonitorMode::Index(3),
        ] {
            let text = toml::to_string_pretty(&Wrapper { m: mode }).unwrap();
            let back: Wrapper = toml::from_str(&text).unwrap();
            assert_eq!(back.m, mode, "round trip of {text:?} must preserve the mode");
        }
        // The exact on-disk forms, as a user would hand-edit them.
        for (form, expected) in [
            ("active-window", MonitorMode::ActiveWindow),
            ("primary", MonitorMode::Primary),
            ("index-0", MonitorMode::Index(0)),
            ("index-2", MonitorMode::Index(2)),
        ] {
            let config: Config = toml::from_str(&format!("[overlay]\nmonitor = \"{form}\"\n")).unwrap();
            assert_eq!(
                config.overlay.monitor, expected,
                "monitor = \"{form}\" in [overlay] must map to {expected:?}"
            );
        }
    }

    #[test]
    fn invalid_monitor_mode_is_rejected_not_reinterpreted() {
        // Unknown strings and malformed indices are hard deserialization
        // errors (the load path then applies defaults in memory and leaves
        // the user's file untouched), never a silent reinterpretation.
        for bad in [
            "\"bogus\"",
            "\"Index(1)\"",
            "\"index\"",
            "\"index-\"",
            "\"index--1\"",
            "\"index-abc\"",
        ] {
            assert!(
                toml::from_str::<MonitorMode>(bad).is_err(),
                "{bad} must not parse as a monitor mode"
            );
        }
    }

    #[test]
    fn monitor_setting_survives_a_config_save_round_trip() {
        let mut config = Config::default();
        config.overlay.monitor = MonitorMode::Index(2);
        let saved = toml::to_string_pretty(&config).unwrap();
        assert!(
            saved.contains("monitor = \"index-2\""),
            "the mode must serialize in its hand-editable string form:\n{saved}"
        );
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(reloaded.overlay.monitor, MonitorMode::Index(2));
    }

    #[test]
    fn layout_defaults_to_expanded_with_separation_off() {
        let config = Config::default();
        assert_eq!(config.overlay.layout, LayoutMode::Expanded);
        assert!(!config.overlay.compact_position_separate);
        assert_eq!(
            config.overlay.compact_effective(),
            CompactPosition {
                vertical: VerticalPosition::Top,
                horizontal: HorizontalPosition::Center,
                margin: 8,
                x: None,
                y: None,
                monitor: MonitorMode::ActiveWindow,
            }
        );
    }

    #[test]
    fn layout_round_trips_through_toml() {
        for (form, expected) in [
            ("expanded", LayoutMode::Expanded),
            ("compact", LayoutMode::Compact),
            ("auto", LayoutMode::Auto),
            ("persistent-compact", LayoutMode::PersistentCompact),
        ] {
            let config: Config = toml::from_str(&format!("[overlay]\nlayout = \"{form}\"\n")).unwrap();
            assert_eq!(
                config.overlay.layout, expected,
                "layout = \"{form}\" in [overlay] must map to {expected:?}"
            );
            let saved = toml::to_string_pretty(&config).unwrap();
            assert!(
                saved.contains(&format!("layout = \"{form}\"")),
                "the mode must serialize in its hand-editable string form:\n{saved}"
            );
        }
        // Unknown modes are hard deserialization errors, never a silent
        // reinterpretation.
        assert!(toml::from_str::<Config>("[overlay]\nlayout = \"bogus\"\n").is_err());
    }

    #[test]
    fn hover_toggles_default_on_and_round_trip_through_toml() {
        // Both default to true. This is a deliberate model change from the
        // old single compact_hover_action: compact pills now expand on the
        // first hover and dismiss on the second, and hovering an expanded
        // pill arms a 500 ms dismiss — the previous model deferred an
        // expanded pill's countdown while the cursor stayed on it, and had
        // no equivalent of dismiss-on-hover at all.
        let defaults = Config::default();
        assert!(defaults.overlay.dismiss_on_hover);
        assert!(defaults.overlay.expand_compact_on_hover);
        for (key, value) in [("dismiss_on_hover", "false"), ("expand_compact_on_hover", "false")] {
            let config: Config = toml::from_str(&format!("[overlay]\n{key} = {value}\n")).unwrap();
            let loaded = if key == "dismiss_on_hover" {
                config.overlay.dismiss_on_hover
            } else {
                config.overlay.expand_compact_on_hover
            };
            assert!(!loaded, "{key} = {value} in [overlay] must load as false");
            let saved = toml::to_string_pretty(&config).unwrap();
            assert!(
                saved.contains(&format!("{key} = {value}")),
                "the toggle must serialize back:\n{saved}"
            );
        }
    }

    #[test]
    fn compact_effective_mirrors_expanded_while_separation_is_off() {
        let mut config = Config::default();
        config.overlay.vertical = VerticalPosition::Bottom;
        config.overlay.horizontal = HorizontalPosition::Right;
        config.overlay.margin = 24;
        config.overlay.position_x = Some(120);
        config.overlay.position_y = Some(40);
        config.overlay.monitor = MonitorMode::Index(1);
        // A stale/customized independent Compact position must not leak
        // through while separation is off.
        config.overlay.compact_vertical = VerticalPosition::Top;
        config.overlay.compact_horizontal = HorizontalPosition::Left;
        assert_eq!(
            config.overlay.compact_effective(),
            CompactPosition {
                vertical: VerticalPosition::Bottom,
                horizontal: HorizontalPosition::Right,
                margin: 24,
                x: Some(120),
                y: Some(40),
                monitor: MonitorMode::Index(1),
            }
        );
    }

    #[test]
    fn compact_effective_uses_independent_fields_while_separation_is_on() {
        let mut config = Config::default();
        config.overlay.vertical = VerticalPosition::Bottom;
        config.overlay.compact_vertical = VerticalPosition::Top;
        config.overlay.compact_horizontal = HorizontalPosition::Left;
        config.overlay.compact_position_x = Some(80);
        config.overlay.compact_position_separate = true;
        assert_eq!(config.overlay.compact_effective().vertical, VerticalPosition::Top);
        assert_eq!(config.overlay.compact_effective().x, Some(80));
        // The Expanded position itself is untouched by the separation flag.
        assert_eq!(config.overlay.vertical, VerticalPosition::Bottom);
    }

    #[test]
    fn compact_is_default_tracks_customization() {
        let mut config = Config::default();
        assert!(config.overlay.compact_is_default());
        config.overlay.compact_horizontal = HorizontalPosition::Right;
        assert!(!config.overlay.compact_is_default());
    }

    #[test]
    fn auto_compact_sources_survive_a_save_round_trip() {
        let mut config = Config::default();
        config.behavior.auto_compact_sources = vec!["youtube-music".into(), "netflix".into()];
        let saved = toml::to_string_pretty(&config).unwrap();
        assert!(saved.contains("auto_compact_sources"), "{saved}");
        let reloaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(
            reloaded.behavior.auto_compact_sources,
            vec!["youtube-music".to_string(), "netflix".to_string()]
        );
    }

    #[test]
    fn switching_layout_keeps_auto_compact_sources() {
        // Auto-compact sources are persisted regardless of the selected
        // layout and must never be cleared by a layout switch.
        let mut config = Config::default();
        config.behavior.auto_compact_sources = vec!["spotify".into()];
        for mode in [
            LayoutMode::Expanded,
            LayoutMode::Compact,
            LayoutMode::Auto,
            LayoutMode::PersistentCompact,
        ] {
            config.overlay.layout = mode;
            let saved = toml::to_string_pretty(&config).unwrap();
            let reloaded: Config = toml::from_str(&saved).unwrap();
            assert_eq!(reloaded.behavior.auto_compact_sources, vec!["spotify".to_string()]);
        }
    }

    #[test]
    fn compact_margin_is_bounded() {
        let mut config = Config::default();
        config.overlay.compact_margin = 10_000;
        config.normalize();
        assert_eq!(config.overlay.compact_margin, 500);
    }

    #[test]
    fn compact_corner_radius_defaults_without_touching_the_expanded_radius() {
        let config = Config::default();
        assert_eq!(config.appearance.corner_radius, 26.0);
        assert_eq!(config.appearance.compact_corner_radius, 12.0);
    }

    #[test]
    fn missing_compact_corner_radius_loads_with_the_default() {
        // A config written before this key existed must load successfully and
        // use the new default, with the existing radius untouched.
        let config: Config = toml::from_str("[appearance]\ncorner_radius = 26.0\n").unwrap();
        assert_eq!(config.appearance.corner_radius, 26.0);
        assert_eq!(config.appearance.compact_corner_radius, 12.0);
    }

    #[test]
    fn compact_corner_radius_round_trips_and_stays_independent() {
        // Expanded and Compact radii are independent knobs: any combination is
        // valid, including equal values for both layouts.
        for (corner, compact) in [(26.0, 12.0), (20.0, 8.0), (26.0, 26.0)] {
            let config: Config = toml::from_str(&format!(
                "[appearance]\ncorner_radius = {corner}\ncompact_corner_radius = {compact}\n"
            ))
            .unwrap();
            assert_eq!(config.appearance.corner_radius, corner);
            assert_eq!(config.appearance.compact_corner_radius, compact);
            let saved = toml::to_string_pretty(&config).unwrap();
            assert!(saved.contains("compact_corner_radius"), "{saved}");
            let reloaded: Config = toml::from_str(&saved).unwrap();
            assert_eq!(reloaded.appearance.corner_radius, corner);
            assert_eq!(reloaded.appearance.compact_corner_radius, compact);
        }
    }

    #[test]
    fn effective_corner_radius_follows_the_effective_layout() {
        let mut config = Config::default();
        config.appearance.corner_radius = 20.0;
        config.appearance.compact_corner_radius = 8.0;
        // `compact` is the already-resolved effective layout, so Auto that
        // resolved to Expanded/Compact selects the matching radius without
        // any Auto-specific logic here.
        assert_eq!(config.appearance.effective_corner_radius(false), 20.0);
        assert_eq!(config.appearance.effective_corner_radius(true), 8.0);
    }

    #[test]
    fn compact_corner_radius_is_bounded() {
        let mut config = Config::default();
        config.appearance.compact_corner_radius = 1000.0;
        config.normalize();
        assert_eq!(config.appearance.compact_corner_radius, 48.0);
        config.appearance.compact_corner_radius = -5.0;
        config.normalize();
        assert_eq!(config.appearance.compact_corner_radius, 4.0);
    }

    #[test]
    fn docs_and_config_example_cover_every_config_field() {
        // Every serializable field must be documented in both
        // docs/configuration.md and config.example.toml. The key set is
        // derived from a fully-populated default config, so no struct field
        // can be added without this test failing until both files mention
        // it — the schema drift the review found cannot recur.
        let mut config = Config::default();
        // Populate the Option fields that skip_serializing_if would omit.
        config.overlay.position_x = Some(0);
        config.overlay.position_y = Some(0);
        config.overlay.compact_position_x = Some(0);
        config.overlay.compact_position_y = Some(0);
        let serialized = toml::to_string_pretty(&config).unwrap();
        let keys: Vec<&str> = serialized
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
            .collect();
        assert!(
            keys.len() >= 30,
            "the fully-populated default must expose the whole field set, got {keys:?}"
        );
        let example = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml"))
            .expect("config.example.toml must exist at the crate root");
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/configuration.md"))
            .expect("docs/configuration.md must exist at the crate root");

        let missing_in_example: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| !example.contains(&format!("{key} =")))
            .collect();
        let missing_in_docs: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| !doc.contains(&format!("`{key}`")))
            .collect();

        assert!(
            missing_in_example.is_empty(),
            "config.example.toml does not cover: {missing_in_example:?}"
        );
        assert!(
            missing_in_docs.is_empty(),
            "docs/configuration.md does not cover: {missing_in_docs:?}"
        );
    }
}
