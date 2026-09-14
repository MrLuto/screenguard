use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server_url: Option<String>,
    pub webui_url: Option<String>,
    pub cloud_account: Option<String>,
    pub cloud_url: String,
    pub heartbeat_interval: u64,
    pub user_scan_interval: u64,
    pub cache_ttl_hours: u64,
    pub idle_seconds: u64,
    pub web_filter: bool,
    pub allow_remote_update: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: None,
            webui_url: None,
            cloud_account: None,
            cloud_url: "wss://api.screenguard.cc/ws".into(),
            heartbeat_interval: 10,
            user_scan_interval: 300,
            cache_ttl_hours: 48,
            idle_seconds: 300,
            web_filter: true,
            allow_remote_update: false,
        }
    }
}
// Get the machine-wide directory from the system rather than a helper/user input.
pub fn data_dir() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("ProgramData").context("ProgramData missing")?)
            .join("ScreenGuard"),
    )
}
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("agent.db"))
}
pub fn load() -> Result<Config> {
    let path = data_dir()?.join("agent.toml");
    let cfg: Config = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        Config::default()
    };
    anyhow::ensure!(
        (1..=60).contains(&cfg.heartbeat_interval),
        "heartbeat_interval must be 1..60 seconds"
    );
    anyhow::ensure!(
        cfg.user_scan_interval >= 5 && cfg.idle_seconds > 0,
        "Invalid scan/idle interval"
    );
    if let Some(account) = &cfg.cloud_account {
        anyhow::ensure!(account.contains('@'), "cloud_account must be an email");
    }
    Ok(cfg)
}
