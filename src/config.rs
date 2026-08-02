use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub overlay: OverlayConfig,
    pub behavior: BehaviorConfig,
    pub appearance: AppearanceConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub duration_ms: u64,
    pub animation_ms: u64,
    pub position: OverlayPosition,
    pub max_width: u32,
    pub margin_top: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(clippy::enum_variant_names)]
pub enum OverlayPosition {
    #[default]
    TopCenter,
    TopRight,
    TopLeft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub enable_track_change: bool,
    pub enable_playback_state_change: bool,
    pub debounce_ms: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub keep_files: u32,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            duration_ms: 3000,
            animation_ms: 200,
            position: OverlayPosition::TopCenter,
            max_width: 240,
            margin_top: 8,
        }
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            enable_track_change: true,
            enable_playback_state_change: true,
            debounce_ms: 200,
        }
    }
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            background_color: [0x00, 0x00, 0x00, 0xE6],
            text_color: [0xFF, 0xFF, 0xFF, 0xFF],
            accent_color: [0x00, 0xD4, 0xAA, 0xFF],
            corner_radius: 16.0,
            padding: 8.0,
            art_size: 32,
            font_size_title: 12.0,
            font_size_artist: 10.0,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { keep_files: 5 }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let config_path = Self::config_path()?;
        let mut config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            toml::from_str(&content)?
        } else {
            Config::default()
        };
        config.normalize();
        if !config_path.exists() {
            config.save()?;
        }
        Ok(config)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let config_path = Self::config_path()?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }

    fn config_path() -> anyhow::Result<PathBuf> {
        Ok(Self::data_dir()?.join("config.toml"))
    }

    pub fn data_dir() -> anyhow::Result<PathBuf> {
        let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("Could not find the Windows app-data directory"))?;
        Ok(base.join("notch").join("notch").join("data"))
    }

    pub fn logs_dir(&self) -> PathBuf {
        Self::data_dir().unwrap_or_else(|_| PathBuf::from("data")).join("logs")
    }

    fn normalize(&mut self) {
        self.overlay.duration_ms = self.overlay.duration_ms.clamp(500, 60_000);
        self.overlay.animation_ms = self.overlay.animation_ms.clamp(100, 500);
        self.overlay.max_width = self.overlay.max_width.clamp(180, 800);
        self.overlay.margin_top = self.overlay.margin_top.clamp(0, 500);
        self.behavior.debounce_ms = self.behavior.debounce_ms.clamp(150, 250);
        self.appearance.corner_radius = self.appearance.corner_radius.clamp(4.0, 48.0);
        self.appearance.padding = self.appearance.padding.clamp(4.0, 32.0);
        self.appearance.art_size = self.appearance.art_size.clamp(24, 96);
        self.appearance.font_size_title = self.appearance.font_size_title.clamp(8.0, 32.0);
        self.appearance.font_size_artist = self.appearance.font_size_artist.clamp(8.0, 28.0);
        self.logging.keep_files = self.logging.keep_files.clamp(1, 100);
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
}
