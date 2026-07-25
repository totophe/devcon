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

    // Pass 1: exact devcontainer.local_folder label match — but only accept a
    // container whose stack *kind* matches the config. Otherwise an orphaned
    // old-stack container (e.g. a leftover image container after the project
    // switched to compose — both carry the same local_folder) would shadow the
    // real one, and we'd reattach to the wrong stack.
    if let Some(r) = rows
        .iter()
        .find(|r| r.local_folder == project_root_str.as_ref() && row_stack_matches(dc, r))
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

/// True if a `docker ps` row's stack kind matches the config's. A container in
/// a compose stack carries `com.docker.compose.service`; a standalone image
/// container doesn't. Used to reject a same-path container of the wrong kind
/// (the image↔compose switch case).
fn row_stack_matches(dc: &Devcontainer, row: &PsRow) -> bool {
    let row_is_compose = !row.compose_service.is_empty();
    row_is_compose == dc.is_compose()
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

/// The container's creation timestamp (`.Created`), e.g.
/// `2026-07-12T09:14:22.123456789Z`. Stable for the life of the container —
/// used as the baseline for rebuild drift detection (is any stack file newer?).
pub fn created_at(container: &Container) -> Option<String> {
    let out = run_docker(&["inspect", "-f", "{{.Created}}", &container.id]).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The container's compose project (`com.docker.compose.project` label), if it
/// belongs to one. Present → the container is part of a compose stack; absent →
/// it's a standalone image container (e.g. one `devcon` created with
/// `docker run`). Lets us tell what kind of stack the *running* container is,
/// independent of what the current config declares — the signal for detecting
/// an image↔compose switch on rebuild.
pub fn compose_project(container: &Container) -> Option<String> {
    let tmpl = "{{index .Config.Labels \"com.docker.compose.project\"}}";
    let out = run_docker(&["inspect", "-f", tmpl, &container.id]).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() || s == "<no value>" {
        None
    } else {
        Some(s)
    }
}

/// True if the container is part of a compose stack (has a compose project
/// label). The inverse is a standalone image container.
pub fn is_compose_container(container: &Container) -> bool {
    compose_project(container).is_some()
}

/// Force-remove a container (`docker rm -f`). Used when rebuilding an
/// image-based stack, where recreation means tearing down the old container
/// before `docker run` makes a fresh one, and by `devcon down`.
pub fn remove(container: &Container) -> Result<(), Error> {
    run_docker(&["rm", "-f", &container.id]).map(|_| ())
}

/// Tear down an entire compose stack by project name (`docker compose -p NAME
/// down`). Used when a project switches *away* from compose (compose → image):
/// the new config no longer carries the compose files, so we remove the old
/// stack by the project label Docker recorded on it. Compose resolves the
/// stack's containers + network from that label, no compose file needed.
pub fn compose_down_project(project: &str) -> Result<(), Error> {
    run_docker(&["compose", "-p", project, "down"]).map(|_| ())
}

/// Stop a container without removing it (`docker stop`). Used by
/// `devcon down --stop`, which keeps the container so the next connect reuses
/// it. Returns the exit status (streams docker's own progress output).
pub fn exec_stop(container: &Container) -> Result<std::process::ExitStatus, Error> {
    Command::new("docker")
        .args(["stop", &container.id])
        .status()
        .map_err(map_spawn_err)
}

/// The container's current start timestamp (`State.StartedAt`), e.g.
/// `2026-07-12T09:14:22.123456789Z`. Changes on every (re)start, stable across
/// connects — so it keys "has postStart run for *this* start?".
pub fn started_at(container: &Container) -> Option<String> {
    let out = run_docker(&["inspect", "-f", "{{.State.StartedAt}}", &container.id]).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
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

/// A dev-container project discovered on this host by scanning containers.
/// One project may span several containers (a compose stack: app + sidecars).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Stable key: the host project path (`devcontainer.local_folder`) when
    /// known, else the compose project name. Human-facing and used for sorting.
    pub key: String,
    /// A friendly display name (the path's basename, or the compose project).
    pub name: String,
    /// Host directory the project lives in: `devcontainer.local_folder` for
    /// image/VS Code containers, or the compose `working_dir` for a stack.
    /// Empty when neither label is present (older Docker, or an odd launch).
    pub path: String,
    /// Kind of stack, for display: `"compose"` or `"container"`.
    pub kind: &'static str,
    /// True if at least one of the project's containers is running.
    pub running: bool,
    /// How many containers make up this project (all states).
    pub container_count: usize,
    /// True if any container carries devcon's own marker label — i.e. devcon
    /// created it (vs. VS Code or a plain compose project).
    pub devcon_managed: bool,
}

/// One raw row from the project-listing `docker ps -a` query.
struct ProjectRow {
    local_folder: String,
    compose_project: String,
    state: String,
    devcon_marker: bool,
    /// `com.docker.compose.project.working_dir` — where a compose stack was
    /// launched from. Empty for non-compose containers.
    working_dir: String,
}

/// List dev-container projects present on this host (running or stopped).
///
/// A "project" is a group of containers sharing an identity: the host path in
/// `devcontainer.local_folder` (VS Code and devcon image-based containers), or
/// the `com.docker.compose.project` label (compose stacks). By default only
/// containers that look like dev containers are counted; `all` widens the net
/// to *every* compose project on the host.
pub fn list_projects(all: bool) -> Result<Vec<Project>, Error> {
    let output = run_docker(&[
        "ps",
        "-a",
        "--format",
        "{{.Label \"devcontainer.local_folder\"}}\t{{.Label \"com.docker.compose.project\"}}\t{{.State}}\t{{.Label \"dev.devcon.postcreate\"}}\t{{.Label \"com.docker.compose.project.working_dir\"}}",
    ])?;
    let stdout = String::from_utf8_lossy(&output);
    let rows = stdout.lines().filter_map(parse_project_row);
    Ok(group_projects(rows, all))
}

/// Parse one tab-separated project-listing row. Returns `None` for blank lines.
fn parse_project_row(line: &str) -> Option<ProjectRow> {
    let c: Vec<&str> = line.splitn(5, '\t').collect();
    if c.iter().all(|f| f.trim().is_empty()) {
        return None;
    }
    let marker = c.get(3).map(|s| s.trim()).unwrap_or("");
    let working_dir = c.get(4).map(|s| s.trim()).unwrap_or("");
    Some(ProjectRow {
        local_folder: c.first().map(|s| s.trim().to_string()).unwrap_or_default(),
        compose_project: c.get(1).map(|s| s.trim().to_string()).unwrap_or_default(),
        state: c.get(2).map(|s| s.trim().to_string()).unwrap_or_default(),
        devcon_marker: !marker.is_empty() && marker != "<no value>",
        working_dir: if working_dir == "<no value>" {
            String::new()
        } else {
            working_dir.to_string()
        },
    })
}

/// Group container rows into projects. Pure (no docker calls) so it's testable.
///
/// Keying: a container with a `devcontainer.local_folder` is keyed on that path
/// (VS Code / devcon image-based); otherwise a compose container is keyed on its
/// `com.docker.compose.project`. When `all` is false, compose-only containers
/// with no dev-container signal are dropped so the list stays dev-focused.
fn group_projects(rows: impl Iterator<Item = ProjectRow>, all: bool) -> Vec<Project> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, Project> = BTreeMap::new();
    // Track which project keys have a genuine dev-container signal on any of
    // their containers — a compose stack's sidecars don't carry it, so we must
    // decide "is this a dev project?" per group, not per container.
    let mut dev_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for r in rows {
        let has_devcontainer = !r.local_folder.is_empty();
        let is_compose = !r.compose_project.is_empty();

        if !has_devcontainer && !is_compose {
            // A devcon-marked image container missing its local_folder label —
            // nothing to key it on. (Shouldn't happen; devcon always sets it.)
            continue;
        }

        // Key on the compose project when present so all of a stack's
        // containers merge — even the dev-container member that also carries a
        // `local_folder`. Standalone image containers key on their path.
        let key = if is_compose {
            r.compose_project.clone()
        } else {
            r.local_folder.clone()
        };
        // Friendly name: the project path's basename if we have one, else the
        // compose project name.
        let name = if has_devcontainer {
            r.local_folder
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&r.local_folder)
                .to_string()
        } else {
            r.compose_project.clone()
        };
        let kind = if is_compose { "compose" } else { "container" };
        // Where the project lives: the dev container's own host path when it
        // carries one, else the compose stack's launch directory.
        let path = if has_devcontainer {
            r.local_folder.clone()
        } else {
            r.working_dir.clone()
        };

        let running = r.state.eq_ignore_ascii_case("running");
        let entry = map.entry(key.clone()).or_insert_with(|| Project {
            key,
            name: name.clone(),
            path: path.clone(),
            kind,
            running: false,
            container_count: 0,
            devcon_managed: false,
        });
        entry.container_count += 1;
        entry.running |= running;
        entry.devcon_managed |= r.devcon_marker;
        // Prefer a real project-path basename over a compose-project name once
        // any member reveals one (the dev-container member carries the path).
        if has_devcontainer {
            entry.name = name;
        }
        // The dev-container member's local_folder is the authoritative path;
        // otherwise fill from a compose working_dir if we didn't have one yet.
        if has_devcontainer {
            entry.path = path;
        } else if entry.path.is_empty() && !r.working_dir.is_empty() {
            entry.path = r.working_dir.clone();
        }
        // A devcontainer label or devcon marker on *any* member makes the whole
        // group a dev project.
        if has_devcontainer || r.devcon_marker {
            dev_keys.insert(entry.key.clone());
        }
    }

    // Keep every group when `all`; otherwise only those with a dev signal.
    map.into_values()
        .filter(|p| all || dev_keys.contains(&p.key))
        .collect()
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
    use super::{group_projects, row_stack_matches, scan_workspace_service, ProjectRow, PsRow};
    use crate::devcontainer::Devcontainer;
    use std::path::PathBuf;

    /// Minimal Devcontainer for stack-kind checks: compose iff `compose` is set.
    fn dc(compose: bool) -> Devcontainer {
        Devcontainer {
            project_root: PathBuf::from("/home/u/proj"),
            config_file: PathBuf::from("/home/u/proj/.devcontainer/devcontainer.json"),
            dockerfile: None,
            image: if compose {
                None
            } else {
                Some("img:latest".into())
            },
            compose_files: if compose {
                vec!["compose.yml".into()]
            } else {
                vec![]
            },
            service: None,
            run_services: vec![],
            workspace_folder: None,
            remote_user: None,
            post_create: vec![],
            post_start: vec![],
            name: None,
        }
    }

    fn ps_row(compose_service: &str) -> PsRow {
        PsRow {
            id: "abc".into(),
            name: "container".into(),
            local_folder: "/home/u/proj".into(),
            compose_service: compose_service.into(),
        }
    }

    #[test]
    fn row_stack_matches_rejects_wrong_kind() {
        let image_row = ps_row(""); // no compose service → standalone image
        let compose_row = ps_row("workbench"); // compose member

        // A compose config must not match a leftover image container sharing the
        // same local_folder (the image→compose switch reattach bug), and vice
        // versa. Same-kind rows still match.
        assert!(!row_stack_matches(&dc(true), &image_row));
        assert!(row_stack_matches(&dc(true), &compose_row));
        assert!(row_stack_matches(&dc(false), &image_row));
        assert!(!row_stack_matches(&dc(false), &compose_row));
    }

    fn row(local_folder: &str, compose: &str, state: &str, marker: bool) -> ProjectRow {
        ProjectRow {
            local_folder: local_folder.into(),
            compose_project: compose.into(),
            state: state.into(),
            devcon_marker: marker,
            working_dir: String::new(),
        }
    }

    /// A compose sidecar row carrying only the stack's `working_dir` (no
    /// devcontainer label — the shape `deploy`-style stacks show up as).
    fn compose_row(compose: &str, state: &str, working_dir: &str) -> ProjectRow {
        ProjectRow {
            local_folder: String::new(),
            compose_project: compose.into(),
            state: state.into(),
            devcon_marker: false,
            working_dir: working_dir.into(),
        }
    }

    #[test]
    fn groups_compose_stack_into_one_project() {
        // A compose dev container + two sidecars, all one project.
        let rows = vec![
            row("/home/u/proj", "proj_devcontainer", "running", false),
            row("", "proj_devcontainer", "running", false),
            row("", "proj_devcontainer", "exited", false),
        ];
        let projects = group_projects(rows.into_iter(), false);
        assert_eq!(projects.len(), 1);
        let p = &projects[0];
        assert_eq!(p.name, "proj");
        assert_eq!(p.kind, "compose");
        assert!(p.running);
        assert_eq!(p.container_count, 3);
    }

    #[test]
    fn image_based_devcon_container_is_a_project() {
        let rows = vec![row("/home/u/app", "", "running", true)];
        let projects = group_projects(rows.into_iter(), false);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].kind, "container");
        assert!(projects[0].devcon_managed);
    }

    #[test]
    fn plain_compose_projects_hidden_without_all() {
        // No devcontainer label, no devcon marker → not a dev container.
        let hidden = group_projects(
            vec![row("", "some_service_stack", "running", false)].into_iter(),
            false,
        );
        assert!(hidden.is_empty());
        let shown = group_projects(
            vec![row("", "some_service_stack", "running", false)].into_iter(),
            true,
        );
        assert_eq!(shown.len(), 1);
    }

    #[test]
    fn compose_stack_path_comes_from_working_dir() {
        // A plain compose stack (like `deploy`): no devcontainer label, but its
        // working_dir tells us where it lives. Shown with --all.
        let projects = group_projects(
            vec![
                compose_row("deploy", "running", "/srv/deploy"),
                compose_row("deploy", "running", "/srv/deploy"),
            ]
            .into_iter(),
            true,
        );
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "deploy");
        assert_eq!(projects[0].path, "/srv/deploy");
    }

    #[test]
    fn devcontainer_path_wins_over_compose_working_dir() {
        // The dev-container member's local_folder is authoritative even when a
        // sidecar reports a different working_dir.
        let projects = group_projects(
            vec![
                compose_row("proj", "running", "/tmp/build"),
                row("/home/u/proj", "proj", "running", false),
            ]
            .into_iter(),
            false,
        );
        assert_eq!(projects[0].path, "/home/u/proj");
    }

    #[test]
    fn stopped_project_reports_not_running() {
        let rows = vec![row("/home/u/x", "", "exited", true)];
        let projects = group_projects(rows.into_iter(), false);
        assert!(!projects[0].running);
    }

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
