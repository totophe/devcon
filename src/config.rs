//! Per-project + global `devcon` configuration.
//!
//! Project config lives at `.devcontainer/devcon.json`; global fallback at
//! `~/.config/devcon/config.json`. Precedence: project over global.
//!
//! Unlike `dcon`, this config is *writable*: when the shell can't be
//! auto-detected unambiguously, `devcon` prompts once and persists the answer
//! into the project file so subsequent launches are silent.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse: Option<bool>,
}

impl Config {
    /// Load and merge configs: project-level over global.
    pub fn load(project_root: &Path) -> Self {
        let global = load_file(global_path().as_deref());
        let project = load_file(Some(&project_config_path(project_root)));

        Self {
            shell: project.shell.or(global.shell),
            mouse: project.mouse.or(global.mouse),
        }
    }

    /// Persist the resolved shell into the project's `devcon.json`, merging with
    /// whatever is already there so we never clobber other keys.
    pub fn persist_shell(project_root: &Path, shell: &str) -> std::io::Result<()> {
        let path = project_config_path(project_root);
        let mut current = load_file(Some(&path));
        current.shell = Some(shell.to_string());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&current).map_err(std::io::Error::other)?;
        std::fs::write(&path, format!("{json}\n"))
    }
}

/// `<project_root>/.devcontainer/devcon.json`
pub fn project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".devcontainer").join("devcon.json")
}

fn global_path() -> Option<PathBuf> {
    dirs_next::config_dir().map(|d| d.join("devcon").join("config.json"))
}

fn load_file(path: Option<&Path>) -> Config {
    let path = match path {
        Some(p) => p,
        None => return Config::default(),
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Config::default();
    };
    serde_json::from_str(&contents).unwrap_or_else(|e| {
        eprintln!("warning: could not parse {}: {e}", path.display());
        Config::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_devcon_json(dir: &Path, contents: &str) {
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcon.json"), contents).unwrap();
    }

    #[test]
    fn reads_project_shell() {
        let tmp = TempDir::new().unwrap();
        write_devcon_json(tmp.path(), r#"{"shell": "/bin/zsh"}"#);
        let cfg = load_file(Some(&project_config_path(tmp.path())));
        assert_eq!(cfg.shell.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = load_file(Some(&project_config_path(tmp.path())));
        assert!(cfg.shell.is_none());
    }

    #[test]
    fn invalid_json_returns_default() {
        let tmp = TempDir::new().unwrap();
        write_devcon_json(tmp.path(), "not json");
        let cfg = load_file(Some(&project_config_path(tmp.path())));
        assert!(cfg.shell.is_none());
    }

    #[test]
    fn persist_shell_creates_and_roundtrips() {
        let tmp = TempDir::new().unwrap();
        Config::persist_shell(tmp.path(), "/usr/bin/zsh").unwrap();
        let cfg = load_file(Some(&project_config_path(tmp.path())));
        assert_eq!(cfg.shell.as_deref(), Some("/usr/bin/zsh"));
    }

    #[test]
    fn persist_shell_preserves_other_keys() {
        let tmp = TempDir::new().unwrap();
        write_devcon_json(tmp.path(), r#"{"mouse": false}"#);
        Config::persist_shell(tmp.path(), "/bin/bash").unwrap();
        let cfg = load_file(Some(&project_config_path(tmp.path())));
        assert_eq!(cfg.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(cfg.mouse, Some(false));
    }
}
