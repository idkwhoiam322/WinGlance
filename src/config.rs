use log::warn;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            overlay: OverlayConfig::default(),
            behavior: BehaviorConfig::default(),
            appearance: AppearanceConfig::default(),
            unknown: toml::Table::new(),
            persistable: true,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_y: Option<i32>,
    /// Unknown keys under `[overlay]`, preserved across saves.
    #[serde(flatten)]
    pub unknown: toml::Table,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerticalPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HorizontalPosition {
    #[default]
    Center,
    Left,
    Right,
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
            duration_ms: 3000,
            animation_ms: 280,
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Center,
            margin: 8,
            max_width: 340,
            position_x: None,
            position_y: None,
            unknown: toml::Table::new(),
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
            padding: 15.0,
            art_size: 48,
            font_size_title: 16.0,
            font_size_artist: 13.0,
            unknown: toml::Table::new(),
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

    /// Loads the config from `config_path`. When the file does not exist, a
    /// fresh default is written there. When it exists but cannot be read
    /// (after retries) or parsed, the file is left completely untouched and
    /// defaults apply in memory for this run with persistence disabled — an
    /// existing user config must never be moved, overwritten, or replaced
    /// with defaults, not even under a backup name.
    fn load_from_path(config_path: &Path) -> anyhow::Result<Self> {
        if !config_path.exists() {
            let mut config = Config::default();
            config.normalize();
            config.save_to(config_path)?;
            return Ok(config);
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
        match toml::from_str::<Config>(&content) {
            Ok(mut config) => {
                config.normalize();
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

    pub fn save(&self) -> anyhow::Result<()> {
        if !self.persistable {
            warn!(
                "config.toml is not persistable this run (it was invalid or unreadable and was left untouched); settings apply until the app exits"
            );
            return Ok(());
        }
        self.save_to(&Self::config_path()?)
    }

    /// Writes `config.toml` via a co-located temp file + same-volume rename,
    /// so a crash mid-write cannot leave a truncated config behind (the
    /// rename atomically replaces an existing file).
    fn save_to(&self, config_path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        let tmp_path = config_path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, content)?;
        if let Err(e) = std::fs::rename(&tmp_path, config_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    }

    fn config_path() -> anyhow::Result<PathBuf> {
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
        self.overlay.animation_ms = self.overlay.animation_ms.clamp(100, 500);
        self.overlay.max_width = self.overlay.max_width.clamp(180, 800);
        self.overlay.margin = self.overlay.margin.clamp(0, 500);
        self.behavior.debounce_ms = self.behavior.debounce_ms.clamp(150, 250);
        self.appearance.corner_radius = self.appearance.corner_radius.clamp(4.0, 48.0);
        self.appearance.padding = self.appearance.padding.clamp(4.0, 32.0);
        self.appearance.art_size = self.appearance.art_size.clamp(24, 96);
        self.appearance.font_size_title = self.appearance.font_size_title.clamp(8.0, 32.0);
        self.appearance.font_size_artist = self.appearance.font_size_artist.clamp(8.0, 28.0);
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

        let config = Config::load_from_path(&config_path).unwrap();
        // Defaults apply in memory only...
        assert!(!config.persistable);
        // ...and the user's file is byte-identical, with no backup or temp
        // file created next to it.
        assert_eq!(std::fs::read(&config_path).unwrap(), original);
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"]);
        // The non-persistable guard makes save() a no-op that must not error.
        assert!(config.save().is_ok());
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

        let mut config = Config::default();
        config.overlay.duration_ms = 5000;
        config.save_to(&config_path).unwrap();
        // Saving twice in a row must replace, never append or corrupt.
        config.overlay.duration_ms = 7000;
        config.save_to(&config_path).unwrap();

        let reloaded: Config = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(reloaded.overlay.duration_ms, 7000);
        assert_eq!(
            sibling_names(&guard.dir),
            vec!["config.toml"],
            "no temp file may remain after the rename"
        );
    }
}
