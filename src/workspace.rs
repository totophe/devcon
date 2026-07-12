//! Resolve the container-side workspace directory used as `docker exec -w`.
//!
//! Precedence (most authoritative first):
//!   1. the live container's actual mount destination (`docker inspect`)
//!   2. `workspaceFolder` from devcontainer.json, with `${...}` expanded
//!   3. `/workspaces/<project-basename>` convention
//!
//! `dcon` passes `workspaceFolder` through verbatim and so emits a literal
//! `${localWorkspaceFolderBasename}` on these images; this resolver fixes that.

use crate::devcontainer::Devcontainer;
use crate::docker::{self, Container};

/// Resolve the workspace directory. Prefers the running container's mount
/// destination; otherwise falls back to the expanded json / convention.
pub fn resolve(dc: &Devcontainer, container: &Container) -> String {
    if let Some(dest) = docker::workspace_mount_destination(container, &dc.project_root) {
        return dest;
    }
    dc.resolved_workspace_folder()
}
