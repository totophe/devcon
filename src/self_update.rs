//! omz-style self-update: download the matching release asset for this host
//! and atomically replace the running binary. Ported from `dcon`.

use std::fs;
use std::process::Command;

const REPO: &str = "totophe/devcon";
const TOOL: &str = "devcon";

pub fn run() -> Result<(), Error> {
    let target = detect_target()?;
    let binary_name = format!("{TOOL}-{target}");
    let url = format!("https://github.com/{REPO}/releases/download/latest/{binary_name}");

    let current_exe = std::env::current_exe().map_err(Error::Io)?;

    eprintln!("Downloading latest {binary_name} …");

    let tmp_path = current_exe.with_extension("tmp");
    download(&url, &tmp_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755)).map_err(Error::Io)?;
    }

    fs::rename(&tmp_path, &current_exe).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        Error::Replace(e)
    })?;

    eprintln!("Updated {}", current_exe.display());
    Ok(())
}

fn detect_target() -> Result<&'static str, Error> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(Error::UnsupportedPlatform(os, arch)),
    }
}

fn download(url: &str, dest: &std::path::Path) -> Result<(), Error> {
    if let Ok(output) = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(dest)
        .output()
    {
        if output.status.success() {
            return Ok(());
        }
    }
    if let Ok(output) = Command::new("wget")
        .args(["-qO"])
        .arg(dest)
        .arg(url)
        .output()
    {
        if output.status.success() {
            return Ok(());
        }
    }
    Err(Error::Download(url.to_string()))
}

#[derive(Debug)]
pub enum Error {
    UnsupportedPlatform(&'static str, &'static str),
    Download(String),
    Replace(std::io::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnsupportedPlatform(os, arch) => write!(f, "unsupported platform: {os}/{arch}"),
            Error::Download(url) => write!(
                f,
                "failed to download {url} — check your network and that curl or wget is installed"
            ),
            Error::Replace(e) => write!(f, "failed to replace binary: {e}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}
