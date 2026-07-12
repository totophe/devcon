//! The final step: replace this process with an interactive shell inside the
//! container. On Unix we `exec` so no wrapper process lingers around the shell
//! — `devcon` gets fully out of the way, mirroring how `tmosh` execs `tmux`.

use crate::docker::Container;
use std::process::Command;

/// Exec an interactive shell (`docker exec -it [-u user] -w workdir <c> <shell>`).
/// On success this never returns; on failure it returns the error.
pub fn shell(
    container: &Container,
    user: Option<&str>,
    workdir: &str,
    shell: &str,
) -> std::io::Error {
    let mut cmd = Command::new("docker");
    cmd.arg("exec").arg("-it");
    if let Some(u) = user {
        cmd.arg("-u").arg(u);
    }
    cmd.arg("-w").arg(workdir);
    cmd.arg(&container.id);
    cmd.arg(shell);
    exec(&mut cmd)
}

/// Replace the current process image with `cmd` via execvp(2). On success it
/// does not return; on failure the returned error explains why.
#[cfg(unix)]
fn exec(cmd: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    cmd.exec()
}

/// Non-Unix fallback: spawn, wait, and exit with the child's code.
#[cfg(not(unix))]
fn exec(cmd: &mut Command) -> std::io::Error {
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}
