//! Locate the project's `.devcontainer` and parse the bits of
//! `devcontainer.json` that `devcon` needs: how to bring the stack up
//! (`image` vs `dockerComposeFile`), where the workspace lives inside the
//! container (`workspaceFolder`), the user to exec as, and the lifecycle
//! command to run once (`postCreateCommand`).
//!
//! `devcontainer.json` is JSONC (comments + trailing commas) in the wild, so
//! we strip comments before handing it to `serde_json`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Where the parsed `devcontainer.json` lives and what it declares.
#[derive(Debug, Clone)]
pub struct Devcontainer {
    /// The project root (the directory *containing* `.devcontainer`).
    pub project_root: PathBuf,
    /// `image` field, if the container is image-based.
    pub image: Option<String>,
    /// `dockerComposeFile` entries, if the container is compose-based.
    pub compose_files: Vec<String>,
    /// `service` field (compose-based: which service is the dev container).
    pub service: Option<String>,
    /// `runServices` (compose-based: services to start alongside `service`).
    pub run_services: Vec<String>,
    /// `workspaceFolder`, raw (may still contain `${...}` variables).
    pub workspace_folder: Option<String>,
    /// `remoteUser` — who a shell/exec should run as.
    pub remote_user: Option<String>,
    /// `postCreateCommand`, normalized to a shell-runnable list of commands.
    pub post_create: Vec<PostCreateCommand>,
    /// `name`, purely informational.
    pub name: Option<String>,
}

/// A single `postCreateCommand`. The devcontainer spec allows a string
/// (run via the shell), an array (argv, no shell), or an object of named
/// commands (run in parallel; we run them sequentially for simplicity).
#[derive(Debug, Clone)]
pub enum PostCreateCommand {
    /// Run through `sh -c`.
    Shell(String),
    /// Run as an argv vector, no shell interpolation.
    Argv(Vec<String>),
}

/// Walk upward from `start` until we find a directory containing `.devcontainer`.
/// Returns the path of that directory (the project root), or `None` if not found.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".devcontainer").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Locate `.devcontainer/devcontainer.json` under `project_root`.
///
/// Supports both the flat layout (`.devcontainer/devcontainer.json`) and the
/// nested layout (`.devcontainer/<name>/devcontainer.json`) — VS Code allows
/// either, and picks the first it finds.
fn config_path(project_root: &Path) -> Option<PathBuf> {
    let dc = project_root.join(".devcontainer");
    let flat = dc.join("devcontainer.json");
    if flat.is_file() {
        return Some(flat);
    }
    // Nested: .devcontainer/<subdir>/devcontainer.json
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dc)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| p.join("devcontainer.json"))
        .filter(|p| p.is_file())
        .collect();
    // Deterministic pick.
    entries.sort();
    entries.into_iter().next()
}

impl Devcontainer {
    /// Load and parse the devcontainer config for a project root.
    /// Returns `Err` if the file is missing or unparseable.
    pub fn load(project_root: &Path) -> Result<Self, Error> {
        let path = config_path(project_root).ok_or(Error::NotFound)?;
        let raw = std::fs::read_to_string(&path).map_err(Error::Io)?;
        let stripped = strip_jsonc(&raw);
        let parsed: Raw = serde_json::from_str(&stripped)
            .map_err(|e| Error::Parse(path.display().to_string(), e.to_string()))?;

        Ok(Self {
            project_root: project_root.to_path_buf(),
            image: parsed.image,
            compose_files: parsed.docker_compose_file.into_vec(),
            service: parsed.service,
            run_services: parsed.run_services,
            workspace_folder: parsed.workspace_folder,
            remote_user: parsed.remote_user,
            post_create: parsed.post_create_command.into_commands(),
            name: parsed.name,
        })
    }

    /// True when the container is defined by a docker-compose file.
    pub fn is_compose(&self) -> bool {
        !self.compose_files.is_empty()
    }

    /// Resolve `workspaceFolder` with devcontainer variables expanded.
    /// Falls back to `/workspaces/<project-basename>` when unset.
    pub fn resolved_workspace_folder(&self) -> String {
        match &self.workspace_folder {
            Some(raw) => expand_variables(raw, &self.project_root),
            None => default_workspace_folder(&self.project_root),
        }
    }
}

/// The conventional container-side workspace path for a project.
pub fn default_workspace_folder(project_root: &Path) -> String {
    let base = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    format!("/workspaces/{base}")
}

/// Expand the subset of devcontainer variables that affect the paths we use.
/// See https://containers.dev/implementors/json_reference/#variables
fn expand_variables(input: &str, project_root: &Path) -> String {
    let local_folder = project_root.to_string_lossy();
    let basename = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // `containerWorkspaceFolder(Basename)` reference the resolved workspace
    // folder itself; without a running container we approximate with the
    // convention, which is what these images use anyway.
    let container_folder = default_workspace_folder(project_root);
    let container_basename = container_folder
        .rsplit('/')
        .next()
        .unwrap_or(basename)
        .to_string();

    input
        .replace("${localWorkspaceFolderBasename}", basename)
        .replace("${localWorkspaceFolder}", &local_folder)
        .replace("${containerWorkspaceFolderBasename}", &container_basename)
        .replace("${containerWorkspaceFolder}", &container_folder)
}

/// The raw JSON shape (only the fields we care about).
#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default, rename = "dockerComposeFile")]
    docker_compose_file: StringOrVec,
    #[serde(default)]
    service: Option<String>,
    #[serde(default, rename = "runServices")]
    run_services: Vec<String>,
    #[serde(default, rename = "workspaceFolder")]
    workspace_folder: Option<String>,
    #[serde(default, rename = "remoteUser")]
    remote_user: Option<String>,
    #[serde(default, rename = "postCreateCommand")]
    post_create_command: CommandField,
}

/// `dockerComposeFile` is a string or an array of strings.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum StringOrVec {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrVec::None => Vec::new(),
            StringOrVec::One(s) => vec![s],
            StringOrVec::Many(v) => v,
        }
    }
}

/// A lifecycle command field: string | array | object-of-commands | absent.
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum CommandField {
    #[default]
    None,
    Shell(String),
    Argv(Vec<String>),
    Named(std::collections::BTreeMap<String, CommandValue>),
}

/// A single value inside the object form of a lifecycle command.
#[derive(Deserialize)]
#[serde(untagged)]
enum CommandValue {
    Shell(String),
    Argv(Vec<String>),
}

impl CommandField {
    fn into_commands(self) -> Vec<PostCreateCommand> {
        match self {
            CommandField::None => Vec::new(),
            CommandField::Shell(s) => vec![PostCreateCommand::Shell(s)],
            CommandField::Argv(v) => vec![PostCreateCommand::Argv(v)],
            // BTreeMap → deterministic order.
            CommandField::Named(map) => map
                .into_values()
                .map(|v| match v {
                    CommandValue::Shell(s) => PostCreateCommand::Shell(s),
                    CommandValue::Argv(a) => PostCreateCommand::Argv(a),
                })
                .collect(),
        }
    }
}

/// Strip `//` line comments and `/* */` block comments from JSONC, preserving
/// string literals. Trailing commas are left for `serde_json`, which tolerates
/// them under its default config only for arrays/objects — so we also drop
/// commas that immediately precede a closing `]` or `}`.
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    // line comment: skip to end of line
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    // block comment: skip to */
                    chars.next(); // consume '*'
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if prev == '*' && c == '/' {
                            break;
                        }
                        prev = c;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }

    strip_trailing_commas(&out)
}

/// Remove commas that directly precede a `]` or `}` (ignoring whitespace),
/// so trailing-comma JSONC parses cleanly.
fn strip_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = input.chars().collect();

    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            // Look ahead past whitespace for a closing bracket.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == ']' || bytes[j] == '}') {
                // drop this comma
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[derive(Debug)]
pub enum Error {
    NotFound,
    Parse(String, String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no devcontainer.json found under .devcontainer/"),
            Error::Parse(path, msg) => write!(f, "failed to parse {path}: {msg}"),
            Error::Io(e) => write!(f, "i/o error reading devcontainer.json: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_dc(dir: &Path, contents: &str) {
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcontainer.json"), contents).unwrap();
    }

    #[test]
    fn finds_devcontainer_in_cwd() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".devcontainer")).unwrap();
        assert_eq!(
            find_project_root(tmp.path()),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn finds_devcontainer_in_parent() {
        let tmp = TempDir::new().unwrap();
        let child = tmp.path().join("subdir").join("nested");
        fs::create_dir_all(tmp.path().join(".devcontainer")).unwrap();
        fs::create_dir_all(&child).unwrap();
        assert_eq!(find_project_root(&child), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn returns_none_when_no_devcontainer() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(find_project_root(tmp.path()), None);
    }

    #[test]
    fn parses_image_based() {
        let tmp = TempDir::new().unwrap();
        write_dc(
            tmp.path(),
            r#"{
                "image": "ghcr.io/example/img:latest",
                "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}",
                "remoteUser": "wm",
                "postCreateCommand": "/opt/wellmade/bin/postcreate.sh"
            }"#,
        );
        let dc = Devcontainer::load(tmp.path()).unwrap();
        assert_eq!(dc.image.as_deref(), Some("ghcr.io/example/img:latest"));
        assert!(!dc.is_compose());
        assert_eq!(dc.remote_user.as_deref(), Some("wm"));
        assert_eq!(dc.post_create.len(), 1);
    }

    #[test]
    fn parses_compose_based_string_and_array() {
        let tmp = TempDir::new().unwrap();
        write_dc(
            tmp.path(),
            r#"{
                "dockerComposeFile": "docker-compose.yml",
                "service": "app",
                "runServices": ["app", "db"],
                "workspaceFolder": "/workspaces/proj"
            }"#,
        );
        let dc = Devcontainer::load(tmp.path()).unwrap();
        assert!(dc.is_compose());
        assert_eq!(dc.compose_files, vec!["docker-compose.yml".to_string()]);
        assert_eq!(dc.service.as_deref(), Some("app"));
        assert_eq!(dc.run_services, vec!["app", "db"]);
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let tmp = TempDir::new().unwrap();
        write_dc(
            tmp.path(),
            r#"{
                // a line comment
                "image": "x", /* block */
                "remoteUser": "wm", // trailing after value
            }"#,
        );
        let dc = Devcontainer::load(tmp.path()).unwrap();
        assert_eq!(dc.image.as_deref(), Some("x"));
        assert_eq!(dc.remote_user.as_deref(), Some("wm"));
    }

    #[test]
    fn does_not_strip_slashes_inside_strings() {
        let tmp = TempDir::new().unwrap();
        write_dc(
            tmp.path(),
            r#"{ "postCreateCommand": "echo http://example.com // not a comment" }"#,
        );
        let dc = Devcontainer::load(tmp.path()).unwrap();
        match &dc.post_create[0] {
            PostCreateCommand::Shell(s) => {
                assert_eq!(s, "echo http://example.com // not a comment")
            }
            _ => panic!("expected shell command"),
        }
    }

    #[test]
    fn expands_workspace_variables() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("myproject");
        fs::create_dir_all(&proj).unwrap();
        write_dc(
            &proj,
            r#"{ "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}" }"#,
        );
        let dc = Devcontainer::load(&proj).unwrap();
        assert_eq!(dc.resolved_workspace_folder(), "/workspaces/myproject");
    }

    #[test]
    fn workspace_folder_defaults_to_convention() {
        let tmp = TempDir::new().unwrap();
        let proj = tmp.path().join("someapp");
        fs::create_dir_all(&proj).unwrap();
        write_dc(&proj, r#"{ "image": "x" }"#);
        let dc = Devcontainer::load(&proj).unwrap();
        assert_eq!(dc.resolved_workspace_folder(), "/workspaces/someapp");
    }

    #[test]
    fn parses_object_form_post_create() {
        let tmp = TempDir::new().unwrap();
        write_dc(
            tmp.path(),
            r#"{
                "image": "x",
                "postCreateCommand": {
                    "install": "npm ci",
                    "setup": ["./setup.sh", "--fast"]
                }
            }"#,
        );
        let dc = Devcontainer::load(tmp.path()).unwrap();
        assert_eq!(dc.post_create.len(), 2);
    }

    #[test]
    fn nested_devcontainer_layout() {
        let tmp = TempDir::new().unwrap();
        let dc = tmp.path().join(".devcontainer").join("web");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcontainer.json"), r#"{ "image": "nested" }"#).unwrap();
        let parsed = Devcontainer::load(tmp.path()).unwrap();
        assert_eq!(parsed.image.as_deref(), Some("nested"));
    }
}
