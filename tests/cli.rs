//! CLI smoke tests — verify argument handling and the no-devcontainer error
//! path without needing a real Docker daemon.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn prints_help() {
    Command::cargo_bin("devcon")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("dev container"));
}

#[test]
fn errors_without_devcontainer() {
    // An empty temp dir has no .devcontainer anywhere up its (temp) tree.
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("devcon")
        .unwrap()
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("no .devcontainer folder found"));
}

#[test]
fn rejects_unknown_flag() {
    Command::cargo_bin("devcon")
        .unwrap()
        .arg("--definitely-not-a-flag")
        .assert()
        .failure();
}
