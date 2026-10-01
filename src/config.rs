//! `~/.config/jotter/config.toml`, read once at startup into a global.
//! Every field is optional; missing file means defaults.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Deserialize, Serialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    // These doc comments are the user documentation: `jotter --generate`
    // turns them into the docs site's configuration reference (markdown;
    // an empty `///` line starts a new paragraph).
    /// Images taller than this many terminal rows are scaled down to fit, once, when the output is first shown. Larger values show plots bigger but push the next cell further down.
    pub max_image_rows: u16,
    /// Display math taller than this many terminal rows is scaled down to fit: `$$…$$` in markdown, latex cells, and LaTeX outputs such as SymPy's. Inline `$…$` math always takes one row. See [math](/jotter/guides/math/).
    pub max_math_rows: u16,
    /// Outputs taller than this many rows are shown in a scrollable viewport instead of in full. Scroll it with `[` and `]` or the mouse wheel; see [long outputs](/jotter/guides/outputs/#long-outputs).
    pub max_output_rows: usize,
    /// How often, in seconds, jOtter writes an autosave when there are unsaved changes. Autosaves go to `~/.local/state/jotter/autosave/`, never next to the notebook; see [data safety](/jotter/guides/data-safety/#autosave-and-recovery). Values below 1 count as 1.
    pub autosave_secs: u64,
    /// The syntax-highlighting theme for code cells and fenced code in markdown. One of `base16-ocean.dark`, `base16-ocean.light`, `base16-eighties.dark`, `base16-mocha.dark`, `InspiredGitHub`, `Solarized (dark)` or `Solarized (light)`.
    ///
    /// On a light terminal background, use `base16-ocean.light`, `InspiredGitHub` or `Solarized (light)`. See them all in the [theme gallery](/jotter/reference/themes/).
    pub theme: String,
    /// The cell editor: `"builtin"`, jOtter's own vim-style editor, or `"nvim"`, an embedded Neovim with your config and plugins that gives you the full Neovim editing model. The Neovim backend is experimental; see [embedded Neovim](/jotter/guides/neovim/). If Neovim is missing or crashes, jOtter falls back to the builtin editor.
    pub editor: String,
    /// With `editor = "nvim"`: load your Neovim config and plugins. jOtter sets `g:jotter = 1` before your config runs, so you can skip plugins that make no sense inside a cell. Set to `false` for a clean Neovim (`-u NONE`).
    pub nvim_user_config: bool,
    /// Python kernels: use IPython's Jedi completer for every completion. Jedi completes names from code that hasn't run yet, but is slow. When `false`, jOtter uses IPython's fast completer and asks Jedi only when the fast one finds nothing; see [completion](/jotter/guides/completion/#where-completions-come-from-python).
    pub jedi: bool,
    /// Open completion automatically when you type `.` after a name. When `false`, completion opens only with `Tab`.
    pub complete_on_dot: bool,
    /// Show plots in your terminal's colours: a figure's white background becomes the terminal background and black text and axes become the terminal foreground, while coloured lines keep their hue. Only the display changes; the notebook, its saved outputs and files written with `savefig` keep their original colours. It applies to images with a large white background and no transparency, so photos and figures you styled yourself are left alone. See [outputs](/jotter/guides/outputs/#plots-in-your-terminals-colours).
    pub recolor_plots: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_image_rows: 18,
            max_math_rows: 4,
            max_output_rows: 15,
            autosave_secs: 30,
            theme: "base16-ocean.dark".into(),
            editor: "builtin".into(),
            nvim_user_config: true,
            jedi: false,
            complete_on_dot: true,
            recolor_plots: true,
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

/// `$XDG_STATE_HOME/jotter` (default `~/.local/state/jotter`): autosaves.
pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("jotter"))
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
