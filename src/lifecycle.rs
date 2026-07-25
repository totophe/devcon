//! Dev-container lifecycle: bring the stack up when it's down, then run the
//! declared `postCreateCommand` exactly once per container (tracked with a
//! label). This is the piece VS Code normally owns; `devcon` reproduces just
//! the subset these images need.

use crate::devcontainer::{Devcontainer, PostCreateCommand};
use crate::docker::{self, Container, MARKER_SENTINEL};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

/// How the caller wants rebuild-on-drift handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuild {
    /// Detect drift (a stack file edited since the container was built) and
    /// prompt before recreating. This is the default.
    Auto,
    /// Always recreate the container, regardless of drift (`--rebuild`).
    Force,
    /// Never recreate; connect to whatever is running (`--no-rebuild`).
    Never,
}

/// Bring the stack up if needed and ensure lifecycle hooks have run.
///
/// Runs, in order:
///   - rebuild-on-drift — if the running container predates an edit to a stack
///     file (`devcontainer.json`, a compose file, the Dockerfile), recreate it
///     so the change takes effect (prompting first, unless forced).
///   - `postCreateCommand` — once per container *creation* (identity-keyed
///     label/sentinel; survives restarts).
///   - `postStartCommand` — once per container *start* (keyed on the container's
///     `StartedAt`, so it re-runs after a real restart but not on re-connects).
///
/// Returns `Ok(None)` if the stack is down and the user declined to start it.
pub fn ensure_up(
    dc: &Devcontainer,
    existing: Option<Container>,
    assume_yes: bool,
    rebuild: Rebuild,
) -> Result<Option<Container>, Error> {
    let container = match existing {
        Some(c) => {
            // Container is up: recreate it if the stack drifted (or was forced).
            match maybe_rebuild(dc, &c, assume_yes, rebuild)? {
                Some(fresh) => fresh,
                None => c,
            }
        }
        None => {
            // Stack is down — ask before doing anything heavyweight.
            if !assume_yes && !confirm_start(dc)? {
                return Ok(None);
            }
            bring_up(dc)?;
            // Re-discover the container now that it's running.
            docker::find(dc)
                .map_err(Error::Docker)?
                .ok_or_else(|| Error::NotUpAfterStart(docker::diagnose_not_running(dc)))?
        }
    };

    // postCreate: once per creation. A hook failure is warned, NOT fatal — a
    // broken postCreate must not lock you out of an otherwise-healthy container.
    // The marker is only stamped on success (inside run_post_create), so it
    // retries on the next connect.
    if !dc.post_create.is_empty() && !docker::has_marker(&container) {
        if let Err(e) = run_post_create(dc, &container) {
            warn_lifecycle(&e);
        }
    }

    // postStart: once per start (keyed on StartedAt). Also non-fatal.
    if !dc.post_start.is_empty() {
        if let Err(e) = run_post_start_if_needed(dc, &container) {
            warn_lifecycle(&e);
        }
    }

    Ok(Some(container))
}

/// Interactive Y/n prompt on stderr. Non-TTY → default to no (never block a
/// script). Empty answer → yes (the common case).
fn confirm_start(dc: &Devcontainer) -> Result<bool, Error> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Ok(false);
    }
    let kind = docker::describe_kind(dc);
    let name = dc.name.as_deref().unwrap_or("this project");
    let mut err = io::stderr();
    let _ = write!(
        err,
        "\x1b[36mdevcon:\x1b[0m {name} {kind} isn't running. Start it? [Y/n] "
    );
    let _ = err.flush();

    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(Error::Io)?;
    let answer = line.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Recreate the running container when the stack has drifted (or a rebuild was
/// forced). Returns `Some(fresh_container)` if a rebuild happened, `None` if we
/// left the existing container in place.
///
/// Drift = any stack-defining file (`devcontainer.json`, a compose file, the
/// Dockerfile) has an mtime newer than the container's creation time. This is
/// devcon's stand-in for VS Code's "Rebuild Container".
fn maybe_rebuild(
    dc: &Devcontainer,
    container: &Container,
    assume_yes: bool,
    rebuild: Rebuild,
) -> Result<Option<Container>, Error> {
    if rebuild == Rebuild::Never {
        return Ok(None);
    }

    // What kind of stack is the *running* container, vs. what the config now
    // declares? A mismatch (image ⇄ compose) means the devcontainer switched
    // stack type — connecting to the old container would land us in the wrong
    // stack entirely, so this always counts as needing a rebuild.
    let existing_compose = docker::is_compose_container(container);
    let type_changed = existing_compose != dc.is_compose();

    let do_rebuild = match rebuild {
        Rebuild::Never => unreachable!("handled above"),
        Rebuild::Force => true,
        Rebuild::Auto => {
            let mut changed = drifted_files(dc, container);
            if type_changed {
                // Surface the switch first — it's the decisive reason, and a
                // stronger signal than any file mtime.
                changed.insert(
                    0,
                    format!(
                        "its stack type ({} → {})",
                        kind_word(existing_compose),
                        kind_word(dc.is_compose()),
                    ),
                );
            }
            if changed.is_empty() {
                false
            } else if !assume_yes && (!io::stdin().is_terminal() || !io::stderr().is_terminal()) {
                // Non-interactive (and not -y): warn but connect anyway — never
                // silently drop someone's container from a script.
                let list = changed.join(", ");
                eprintln!(
                    "\x1b[33mdevcon:\x1b[0m {list} changed since this container was built — \
                     run `devcon --rebuild` to recreate it."
                );
                false
            } else {
                assume_yes || confirm_rebuild(&changed)?
            }
        }
    };

    if !do_rebuild {
        return Ok(None);
    }

    rebuild_stack(dc, container, existing_compose)?;
    let fresh = docker::find(dc)
        .map_err(Error::Docker)?
        .ok_or_else(|| Error::NotUpAfterStart(docker::diagnose_not_running(dc)))?;
    Ok(Some(fresh))
}

/// `"compose"` / `"container"` — for drift/rebuild messages.
fn kind_word(is_compose: bool) -> &'static str {
    if is_compose {
        "compose"
    } else {
        "container"
    }
}

/// The stack files whose mtime is newer than the container's creation time.
/// Empty when the container is up to date (or when we can't read `.Created`,
/// in which case we conservatively report no drift rather than nag).
fn drifted_files(dc: &Devcontainer, container: &Container) -> Vec<String> {
    let Some(created) = docker::created_at(container).and_then(|s| parse_rfc3339_secs(&s)) else {
        return Vec::new();
    };
    dc.stack_files()
        .into_iter()
        .filter(|f| file_newer_than(f, created))
        .map(|f| display_relative(&f, &dc.project_root))
        .collect()
}

/// True if `path`'s mtime is strictly after `created` (Unix seconds).
fn file_newer_than(path: &Path, created: i64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64) > created,
        Err(_) => false,
    }
}

/// Present a stack file relative to the project root when possible, for a
/// tidier message (`.devcontainer/docker-compose.yml` beats an absolute path).
fn display_relative(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Interactive rebuild prompt on stderr. Defaults to **No** — recreating drops
/// the container, so we only proceed on an explicit yes.
fn confirm_rebuild(changed: &[String]) -> Result<bool, Error> {
    let list = changed.join(", ");
    let mut err = io::stderr();
    let _ = write!(
        err,
        "\x1b[36mdevcon:\x1b[0m {list} changed since this container was built. \
         Rebuild it? [y/N] "
    );
    let _ = err.flush();

    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(Error::Io)?;
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Recreate the container so stack changes take effect.
///
/// The old stack is torn down according to what the running container *actually*
/// is (`existing_compose`), which may differ from what the config now declares
/// when the project switched stack type. This is what stops an image↔compose
/// switch from leaving an orphaned old container that a later `find()` would
/// reattach to. An old image container is always `docker rm -f`'d (even when the
/// new stack is compose — otherwise the `docker run` container survives
/// untouched); an old compose stack is brought fully down by its project label
/// (the new image-based config no longer carries the compose files). A same-kind
/// rebuild takes the fast path: compose force-recreates in place, image re-runs
/// after removal.
///
/// Either way the fresh container has no run-once markers, so
/// `postCreateCommand`/`postStartCommand` re-run naturally afterward.
fn rebuild_stack(
    dc: &Devcontainer,
    container: &Container,
    existing_compose: bool,
) -> Result<(), Error> {
    eprintln!("\x1b[36mdevcon:\x1b[0m rebuilding…");
    let target_compose = dc.is_compose();

    // Tear down the OLD stack by its real kind.
    if existing_compose && !target_compose {
        // compose → image: remove the whole old stack, not just the dev service.
        tear_down_old_compose(container)?;
    } else if !existing_compose {
        // Old is a standalone image container (target image or compose): remove
        // it so nothing can reattach to it. For a same-kind compose rebuild we
        // skip this and let `--force-recreate` handle it in place.
        docker::remove(container).map_err(Error::Docker)?;
    }

    // Bring up the NEW stack.
    if target_compose {
        rebuild_compose(dc)
    } else {
        bring_up_image(dc)
    }
}

/// Bring the old compose stack down by its project label. Used only on a
/// compose → image switch, where the current (image-based) config no longer
/// knows the compose files. Falls back to removing just the discovered
/// container if the label is somehow absent.
fn tear_down_old_compose(container: &Container) -> Result<(), Error> {
    match docker::compose_project(container) {
        Some(project) => {
            eprintln!("\x1b[36mdevcon:\x1b[0m removing old compose stack '{project}'…");
            docker::compose_down_project(&project).map_err(Error::Docker)
        }
        None => docker::remove(container).map_err(Error::Docker),
    }
}

/// `docker compose up -d --build --force-recreate` for the dev service(s).
/// `--build` picks up Dockerfile edits; `--force-recreate` picks up compose /
/// devcontainer.json edits even when the image is unchanged.
fn rebuild_compose(dc: &Devcontainer) -> Result<(), Error> {
    let compose_dir = dc.project_root.join(".devcontainer");

    let mut args: Vec<String> = vec!["compose".into()];
    for f in &dc.compose_files {
        args.push("-f".into());
        args.push(f.clone());
    }
    args.push("up".into());
    args.push("-d".into());
    args.push("--build".into());
    args.push("--force-recreate".into());
    if !dc.run_services.is_empty() {
        args.extend(dc.run_services.iter().cloned());
    } else if let Some(svc) = &dc.service {
        args.push(svc.clone());
    }

    let status = Command::new("docker")
        .args(&args)
        .current_dir(&compose_dir)
        .status()
        .map_err(map_spawn_err)?;
    if !status.success() {
        return Err(Error::BringUpFailed(
            "docker compose up --build failed".into(),
        ));
    }
    Ok(())
}

/// Parse a Docker `.Created`/RFC3339 UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fff…]Z`) to Unix epoch seconds. Docker always emits
/// UTC with a `Z` suffix, so we don't handle numeric offsets. Returns `None`
/// on any shape we don't recognize (caller treats that as "no drift").
fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;

    // Time portion, dropping any fractional seconds and the trailing 'Z'.
    let time = rest.trim_end_matches('Z').split('.').next().unwrap_or(rest);
    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let min: i64 = tparts.next()?.parse().ok()?;
    let sec: i64 = tparts.next()?.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days from the Unix epoch (1970-01-01) to this date, via a civil-calendar
    // algorithm (Howard Hinnant's days_from_civil) — no leap-year edge cases.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe - 719468;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// How far `devcon down` tears the stack down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TearDown {
    /// Remove the container(s) (and, for compose, the network): `compose down`
    /// / `docker rm -f`. The default — the counterpart to bringing the stack up.
    Remove,
    /// Stop but keep the container(s): `compose stop` / `docker stop`. A later
    /// `devcon` reconnects to the same container without recreating it.
    Stop,
}

/// Bring the project's stack down. Compose stacks use `docker compose down`
/// (or `stop`); image-based ones remove (or stop) the discovered container.
///
/// `existing` is the container `docker::find` located, if any. For compose,
/// `down` works from the compose files even when we couldn't pinpoint the dev
/// container, so a missing `existing` isn't fatal there; for image-based stacks
/// there's nothing to do without a container.
pub fn bring_down(
    dc: &Devcontainer,
    existing: Option<&Container>,
    mode: TearDown,
) -> Result<(), Error> {
    if dc.is_compose() {
        bring_down_compose(dc, mode)
    } else {
        match existing {
            Some(c) => bring_down_image(c, mode),
            None => {
                eprintln!("\x1b[36mdevcon:\x1b[0m nothing to stop — no container is running.");
                Ok(())
            }
        }
    }
}

/// `docker compose … down` (or `stop`) for the project's compose stack.
/// `down` on the whole stack tears down every service + the network; `stop`
/// leaves the containers in place. We don't scope to `runServices` here — down
/// means down.
fn bring_down_compose(dc: &Devcontainer, mode: TearDown) -> Result<(), Error> {
    let verb = match mode {
        TearDown::Remove => "down",
        TearDown::Stop => "stop",
    };
    eprintln!("\x1b[36mdevcon:\x1b[0m {verb} compose stack…");
    let compose_dir = dc.project_root.join(".devcontainer");

    let mut args: Vec<String> = vec!["compose".into()];
    for f in &dc.compose_files {
        args.push("-f".into());
        args.push(f.clone());
    }
    args.push(verb.into());

    let status = Command::new("docker")
        .args(&args)
        .current_dir(&compose_dir)
        .status()
        .map_err(map_spawn_err)?;
    if !status.success() {
        return Err(Error::BringDownFailed(format!(
            "docker compose {verb} failed"
        )));
    }
    Ok(())
}

/// Stop or remove a single image-based container.
fn bring_down_image(container: &Container, mode: TearDown) -> Result<(), Error> {
    match mode {
        TearDown::Remove => {
            eprintln!("\x1b[36mdevcon:\x1b[0m removing {}…", container.name);
            docker::remove(container).map_err(Error::Docker)
        }
        TearDown::Stop => {
            eprintln!("\x1b[36mdevcon:\x1b[0m stopping {}…", container.name);
            let status = docker::exec_stop(container).map_err(Error::Docker)?;
            if !status.success() {
                return Err(Error::BringDownFailed("docker stop failed".into()));
            }
            Ok(())
        }
    }
}

/// Bring the container up: `docker compose up -d` for compose stacks, or
/// `docker run` for image-based ones.
fn bring_up(dc: &Devcontainer) -> Result<(), Error> {
    if dc.is_compose() {
        bring_up_compose(dc)
    } else {
        bring_up_image(dc)
    }
}

fn bring_up_compose(dc: &Devcontainer) -> Result<(), Error> {
    eprintln!("\x1b[36mdevcon:\x1b[0m starting compose stack…");
    let compose_dir = dc.project_root.join(".devcontainer");

    let mut args: Vec<String> = vec!["compose".into()];
    for f in &dc.compose_files {
        args.push("-f".into());
        args.push(f.clone());
    }
    args.push("up".into());
    args.push("-d".into());
    // Only bring up the declared runServices (or the primary service) if named.
    if !dc.run_services.is_empty() {
        args.extend(dc.run_services.iter().cloned());
    } else if let Some(svc) = &dc.service {
        args.push(svc.clone());
    }

    let status = Command::new("docker")
        .args(&args)
        .current_dir(&compose_dir)
        .status()
        .map_err(map_spawn_err)?;

    if !status.success() {
        return Err(Error::BringUpFailed("docker compose up failed".into()));
    }
    Ok(())
}

fn bring_up_image(dc: &Devcontainer) -> Result<(), Error> {
    let image = dc.image.as_deref().ok_or_else(|| {
        Error::BringUpFailed("no image or dockerComposeFile in devcontainer.json".into())
    })?;

    eprintln!("\x1b[36mdevcon:\x1b[0m starting container from {image}…");

    let workspace = dc.resolved_workspace_folder();
    let project_root = dc.project_root.to_string_lossy().into_owned();
    let codename = crate::codename::derive(&dc.project_root);
    let mount = format!("type=bind,source={project_root},target={workspace}");
    let local_folder_label = format!("devcontainer.local_folder={project_root}");

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        format!("devcon-{codename}"),
        "--label".into(),
        local_folder_label,
        "--mount".into(),
        mount,
        "-w".into(),
        workspace,
    ];
    // Stamp the run-once marker label at creation for containers we own.
    args.push("--label".into());
    args.push(format!("{}=1", docker::MARKER_LABEL));
    if let Some(user) = &dc.remote_user {
        args.push("-u".into());
        args.push(user.clone());
    }
    args.push(image.to_string());
    // Keep the container alive; the shell comes later via `docker exec`.
    args.push("sleep".into());
    args.push("infinity".into());

    let status = Command::new("docker")
        .args(&args)
        .status()
        .map_err(map_spawn_err)?;
    if !status.success() {
        return Err(Error::BringUpFailed("docker run failed".into()));
    }
    Ok(())
}

/// Run every `postCreateCommand`, then stamp the marker so it never runs again
/// for this container.
fn run_post_create(dc: &Devcontainer, container: &Container) -> Result<(), Error> {
    eprintln!("\x1b[36mdevcon:\x1b[0m running postCreateCommand…");
    run_commands(dc, container, "postCreateCommand", &dc.post_create)?;
    stamp_marker(container)?;
    Ok(())
}

/// Run `postStartCommand`, but only if it hasn't already run for the container's
/// *current start*. Keyed on `State.StartedAt`, so it runs once per start and
/// re-runs after a real restart — matching the devcontainer spec for
/// forever-running containers, where a connect is an *attach*, not a *start*.
fn run_post_start_if_needed(dc: &Devcontainer, container: &Container) -> Result<(), Error> {
    let started_at = docker::started_at(container);
    let sentinel = post_start_sentinel(started_at.as_deref());

    // Already ran for this start?
    if let Ok(status) =
        docker::exec_status(container, &["test", "-f", &sentinel]).map_err(Error::Docker)
    {
        if status.success() {
            return Ok(());
        }
    }

    eprintln!("\x1b[36mdevcon:\x1b[0m running postStartCommand…");
    run_commands(dc, container, "postStartCommand", &dc.post_start)?;

    // Stamp this start's sentinel.
    let touch = format!("touch {sentinel} 2>/dev/null || true");
    let _ = docker::exec_command(container, None, None, &["sh", "-c", &touch]);
    Ok(())
}

/// Filesystem-safe sentinel path for postStart, incorporating the container's
/// StartedAt so it's distinct per start (and absent after a restart).
fn post_start_sentinel(started_at: Option<&str>) -> String {
    // Keep only alnum from the timestamp → a stable, path-safe token.
    let token: String = started_at
        .unwrap_or("unknown")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("/tmp/.devcon-poststart-{token}")
}

/// Run a list of lifecycle commands as the (existence-checked) remoteUser, in
/// the workspace folder. Errors on the first non-zero exit.
fn run_commands(
    dc: &Devcontainer,
    container: &Container,
    hook: &'static str,
    commands: &[PostCreateCommand],
) -> Result<(), Error> {
    let workdir = dc.resolved_workspace_folder();
    // Only pass -u if the declared remoteUser actually exists in the container.
    let user = docker::resolve_user(container, dc.remote_user.as_deref());

    for cmd in commands {
        let argv: Vec<&str> = match cmd {
            PostCreateCommand::Shell(s) => vec!["sh", "-c", s],
            PostCreateCommand::Argv(v) => v.iter().map(String::as_str).collect(),
        };
        let status = docker::exec_command(container, user.as_deref(), Some(&workdir), &argv)
            .map_err(Error::Docker)?;
        if !status.success() {
            return Err(Error::LifecycleFailed(hook, describe(cmd)));
        }
    }
    Ok(())
}

/// Record that postCreate ran by adding a label to the container. Docker can't
/// relabel a running container in-place, so we use `docker container update`'s
/// unavailability gracefully: we write a sentinel *inside* the container as the
/// portable mechanism, and additionally try a filesystem marker the inspect
/// reads. Simpler + reliable: drop a marker file the next `has_marker` checks.
///
/// NOTE: Docker has no supported "add label to running container" command, so
/// the label is set at `docker run`/`compose` creation for image-based
/// containers we own. For containers we did *not* create (already-running
/// compose services), we fall back to an in-container sentinel file.
fn stamp_marker(container: &Container) -> Result<(), Error> {
    // In-container sentinel (works regardless of who created the container —
    // image-based ones we created also carry MARKER_LABEL, set at `docker run`).
    let touch = format!("touch {MARKER_SENTINEL} 2>/dev/null || true");
    let _ = docker::exec_command(container, None, None, &["sh", "-c", &touch]);
    Ok(())
}

fn describe(cmd: &PostCreateCommand) -> String {
    match cmd {
        PostCreateCommand::Shell(s) => s.clone(),
        PostCreateCommand::Argv(v) => v.join(" "),
    }
}

/// Report a lifecycle-hook failure without aborting. devcon still connects, so
/// a broken `postCreateCommand`/`postStartCommand` can't lock you out of the
/// container. The unstamped marker means the hook retries on the next connect.
fn warn_lifecycle(e: &Error) {
    eprintln!(
        "\x1b[33mdevcon:\x1b[0m {e} — connecting anyway; it will retry on the next \
         connect once the cause is fixed."
    );
}

fn map_spawn_err(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::Docker(docker::Error::DockerNotFound)
    } else {
        Error::Io(e)
    }
}

#[derive(Debug)]
pub enum Error {
    Docker(docker::Error),
    BringUpFailed(String),
    BringDownFailed(String),
    /// Nothing is running after a bring-up. The optional string is a specific
    /// diagnosis (e.g. the dev service exited immediately); `None` falls back to
    /// a generic message.
    NotUpAfterStart(Option<String>),
    /// (hook name, the command that failed)
    LifecycleFailed(&'static str, String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Docker(e) => write!(f, "{e}"),
            Error::BringUpFailed(msg) => write!(f, "failed to start the dev container: {msg}"),
            Error::BringDownFailed(msg) => write!(f, "failed to stop the dev container: {msg}"),
            Error::NotUpAfterStart(Some(detail)) => write!(f, "{detail}"),
            Error::NotUpAfterStart(None) => write!(
                f,
                "started the stack but no matching container is running — check `docker ps`"
            ),
            Error::LifecycleFailed(hook, cmd) => write!(f, "{hook} failed: {cmd}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_rfc3339_secs, post_start_sentinel};

    #[test]
    fn parses_docker_created_timestamp() {
        // 2026-07-12T09:14:22Z — cross-checked against a known epoch value.
        assert_eq!(
            parse_rfc3339_secs("2026-07-12T09:14:22.123456789Z"),
            Some(1_783_847_662)
        );
        // The Unix epoch itself.
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        // Fractional seconds and the trailing Z are optional.
        assert_eq!(parse_rfc3339_secs("2000-01-01T00:00:00"), Some(946_684_800));
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert_eq!(parse_rfc3339_secs("not-a-date"), None);
        assert_eq!(parse_rfc3339_secs("2026-13-01T00:00:00Z"), None); // bad month
        assert_eq!(parse_rfc3339_secs(""), None);
    }

    #[test]
    fn sentinel_is_path_safe_and_start_specific() {
        let a = post_start_sentinel(Some("2026-07-12T09:14:22.123456789Z"));
        let b = post_start_sentinel(Some("2026-07-12T10:00:00.000000000Z"));
        assert!(a.starts_with("/tmp/.devcon-poststart-"));
        // The StartedAt token is alnum-only — no ':' or '.' that break paths.
        let token = a.strip_prefix("/tmp/.devcon-poststart-").unwrap();
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
        // Different starts → different sentinels (so a restart re-runs postStart).
        assert_ne!(a, b);
    }

    #[test]
    fn sentinel_handles_missing_timestamp() {
        let s = post_start_sentinel(None);
        assert_eq!(s, "/tmp/.devcon-poststart-unknown");
    }
}
