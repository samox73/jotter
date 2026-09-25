//! `~/.config/jotter/config.toml`, read once at startup into a global.
//! Every field is optional; missing file means defaults.

use serde::Deserialize;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Images taller than this many terminal rows are downscaled once.
    pub max_image_rows: u16,
    /// Output rows shown per cell before it becomes a scrollable viewport.
    pub max_output_rows: usize,
    /// Seconds between autosave-sidecar checks.
    pub autosave_secs: u64,
    /// Syntect theme: base16-ocean.dark (default), base16-ocean.light,
    /// base16-eighties.dark, base16-mocha.dark, InspiredGitHub,
    /// Solarized (dark), Solarized (light).
    pub theme: String,
    /// Cell editor backend: "builtin" (default), or "nvim" — an embedded
    /// `nvim --embed` owns text/mode/registers (experimental; full modal
    /// editing: dw, ciw, visual mode, counts, macros, `.`). Falls back to
    /// builtin when nvim is missing or dies.
    pub editor: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_image_rows: 18,
            max_output_rows: 15,
            autosave_secs: 30,
            theme: "base16-ocean.dark".into(),
            editor: "builtin".into(),
        }
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("jotter/config.toml"))
}

/// Load the config file (once, before `get`). Returns a user-facing warning
/// if the file exists but is invalid; defaults apply either way.
pub fn init() -> Option<String> {
    let path = path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str(&text) {
        Ok(c) => {
            let _ = CONFIG.set(c);
            None
        }
        Err(e) => Some(format!("config ignored ({}): {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_parses_and_unknown_keys_error() {
        let c: Config = toml::from_str("max_output_rows = 30").unwrap();
        assert_eq!(c.max_output_rows, 30);
        assert_eq!(c.max_image_rows, 18); // default kept
        assert!(toml::from_str::<Config>("max_outpt_rows = 30").is_err()); // typo caught
    }
}
