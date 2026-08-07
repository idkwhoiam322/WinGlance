use log::warn;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerticalPosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
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
    pub allowed_sources: Vec<String>,
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
            allowed_sources: Vec::new(),
            unknown: toml::Table::new(),
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            background_color: [0x12, 0x14, 0x1C, 0xFF],
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

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let config_path = Self::config_path()?;
        if !config_path.exists() {
            let mut config = Config::default();
            config.normalize();
            config.save()?;
            return Ok(config);
        }
        let content = match std::fs::read_to_string(&config_path) {
            Ok(content) => content,
            Err(error) => {
                // An unreadable config (bad encoding such as UTF-16, transient
                // I/O error) must never be replaced with defaults: that would
                // destroy the user's file. Quarantine it like the
                // invalid-TOML path below; only when even the rename fails do
                // we run on defaults for this launch without touching the file.
                warn!("config.toml could not be read ({error}); quarantining it and falling back to defaults");
                let backup = config_path.with_extension("toml.bad");
                return match std::fs::rename(&config_path, &backup) {
                    Ok(()) => Self::default_config_saved(),
                    Err(rename_error) => {
                        warn!(
                            "keeping the unreadable config as {backup:?} failed: {rename_error}; defaults apply for this run only"
                        );
                        let mut config = Config::default();
                        config.normalize();
                        Ok(config)
                    }
                };
            }
        };
        match toml::from_str::<Config>(&content) {
            Ok(mut config) => {
                config.normalize();
                Ok(config)
            }
            Err(error) => {
                // A hand-edited or partially written config must not kill the
                // app with no console and no dialog: fall back to defaults and
                // keep the bad file for inspection under config.toml.bad.
                warn!("config.toml is not valid TOML ({error}); falling back to defaults");
                let backup = config_path.with_extension("toml.bad");
                if let Err(rename_error) = std::fs::rename(&config_path, &backup) {
                    warn!("keeping the bad config as {backup:?} failed: {rename_error}");
                }
                Self::default_config_saved()
            }
        }
    }

    /// Defaults in memory with a best-effort persist, so the next launch
    /// starts from a healthy file while the bad one stays quarantined.
    fn default_config_saved() -> anyhow::Result<Self> {
        let mut config = Config::default();
        config.normalize();
        if let Err(error) = config.save() {
            warn!("could not write a fresh config.toml: {error}");
        }
        Ok(config)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let config_path = Self::config_path()?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        // Write to a temp file and rename so a crash mid-write cannot leave a
        // truncated config behind (the rename is atomic on the same volume).
        let tmp_path = config_path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, content)?;
        if let Err(e) = std::fs::rename(&tmp_path, &config_path) {
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
}
