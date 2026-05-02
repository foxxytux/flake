use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub codex: CodexConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default = "UiConfig::default_sidebar_width")]
    pub sidebar_width: u16,
    #[serde(default = "UiConfig::default_ai_width")]
    pub ai_width: u16,
    #[serde(default = "UiConfig::default_show_hidden")]
    pub show_hidden: bool,
    #[serde(default = "UiConfig::default_model_hint")]
    pub model_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexConfig {
    #[serde(default = "CodexConfig::default_model")]
    pub model: String,
    #[serde(default = "CodexConfig::default_base_url")]
    pub base_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            codex: CodexConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub last_dir: Option<PathBuf>,
    #[serde(default)]
    pub last_file: Option<PathBuf>,
    #[serde(default)]
    pub open_files: Vec<PathBuf>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub secondary_tab: Option<usize>,
    #[serde(default)]
    pub split_enabled: bool,
    #[serde(default)]
    pub explorer_selected: usize,
    #[serde(default)]
    pub explorer_scroll: usize,
    #[serde(default)]
    pub codex_model: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: Self::default_sidebar_width(),
            ai_width: Self::default_ai_width(),
            show_hidden: Self::default_show_hidden(),
            model_hint: Self::default_model_hint(),
        }
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            model: Self::default_model(),
            base_url: Self::default_base_url(),
        }
    }
}

impl UiConfig {
    fn default_sidebar_width() -> u16 {
        28
    }

    fn default_ai_width() -> u16 {
        36
    }

    fn default_show_hidden() -> bool {
        false
    }

    fn default_model_hint() -> String {
        "gpt-5.4-mini".to_string()
    }
}

impl CodexConfig {
    fn default_model() -> String {
        "gpt-5.4-mini".to_string()
    }

    fn default_base_url() -> String {
        "https://chatgpt.com/backend-api".to_string()
    }
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file at {}", path.display()))?;
    let config: Config = toml::from_str(&contents).with_context(|| "failed to parse config")?;
    Ok(config)
}

pub fn config_path() -> PathBuf {
    if let Ok(base) = env::var("XDG_CONFIG_HOME") {
        return Path::new(&base).join("flake").join("config.toml");
    }
    if let Some(home) = env::var_os("HOME") {
        return Path::new(&home)
            .join(".config")
            .join("flake")
            .join("config.toml");
    }
    PathBuf::from("flake.toml")
}

pub fn state_dir() -> PathBuf {
    if let Ok(base) = env::var("XDG_STATE_HOME") {
        return Path::new(&base).join("flake");
    }
    if let Some(home) = env::var_os("HOME") {
        return Path::new(&home).join(".local").join("state").join("flake");
    }
    PathBuf::from(".flake-state")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.toml")
}

pub fn auth_path() -> PathBuf {
    state_dir().join("auth.json")
}

pub fn load_state() -> Result<AppState> {
    let path = state_path();
    if !path.exists() {
        return Ok(AppState::default());
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read state file at {}", path.display()))?;
    let state: AppState = toml::from_str(&contents).with_context(|| "failed to parse state")?;
    Ok(state)
}

pub fn save_state(state: &AppState) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create state dir {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(state).with_context(|| "failed to serialize state")?;
    fs::write(&path, contents)
        .with_context(|| format!("failed to write state file at {}", path.display()))?;
    Ok(())
}
