//! `devcon self uninstall` — take devcon back off this machine.
//!
//! Deliberately narrow. Unlike `spot` and `tmosh`, devcon is not a login hook:
//! it never edits a shell rc and keeps no cache, so there is exactly one thing
//! of its own to remove — the binary.
//!
//! It also leaves your projects entirely alone. `.devcontainer/devcon.json`
//! files are project configuration that belong to the repo, not to the
//! installation, and any running containers belong to Docker.

use std::fs;

pub fn run(dry_run: bool) -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("devcon: cannot locate my own binary; remove it by hand");
        return 1;
    };

    if dry_run {
        println!("would remove {}", exe.display());
        println!("\nDry run: nothing was changed. Re-run without --dry-run to apply.");
        return 0;
    }

    // Unlinking a running executable is fine on unix — the inode outlives the
    // name. A package-managed install will fail here, which is correct: the
    // package manager owns that file.
    if let Err(e) = fs::remove_file(&exe) {
        eprintln!(
            "devcon: could not remove {}: {e}\n\
             \x20       If devcon came from a package, uninstall it with your \
             package manager.",
            exe.display()
        );
        return 1;
    }

    println!("remove {}", exe.display());
    println!("\nUninstalled.");
    println!(
        "Your containers and any .devcontainer/devcon.json files are untouched — \n\
         those belong to your projects and to Docker, not to devcon."
    );
    0
}
