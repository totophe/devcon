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

/// One row of `docker ps` with the labels we discriminate on.
struct PsRow {
    id: String,
    name: String,
    local_folder: String,
    compose_service: String,
}

/// Find the running Docker container that is the project's *dev container*.
///
/// This must be robust for compose stacks, where several services
/// (`<proj>-app-1`, `<proj>-postgres-1`, …) all share the project name — only
/// one of them is the dev container. Strategy, most reliable first:
///
///   1. `devcontainer.local_folder` label == the project root (exact).
///   2. Compose: the container whose `com.docker.compose.service` matches the
///      dev service — the one declared in devcontainer.json, else the service
///      whose workspace is bind-mounted at the project root.
///   3. Name heuristic: a container name containing the project folder — but
///      only after ruling out obvious sidecars, and preferring the declared
///      service name if we have one.
///
/// Returns `Ok(None)` when docker is reachable but nothing matches (stack down).
pub fn find(dc: &Devcontainer) -> Result<Option<Container>, Error> {
    let project_root = &dc.project_root;
    let output = run_docker(&[
        "ps",
        "--format",
        "{{.ID}}\t{{.Names}}\t{{.Label \"devcontainer.local_folder\"}}\t{{.Label \"com.docker.compose.service\"}}",
    ])?;

    let stdout = String::from_utf8_lossy(&output);
    let project_root_str = project_root.to_string_lossy();
    let project_folder = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let rows: Vec<PsRow> = stdout
        .lines()
        .filter_map(|line| {
            let c: Vec<&str> = line.splitn(4, '\t').collect();
            if c.len() < 2 {
                return None;
            }
            Some(PsRow {
                id: c[0].to_string(),
                name: c[1].trim().to_string(),
                local_folder: c.get(2).map(|s| s.trim().to_string()).unwrap_or_default(),
                compose_service: c.get(3).map(|s| s.trim().to_string()).unwrap_or_default(),
            })
        })
        .collect();

    // Pass 1: exact devcontainer.local_folder label match.
    if let Some(r) = rows
        .iter()
        .find(|r| r.local_folder == project_root_str.as_ref())
    {
        return Ok(Some(r.into()));
    }

    // Pass 2 (compose): match on the dev service.
    if dc.is_compose() {
        // The dev service: declared in json, or inferred from the compose file.
        let service = dc.service.clone().or_else(|| infer_dev_service(dc));
        if let Some(service) = service {
            if let Some(r) = rows.iter().find(|r| r.compose_service == service) {
                return Ok(Some(r.into()));
            }
        }
        // Still ambiguous: prefer a container whose workspace is bind-mounted at
        // the project root (the dev service mounts your code; sidecars don't).
        if let Some(r) = rows
            .iter()
            .filter(|r| {
                !r.compose_service.is_empty() && name_matches_project(&r.name, project_folder)
            })
            .find(|r| mounts_project(&r.id, project_root))
        {
            return Ok(Some(r.into()));
        }
    }

    // Pass 3: name heuristic — but prefer the declared service name if any.
    if !project_folder.is_empty() {
        if let Some(svc) = &dc.service {
            if let Some(r) = rows
                .iter()
                .find(|r| r.name.contains(project_folder) && r.name.contains(svc.as_str()))
            {
                return Ok(Some(r.into()));
            }
        }
        if let Some(r) = rows
            .iter()
            .find(|r| name_matches_project(&r.name, project_folder))
        {
            return Ok(Some(r.into()));
        }
    }

    Ok(None)
}

impl From<&PsRow> for Container {
    fn from(r: &PsRow) -> Self {
        Container {
            id: r.id.clone(),
            name: r.name.clone(),
        }
    }
}

fn name_matches_project(name: &str, project_folder: &str) -> bool {
    !project_folder.is_empty() && name.contains(project_folder)
}

/// True if the container bind-mounts the project root (i.e. it's the dev
/// service that carries your code, not a db/cache sidecar).
fn mounts_project(container_id: &str, project_root: &Path) -> bool {
    let tmpl = "{{range .Mounts}}{{.Source}}\n{{end}}";
    match run_docker(&["inspect", "-f", tmpl, container_id]) {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out);
            let root = project_root.to_string_lossy();
            text.lines().any(|src| src.trim() == root.as_ref())
        }
        Err(_) => false,
    }
}

/// Infer the dev service from the compose file: the service whose `volumes`
/// bind-mount into `/workspaces` (the dev container convention). Best-effort
/// text scan — avoids a YAML dependency for a single heuristic.
fn infer_dev_service(dc: &Devcontainer) -> Option<String> {
    let compose_dir = dc.project_root.join(".devcontainer");
    for rel in &dc.compose_files {
        let path = compose_dir.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(svc) = scan_workspace_service(&text) {
            return Some(svc);
        }
    }
    None
}

/// Scan a compose file's text for the first service that mounts into
/// `/workspaces`. Indentation-based: services are 2-space keys under `services:`.
fn scan_workspace_service(text: &str) -> Option<String> {
    let mut in_services = false;
    let mut current: Option<String> = None;
    let mut found: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') || trimmed.is_empty() {
            continue;
        }
        // Top-level key?
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_services = trimmed.starts_with("services:");
            current = None;
            continue;
        }
        if !in_services {
            continue;
        }
        // A service name is a key indented exactly 2 spaces: "  app:".
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && trimmed.ends_with(':') {
            current = Some(trimmed.trim().trim_end_matches(':').to_string());
            continue;
        }
        // Any deeper line mentioning /workspaces marks the current service.
        if current.is_some() && line.contains("/workspaces") {
            found = current.clone();
            break;
        }
    }
    found
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

#[cfg(test)]
mod tests {
    use super::scan_workspace_service;

    #[test]
    fn finds_service_that_mounts_workspaces() {
        let compose = r#"
services:
  app:
    image: ghcr.io/wellmade-oss/dc-workbench:latest
    volumes:
      - .:/workspaces/wellmade-os:cached
    command: sleep infinity
  postgres:
    image: postgres:16
    volumes:
      - pgdata:/var/lib/postgresql/data
"#;
        assert_eq!(scan_workspace_service(compose).as_deref(), Some("app"));
    }

    #[test]
    fn picks_the_workspace_service_even_when_not_first() {
        let compose = r#"
services:
  postgres:
    image: postgres:16
  mailpit:
    image: axllent/mailpit
  app:
    volumes:
      - .:/workspaces/proj
"#;
        assert_eq!(scan_workspace_service(compose).as_deref(), Some("app"));
    }

    #[test]
    fn none_when_no_workspace_mount() {
        let compose = r#"
services:
  db:
    image: postgres:16
    volumes:
      - pgdata:/var/lib/postgresql/data
"#;
        assert_eq!(scan_workspace_service(compose), None);
    }

    #[test]
    fn ignores_workspaces_outside_services_block() {
        let compose = r#"
volumes:
  cache:
    driver_opts:
      device: /workspaces/whatever
services:
  db:
    image: postgres:16
"#;
        assert_eq!(scan_workspace_service(compose), None);
    }
}
