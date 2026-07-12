//! Dev-container lifecycle: bring the stack up when it's down, then run the
//! declared `postCreateCommand` exactly once per container (tracked with a
//! label). This is the piece VS Code normally owns; `devcon` reproduces just
//! the subset these images need.

use crate::devcontainer::{Devcontainer, PostCreateCommand};
use crate::docker::{self, Container, MARKER_SENTINEL};
use std::io::{self, IsTerminal, Write};
use std::process::Command;

/// Bring the stack up if needed and ensure lifecycle hooks have run.
///
/// Runs, in order:
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
) -> Result<Option<Container>, Error> {
    let container = match existing {
        Some(c) => c,
        None => {
            // Stack is down — ask before doing anything heavyweight.
            if !assume_yes && !confirm_start(dc)? {
                return Ok(None);
            }
            bring_up(dc)?;
            // Re-discover the container now that it's running.
            docker::find(dc)
                .map_err(Error::Docker)?
                .ok_or(Error::NotUpAfterStart)?
        }
    };

    // postCreate: once per creation.
    if !dc.post_create.is_empty() && !docker::has_marker(&container) {
        run_post_create(dc, &container)?;
    }

    // postStart: once per start (keyed on StartedAt).
    if !dc.post_start.is_empty() {
        run_post_start_if_needed(dc, &container)?;
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
    NotUpAfterStart,
    /// (hook name, the command that failed)
    LifecycleFailed(&'static str, String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Docker(e) => write!(f, "{e}"),
            Error::BringUpFailed(msg) => write!(f, "failed to start the dev container: {msg}"),
            Error::NotUpAfterStart => write!(
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
    use super::post_start_sentinel;

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
