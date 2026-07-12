//! Resolve which shell to drop the user into, and remember the answer.
//!
//! Precedence:
//!   1. `--shell` flag
//!   2. `.devcontainer/devcon.json` (persisted from a previous run)
//!   3. auto-detect inside the container (`$SHELL`, then probe zsh→bash→sh)
//!   4. if auto-detect is ambiguous, prompt once and persist
//!
//! "Ambiguous" means we could not read a concrete `$SHELL` *and* more than one
//! candidate shell exists in the container — so we ask rather than guess.

use crate::config::Config;
use crate::docker::{self, Container};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

const CANDIDATES: &[&str] = &["/bin/zsh", "/usr/bin/zsh", "/bin/bash", "/bin/sh"];

/// Resolve the shell, persisting a freshly-prompted choice into the project's
/// devcon.json. `flag` is the value of `--shell`, `cfg_shell` is what config
/// already holds.
pub fn resolve(
    flag: Option<&str>,
    cfg_shell: Option<&str>,
    container: &Container,
    project_root: &Path,
) -> String {
    if let Some(s) = flag {
        return s.to_string();
    }
    if let Some(s) = cfg_shell {
        return s.to_string();
    }

    match detect(container) {
        Detection::Certain(shell) => shell,
        Detection::Ambiguous(found) => {
            let chosen = prompt(&found).unwrap_or_else(|| found[0].clone());
            // Persist so we never ask again for this project.
            if let Err(e) = Config::persist_shell(project_root, &chosen) {
                eprintln!("warning: could not save shell choice: {e}");
            }
            chosen
        }
    }
}

enum Detection {
    /// We are confident about the shell (from `$SHELL` or a single candidate).
    Certain(String),
    /// Multiple candidates exist and `$SHELL` was unreadable — ask the user.
    Ambiguous(Vec<String>),
}

/// Inspect the container to decide the shell.
fn detect(container: &Container) -> Detection {
    // 1. Honor the container's own $SHELL if it points at a real path.
    if let Some(shell) = read_env_shell(container) {
        return Detection::Certain(shell);
    }

    // 2. Probe candidates. If exactly one exists → certain; if several → ask.
    let present: Vec<String> = CANDIDATES
        .iter()
        .filter(|c| is_executable(container, c))
        .map(|c| c.to_string())
        .collect();

    match present.len() {
        0 => Detection::Certain("/bin/sh".to_string()),
        1 => Detection::Certain(present.into_iter().next().unwrap()),
        _ => Detection::Ambiguous(present),
    }
}

/// `docker exec <c> sh -c 'echo $SHELL'`, accepting only an absolute path that
/// actually exists in the container.
fn read_env_shell(container: &Container) -> Option<String> {
    let out = docker::exec_capture(container, &["sh", "-c", "echo $SHELL"]).ok()?;
    let shell = String::from_utf8_lossy(&out).trim().to_string();
    if shell.starts_with('/') && is_executable(container, &shell) {
        Some(shell)
    } else {
        None
    }
}

fn is_executable(container: &Container, path: &str) -> bool {
    docker::exec_status(container, &["test", "-x", path])
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask which shell to use. Cooked-mode prompt on stderr. Returns the chosen
/// path, or `None` on EOF / non-TTY (caller falls back to the first candidate).
fn prompt(candidates: &[String]) -> Option<String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return None;
    }
    let mut err = io::stderr();
    let _ = writeln!(
        err,
        "\x1b[36mdevcon:\x1b[0m multiple shells available — pick one:"
    );
    for (i, c) in candidates.iter().enumerate() {
        let _ = writeln!(err, "  {}) {}", i + 1, c);
    }
    let _ = write!(err, "Choice [1]: ");
    let _ = err.flush();

    let mut line = String::new();
    io::stdin().read_line(&mut line).ok()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Some(candidates[0].clone());
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n >= 1 && n <= candidates.len() => Some(candidates[n - 1].clone()),
        _ => Some(candidates[0].clone()),
    }
}
