use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const APP_NAME: &str = "sync-pl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub spotify: Option<SpotifyConfig>,
    pub deezer: Option<DeezerConfig>,
    #[serde(default)]
    pub spotify_web: Option<SpotifyWebConfig>,
    #[serde(default)]
    pub youtube: Option<YoutubeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyWebConfig {
    /// Browser session cookie (sp_dc) — enables the web-player search
    /// fallback when the official API is rate-limited.
    pub sp_dc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
}

pub fn default_redirect_uri() -> String {
    "syncpl://callback".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoutubeConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeezerConfig {
    pub arl_token: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config {
                spotify: None,
                deezer: None,
                spotify_web: None,
                youtube: None,
            });
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {}", path.display()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| "Failed to parse config file")?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("Could not determine config directory")?;
    Ok(config_dir.join(APP_NAME).join("config.toml"))
}
