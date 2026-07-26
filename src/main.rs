//! devcon — bring up a project's dev container and drop into its shell,
//! without VS Code.
//!
//! Mental model: you're already inside a login multiplexer (tmux, via tmosh).
//! `devcon` doesn't touch it — it just guarantees the dev container is alive
//! (up + postCreate run once) and execs you into a shell inside it:
//!
//!     tmux (host, tmosh)  →  docker exec  →  your shell (later: zellij)

mod codename;
mod config;
mod connect;
mod devcontainer;
mod docker;
mod lifecycle;
mod self_update;
mod shell;
mod uninstall;
mod workspace;

use clap::{Parser, Subcommand};
use devcontainer::Devcontainer;

#[derive(Parser, Debug)]
#[command(
    name = "devcon",
    version,
    about = "Bring up a project's dev container and drop into its shell (VS Code-free)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Shell to use inside the container (overrides config and auto-detection)
    #[arg(short = 's', long = "shell", value_name = "SHELL")]
    shell: Option<String>,

    /// Start the stack without asking if it isn't running
    #[arg(short = 'y', long = "yes")]
    yes: bool,

    /// Recreate the container even if nothing changed (like VS Code's "Rebuild
    /// Container"). Rebuilds the image and re-runs postCreate.
    #[arg(long = "rebuild", conflicts_with = "no_rebuild")]
    rebuild: bool,

    /// Never recreate on drift — connect to the running container as-is,
    /// suppressing the "stack changed, rebuild?" prompt.
    #[arg(long = "no-rebuild")]
    no_rebuild: bool,

    /// Pull the latest container image(s) before starting, then recreate so the
    /// new image takes effect. Implies --rebuild. (docker run only reuses a
    /// cached image otherwise; compose up/--build won't re-pull on its own.)
    #[arg(long = "pull", conflicts_with = "no_rebuild")]
    pull: bool,

    /// Print version (a lowercase alias for the default -V/--version)
    #[arg(short = 'v', action = clap::ArgAction::Version)]
    version_alias: Option<bool>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List dev-container projects present on this host (running or stopped)
    #[command(name = "ls", visible_alias = "ps")]
    Ls {
        /// Include every compose project on the host, not just dev containers
        #[arg(short = 'a', long = "all")]
        all: bool,
    },
    /// Stop the current project's stack (compose down / remove the container)
    #[command(name = "down")]
    Down {
        /// Keep the container(s), just stop them (compose stop / docker stop),
        /// so the next connect reuses them instead of recreating
        #[arg(short = 's', long = "stop")]
        stop: bool,
    },
    /// Manage the devcon installation itself
    #[command(name = "self", subcommand)]
    Selfcmd(SelfAction),
    /// Deprecated: use `devcon self update`
    #[command(name = "self-update", hide = true)]
    SelfUpdate,
}

/// devcon's own lifecycle, grouped rustup-style to match `spot` and `tmosh`:
/// updating and uninstalling are actions, not modifiers on the primary one.
#[derive(Subcommand, Debug)]
enum SelfAction {
    /// Check for and install the latest release
    Update,
    /// Remove the devcon binary from this machine
    Uninstall {
        /// Show what would be removed without changing anything
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Selfcmd(action)) => {
            match action {
                SelfAction::Update => self_update::run().unwrap_or_else(|e| fail(&e.to_string())),
                SelfAction::Uninstall { dry_run } => {
                    std::process::exit(uninstall::run(dry_run));
                }
            }
            return;
        }
        // Kept as a hidden alias: shipped since v0.2.0, so scripts and muscle
        // memory in the wild still use it.
        Some(Commands::SelfUpdate) => {
            eprintln!("devcon: `self-update` is deprecated; use `devcon self update`");
            self_update::run().unwrap_or_else(|e| fail(&e.to_string()));
            return;
        }
        Some(Commands::Ls { all }) => {
            list_projects(all);
            return;
        }
        Some(Commands::Down { stop }) => {
            down_stack(stop);
            return;
        }
        None => {}
    }

    let cwd = std::env::current_dir()
        .unwrap_or_else(|e| fail(&format!("cannot determine current directory: {e}")));

    // 1. Locate the project and parse its devcontainer.json.
    let project_root = devcontainer::find_project_root(&cwd).unwrap_or_else(|| {
        eprintln!("error: no .devcontainer folder found in {cwd:?} or any parent directory");
        eprintln!("hint: run devcon from inside a project that has a .devcontainer folder");
        std::process::exit(1);
    });

    let dc = Devcontainer::load(&project_root).unwrap_or_else(|e| fail(&e.to_string()));

    // 2. Is the container already running?
    let existing = docker::find(&dc).unwrap_or_else(|e| fail(&e.to_string()));

    // 3. Ensure it's up (prompting if needed), rebuilt if the stack drifted,
    //    and postCreate has run once.
    // --pull forces a recreate: a freshly pulled image only takes effect on a
    // new container, so a bare pull with no rebuild would be a no-op.
    let rebuild = if cli.rebuild || cli.pull {
        lifecycle::Rebuild::Force
    } else if cli.no_rebuild {
        lifecycle::Rebuild::Never
    } else {
        lifecycle::Rebuild::Auto
    };
    let container = match lifecycle::ensure_up(&dc, existing, cli.yes, rebuild, cli.pull) {
        Ok(Some(c)) => c,
        Ok(None) => {
            // User declined to start the stack — bow out to the shell quietly.
            eprintln!("devcon: not started. Nothing to connect to.");
            std::process::exit(0);
        }
        Err(e) => fail(&e.to_string()),
    };

    // 4. Resolve the workspace dir (inspect > expanded json > convention).
    let workdir = workspace::resolve(&dc, &container);

    // 5. Resolve the shell (flag > config > detect > prompt+persist).
    let cfg = config::Config::load(&project_root);
    let shell = shell::resolve(
        cli.shell.as_deref(),
        cfg.shell.as_deref(),
        &container,
        &project_root,
    );

    // 6. Exec into the container. On success this never returns.
    // Only pass -u if the declared remoteUser exists in this container.
    let user = docker::resolve_user(&container, dc.remote_user.as_deref());
    eprintln!(
        "\x1b[36mdevcon:\x1b[0m connecting to {} ({workdir}) …",
        container.name
    );
    let err = connect::shell(&container, user.as_deref(), &workdir, &shell);
    fail(&format!("failed to exec shell in container: {err}"));
}

/// Render `devcon ls`: the dev-container projects present on this host.
fn list_projects(all: bool) {
    let projects = docker::list_projects(all).unwrap_or_else(|e| fail(&e.to_string()));

    if projects.is_empty() {
        if all {
            eprintln!("devcon: no containers found on this host.");
        } else {
            eprintln!(
                "devcon: no dev-container projects found (pass --all to include \
                 every compose project)."
            );
        }
        return;
    }

    // Column-align on the project name (leave room for the header, too).
    let name_w = projects
        .iter()
        .map(|p| p.name.len())
        .chain(std::iter::once("PROJECT".len()))
        .max()
        .unwrap_or(0);

    // Header row (bold), matching the data layout: STATUS(7) PROJECT KIND(9) PATH.
    println!(
        "\x1b[1m{status:<7}  {name:<name_w$}  {kind:<9}  PATH\x1b[0m",
        status = "STATUS",
        name = "PROJECT",
        kind = "KIND",
    );

    for p in &projects {
        let status = if p.running {
            "\x1b[32mup\x1b[0m     " // green, padded to "stopped"'s width
        } else {
            "\x1b[90mstopped\x1b[0m"
        };
        let managed = if p.devcon_managed { " *" } else { "" };
        let count = if p.container_count > 1 {
            format!("  ({} containers)", p.container_count)
        } else {
            String::new()
        };
        // Unknown path (no location label) shows a dim dash rather than a blank.
        let path = if p.path.is_empty() {
            "\x1b[90m—\x1b[0m".to_string()
        } else {
            p.path.clone()
        };
        println!(
            "{status}  {name:<name_w$}  {kind:<9}  {path}{managed}{count}",
            name = p.name,
            kind = p.kind,
        );
    }
    if projects.iter().any(|p| p.devcon_managed) {
        eprintln!("\n\x1b[90m* created by devcon\x1b[0m");
    }
}

/// Handle `devcon down [--stop]`: locate the current project and tear its
/// stack down (remove by default, or just stop with `--stop`).
fn down_stack(stop: bool) {
    let cwd = std::env::current_dir()
        .unwrap_or_else(|e| fail(&format!("cannot determine current directory: {e}")));
    let project_root = devcontainer::find_project_root(&cwd).unwrap_or_else(|| {
        eprintln!("error: no .devcontainer folder found in {cwd:?} or any parent directory");
        std::process::exit(1);
    });
    let dc = Devcontainer::load(&project_root).unwrap_or_else(|e| fail(&e.to_string()));
    let existing = docker::find(&dc).unwrap_or_else(|e| fail(&e.to_string()));

    let mode = if stop {
        lifecycle::TearDown::Stop
    } else {
        lifecycle::TearDown::Remove
    };
    lifecycle::bring_down(&dc, existing.as_ref(), mode).unwrap_or_else(|e| fail(&e.to_string()));
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}
