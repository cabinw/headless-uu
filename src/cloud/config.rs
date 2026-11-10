//! Runtime configuration. Nothing machine- or account-specific is compiled in.
//! File: $UU_CONFIG, else $XDG_CONFIG_HOME/uuremote/config.json, else ~/.config/uuremote/config.json.
//! Environment variables override file values. Keep the file private (chmod 600).

use std::path::PathBuf;

/// Tests that point UU_CONFIG at a temp file must take this, or they race.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("UU_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("uuremote/config.json")
}

/// Re-reads the config file (used after `login` rewrites it while the server runs).
pub fn read_fresh() -> Result<Option<serde_json::Value>, String> {
    let p = path();
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("invalid config {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
    }
}
