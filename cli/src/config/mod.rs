use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub worker_base_url: String,
    pub email: Option<String>,
    pub default_mailbox_id: Option<String>,
    /// Bearer token for the worker's CLI auth. Stored on disk in plaintext —
    /// the file is chmod 0600 on Unix. If that's not strong enough for your
    /// threat model, don't use this CLI on a shared machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let home = std::env::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
        Ok(home.join(".postr").join("config"))
    }

    pub fn load() -> Result<Option<Self>> {
        let p = Self::path()?;
        if !p.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let cfg: Self =
            serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))?;
        if cfg.worker_base_url.is_empty() {
            return Ok(None);
        }
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        let dir = p
            .parent()
            .ok_or_else(|| anyhow!("config path has no parent"))?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict_dir_perms(dir);
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, s).with_context(|| format!("writing {}", p.display()))?;
        restrict_file_perms(&p);
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

// ── Token helpers ────────────────────────────────────────────────────────
//
// Thin wrappers that load → mutate `token` → save. Each call rewrites the
// whole file, which is fine — it's a few hundred bytes.

pub fn save_token(token: &str) -> Result<()> {
    let mut cfg = Config::load()?.unwrap_or_default();
    cfg.token = Some(token.to_string());
    cfg.save()
}

pub fn load_token() -> Result<Option<String>> {
    Ok(Config::load()?.and_then(|c| c.token))
}

pub fn delete_token() -> Result<()> {
    let Some(mut cfg) = Config::load()? else {
        return Ok(());
    };
    cfg.token = None;
    cfg.save()
}

// ── Permissions ──────────────────────────────────────────────────────────

#[cfg(unix)]
fn restrict_file_perms(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

#[cfg(unix)]
fn restrict_dir_perms(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_file_perms(_p: &std::path::Path) {}

#[cfg(not(unix))]
fn restrict_dir_perms(_p: &std::path::Path) {}
