//! Default locations: the database under the user data dir, the config under the config dir.

use std::path::PathBuf;

use crate::Failure;

/// `$XDG_DATA_HOME`, else `~/Library/Application Support` on macOS, else `~/.local/share`.
pub(crate) fn data_dir() -> Result<PathBuf, Failure> {
    if let Some(dir) = env_dir("XDG_DATA_HOME") {
        return Ok(dir);
    }
    let home = home()?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        home.join(".local").join("share")
    })
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub(crate) fn config_dir() -> Result<PathBuf, Failure> {
    match env_dir("XDG_CONFIG_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join(".config")),
    }
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> Result<PathBuf, Failure> {
    env_dir("HOME").ok_or_else(|| Failure::runtime("HOME is not set; pass --db or --config"))
}
