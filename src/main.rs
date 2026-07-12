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
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Update devcon to the latest version
    #[command(name = "self-update")]
    SelfUpdate,
}

fn main() {
    let cli = Cli::parse();

    if let Some(Commands::SelfUpdate) = cli.command {
        self_update::run().unwrap_or_else(|e| fail(&e.to_string()));
        return;
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
    let existing = docker::find(&project_root).unwrap_or_else(|e| fail(&e.to_string()));

    // 3. Ensure it's up (prompting if needed) and postCreate has run once.
    let container = match lifecycle::ensure_up(&dc, existing, cli.yes) {
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
    let user = dc.remote_user.as_deref();
    eprintln!(
        "\x1b[36mdevcon:\x1b[0m connecting to {} ({workdir}) …",
        container.name
    );
    let err = connect::shell(&container, user, &workdir, &shell);
    fail(&format!("failed to exec shell in container: {err}"));
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}
