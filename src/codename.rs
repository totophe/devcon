//! Derive a stable, human-readable name from a project root path. Used as the
//! session/workspace identifier so the same project always maps to the same
//! name across invocations. Ported from `dcon` — same rules.

use std::path::Path;

const WORKSPACE_DIRS: &[&str] = &["workspace", "workspaces"];

/// Derive a codename from a project root path.
///
/// Rules:
///   - If the project sits two levels inside a workspace folder
///     (`<anywhere>/workspaces/<scope>/<project>`), the name is `<scope>_<project>`.
///   - Otherwise, just the folder name is used.
///
/// Examples:
///   /home/user/workspaces/totophe/devcon  →  totophe_devcon
///   /home/user/workspaces/myproject       →  myproject
///   /home/user/projects/myapp             →  myapp
pub fn derive(project_root: &Path) -> String {
    let project = folder_name(project_root);

    if let Some(parent) = project_root.parent() {
        if let Some(grandparent) = parent.parent() {
            if is_workspace_dir(grandparent) {
                let scope = folder_name(parent);
                return format!("{scope}_{project}");
            }
        }
        if is_workspace_dir(parent) {
            // project is a direct child of workspaces/ — no scope prefix
            return project.to_string();
        }
    }

    project.to_string()
}

fn folder_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
}

fn is_workspace_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| WORKSPACE_DIRS.contains(&n))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn scoped_inside_workspaces() {
        assert_eq!(
            derive(&p("/home/totophe/workspaces/totophe/devcon")),
            "totophe_devcon"
        );
    }

    #[test]
    fn scoped_inside_workspace_singular() {
        assert_eq!(
            derive(&p("/home/totophe/workspace/acme/myapp")),
            "acme_myapp"
        );
    }

    #[test]
    fn direct_child_of_workspaces_no_scope() {
        assert_eq!(
            derive(&p("/home/totophe/workspaces/myproject")),
            "myproject"
        );
    }

    #[test]
    fn unrelated_path_uses_folder_name() {
        assert_eq!(derive(&p("/home/totophe/projects/myapp")), "myapp");
    }

    #[test]
    fn shallow_path() {
        assert_eq!(derive(&p("/myapp")), "myapp");
    }
}
