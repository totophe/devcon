//! All interaction with the `docker` CLI: finding the project's container,
//! inspecting a running one for ground-truth facts (workspace mount
//! destination, the run-once marker label), and running the up-front `ps`
//! query.

use crate::devcontainer::Devcontainer;
use std::path::Path;
use std::process::Command;

/// The label `devcon` stamps on containers *it creates* (image-based) at
/// `docker run` time, recording that postCreate ran. Read via `docker inspect`.
pub const MARKER_LABEL: &str = "dev.devcon.postcreate";

/// Portable run-once sentinel written *inside* the container. Works even for
/// compose services `devcon` did not create (where we can't add a label after
/// the fact). Checked alongside [`MARKER_LABEL`].
pub const MARKER_SENTINEL: &str = "/tmp/.devcon-postcreate-done";

/// A running container matched to a project.
#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub name: String,
}

/// Find the running Docker container associated with the given project root.
///
/// Strategy (in order):
///   1. Match on the `devcontainer.local_folder` label — most reliable.
///   2. Fall back to a name heuristic: the container name contains the
///      last path component of the project root.
///
/// Returns `Ok(None)` when docker is reachable but no matching container is
/// running (i.e. the stack is down).
pub fn find(project_root: &Path) -> Result<Option<Container>, Error> {
    let output = run_docker(&[
        "ps",
        "--format",
        "{{.ID}}\t{{.Names}}\t{{.Label \"devcontainer.local_folder\"}}",
    ])?;

    let stdout = String::from_utf8_lossy(&output);
    let project_root_str = project_root.to_string_lossy();
    let project_folder = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Pass 1: label match (exact path)
    for line in stdout.lines() {
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        if cols.len() < 3 {
            continue;
        }
        if cols[2].trim() == project_root_str.as_ref() {
            return Ok(Some(Container {
                id: cols[0].to_string(),
                name: cols[1].to_string(),
            }));
        }
    }

    // Pass 2: name heuristic — container name contains the project folder name
    if !project_folder.is_empty() {
        for line in stdout.lines() {
            let cols: Vec<&str> = line.splitn(3, '\t').collect();
            if cols.len() < 2 {
                continue;
            }
            if cols[1].trim().contains(project_folder) {
                return Ok(Some(Container {
                    id: cols[0].to_string(),
                    name: cols[1].trim().to_string(),
                }));
            }
        }
    }

    Ok(None)
}

/// The container-side path where the workspace is mounted, read from the live
/// container. Returns `None` if inspect fails or no mount matches the project.
///
/// We look for a mount whose *source* is the project root (bind) or whose
/// *destination* looks like a `/workspaces/...` path.
pub fn workspace_mount_destination(container: &Container, project_root: &Path) -> Option<String> {
    // Format: one line per mount, "<source>\t<destination>".
    let tmpl = "{{range .Mounts}}{{.Source}}\t{{.Destination}}\n{{end}}";
    let out = run_docker(&["inspect", "-f", tmpl, &container.id]).ok()?;
    let text = String::from_utf8_lossy(&out);
    let project_root_str = project_root.to_string_lossy();

    let mut workspaces_fallback: Option<String> = None;
    for line in text.lines() {
        let mut parts = line.splitn(2, '\t');
        let source = parts.next().unwrap_or("").trim();
        let dest = parts.next().unwrap_or("").trim();
        if dest.is_empty() {
            continue;
        }
        // Exact bind of the project root → authoritative.
        if source == project_root_str.as_ref() {
            return Some(dest.to_string());
        }
        // Otherwise remember the first /workspaces/* destination as a fallback.
        if workspaces_fallback.is_none() && dest.starts_with("/workspaces/") {
            workspaces_fallback = Some(dest.to_string());
        }
    }
    workspaces_fallback
}

/// True if postCreate has already run for this container. Checks two signals:
///   1. the [`MARKER_LABEL`] on containers `devcon` created (image-based), and
///   2. the in-container [`MARKER_SENTINEL`] file (portable, incl. compose).
pub fn has_marker(container: &Container) -> bool {
    // 1. Label check (cheap, no exec).
    let tmpl = format!("{{{{index .Config.Labels \"{MARKER_LABEL}\"}}}}");
    if let Ok(out) = run_docker(&["inspect", "-f", &tmpl, &container.id]) {
        let v = String::from_utf8_lossy(&out);
        let v = v.trim();
        if !v.is_empty() && v != "<no value>" {
            return true;
        }
    }

    // 2. Sentinel file check.
    let status = Command::new("docker")
        .args(["exec", &container.id, "test", "-f", MARKER_SENTINEL])
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Run `docker exec` (streaming, inheriting stdio) inside the container.
/// Returns the exit status.
pub fn exec_command(
    container: &Container,
    user: Option<&str>,
    workdir: Option<&str>,
    argv: &[&str],
) -> Result<std::process::ExitStatus, Error> {
    let mut args: Vec<String> = vec!["exec".into(), "-i".into()];
    if let Some(u) = user {
        args.push("-u".into());
        args.push(u.to_string());
    }
    if let Some(w) = workdir {
        args.push("-w".into());
        args.push(w.to_string());
    }
    args.push(container.id.clone());
    args.extend(argv.iter().map(|s| s.to_string()));

    Command::new("docker")
        .args(&args)
        .status()
        .map_err(map_spawn_err)
}

/// Resolve which user to `docker exec -u` as. Returns the declared `remoteUser`
/// only if it actually exists in the container's passwd database; otherwise
/// `None` (exec as the image's default user).
///
/// `devcontainer.json` may declare a `remoteUser` that the *running* image
/// doesn't provide (e.g. the container was started from a different base image
/// than the config assumes). Passing `-u <missing-user>` makes `docker exec`
/// fail with "unable to find user … in passwd file", so we probe first.
pub fn resolve_user(container: &Container, declared: Option<&str>) -> Option<String> {
    let user = declared?;
    if user_exists(container, user) {
        Some(user.to_string())
    } else {
        eprintln!(
            "\x1b[33mdevcon:\x1b[0m remoteUser '{user}' not found in container — \
             using the image's default user instead"
        );
        None
    }
}

/// True if `user` (name or numeric uid) resolves inside the container.
fn user_exists(container: &Container, user: &str) -> bool {
    // `id <user>` succeeds for both names and numeric uids that exist.
    Command::new("docker")
        .args(["exec", &container.id, "id", user])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `docker exec <container> <argv...>` capturing stdout (no TTY). Used for
/// probes like `echo $SHELL`. Errors on non-zero exit.
pub fn exec_capture(container: &Container, argv: &[&str]) -> Result<Vec<u8>, Error> {
    let mut args = vec!["exec", &container.id];
    args.extend_from_slice(argv);
    run_docker(&args)
}

/// Run `docker exec <container> <argv...>` for its status only (e.g. `test -x`).
pub fn exec_status(
    container: &Container,
    argv: &[&str],
) -> Result<std::process::ExitStatus, Error> {
    let mut args = vec!["exec".to_string(), container.id.clone()];
    args.extend(argv.iter().map(|s| s.to_string()));
    Command::new("docker")
        .args(&args)
        .status()
        .map_err(map_spawn_err)
}

/// Run a docker subcommand, capturing stdout. Errors on non-zero exit.
fn run_docker(args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(map_spawn_err)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(Error::CommandFailed(
            args.first().unwrap_or(&"").to_string(),
            stderr,
        ));
    }
    Ok(output.stdout)
}

fn map_spawn_err(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::DockerNotFound
    } else {
        Error::Io(e)
    }
}

/// Convenience: does this project resolve to a compose stack or a single image?
/// (Kept here so callers only need one import surface for docker concerns.)
pub fn describe_kind(dc: &Devcontainer) -> &'static str {
    if dc.is_compose() {
        "compose stack"
    } else {
        "container"
    }
}

#[derive(Debug)]
pub enum Error {
    DockerNotFound,
    CommandFailed(String, String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DockerNotFound => write!(
                f,
                "docker not found — install Docker and make sure it is on your PATH"
            ),
            Error::CommandFailed(cmd, msg) => write!(f, "docker {cmd} failed: {msg}"),
            Error::Io(e) => write!(f, "i/o error running docker: {e}"),
        }
    }
}
