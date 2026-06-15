use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod keyring;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub worker_base_url: String,
    pub email: Option<String>,
    pub default_mailbox_id: Option<String>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let proj = ProjectDirs::from("dev", "postr", "postr").context("no home directory")?;
        let dir = proj.config_dir();
        std::fs::create_dir_all(dir).ok();
        Ok(dir.join("config.toml"))
    }

    pub fn load() -> Result<Option<Self>> {
        let p = Self::path()?;
        if !p.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(&p)?;
        let cfg: Self = toml::from_str(&s)?;
        if cfg.worker_base_url.is_empty() {
            return Ok(None);
        }
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        let s = toml::to_string_pretty(self)?;
        std::fs::write(&p, s)?;
        Ok(())
    }

    pub fn clear() -> Result<()> {
        let p = Self::path()?;
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }
}
