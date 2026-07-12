//! Dev-container lifecycle: bring the stack up when it's down, then run the
//! declared `postCreateCommand` exactly once per container (tracked with a
//! label). This is the piece VS Code normally owns; `devcon` reproduces just
//! the subset these images need.

use crate::devcontainer::{Devcontainer, PostCreateCommand};
use crate::docker::{self, Container, MARKER_SENTINEL};
use std::io::{self, IsTerminal, Write};
use std::process::Command;

/// Bring the stack up if needed and ensure `postCreate` has run.
///
/// - If a container is already running: return it (running `postCreate` only
///   if the marker is absent).
/// - If not: prompt the user (unless `assume_yes`), bring it up, run
///   `postCreate`, stamp the marker, and return the now-running container.
///
/// Returns `Ok(None)` if the stack is down and the user declined to start it.
pub fn ensure_up(
    dc: &Devcontainer,
    existing: Option<Container>,
    assume_yes: bool,
) -> Result<Option<Container>, Error> {
    if let Some(container) = existing {
        // Already up. Run postCreate once if it hasn't been marked.
        if !docker::has_marker(&container) && !dc.post_create.is_empty() {
            run_post_create(dc, &container)?;
        }
        return Ok(Some(container));
    }

    // Stack is down — ask before doing anything heavyweight.
    if !assume_yes && !confirm_start(dc)? {
        return Ok(None);
    }

    bring_up(dc)?;

    // Re-discover the container now that it's running.
    let container = docker::find(&dc.project_root)
        .map_err(Error::Docker)?
        .ok_or(Error::NotUpAfterStart)?;

    if !dc.post_create.is_empty() {
        run_post_create(dc, &container)?;
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

/// Run every `postCreateCommand`, then stamp the marker label so we never run
/// it again for this container.
fn run_post_create(dc: &Devcontainer, container: &Container) -> Result<(), Error> {
    eprintln!("\x1b[36mdevcon:\x1b[0m running postCreateCommand…");
    let workdir = dc.resolved_workspace_folder();
    let user = dc.remote_user.as_deref();

    for cmd in &dc.post_create {
        let argv: Vec<&str> = match cmd {
            PostCreateCommand::Shell(s) => vec!["sh", "-c", s],
            PostCreateCommand::Argv(v) => v.iter().map(String::as_str).collect(),
        };
        let status =
            docker::exec_command(container, user, Some(&workdir), &argv).map_err(Error::Docker)?;
        if !status.success() {
            return Err(Error::PostCreateFailed(describe(cmd)));
        }
    }

    stamp_marker(container)?;
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
    PostCreateFailed(String),
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
            Error::PostCreateFailed(cmd) => write!(f, "postCreateCommand failed: {cmd}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}
