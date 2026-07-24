# devcon — Implementation Plan

## Goal

`devcon` connects you to a project's dev container from a fully remote,
terminal-first workflow — no VS Code. It is the evolution of `dcon` for a world
where:

- the terminal multiplexer is owned at **login** (tmux, via `tmosh`), not
  per-project — so `devcon` must not start its own multiplexer (no
  tmux-in-tmux);
- nothing else brings the container up or runs its lifecycle scripts, because
  VS Code (which normally does) is not in the loop.

So `devcon`'s job is: **from a project root, guarantee the dev container is
fully alive (up + `postCreateCommand` run once), then `exec` me into a shell
inside it.**

## Mental model

```
tmux  (host — owned by tmosh at login)
  └── docker exec        ← devcon: ensure up + postCreate ran
        └── your shell   ← devcon execs you in (later: zellij)
```

## How it works

```
devcon                                  # at project root
  │
  ├─ 1. find .devcontainer  (walk up from cwd)
  ├─ 2. parse devcontainer.json (JSONC: strip comments + trailing commas)
  ├─ 3. resolve container by devcontainer.local_folder label (name heuristic fallback)
  │
  ├─ 4. container running?
  │        NO  → prompt "Start it? [Y/n]"   (Esc/n → back to shell)
  │               └─ bring up (compose up -d | docker run) → run postCreate → mark
  │        YES → (if unmarked) run postCreate → mark ; else skip
  │
  ├─ 5. resolve workspace dir:  inspect mount dest  >  ${…}-expanded json  >  /workspaces/<base>
  ├─ 6. resolve shell:  --shell  >  devcon.json  >  auto-detect  >  ask-once+persist
  │
  └─ 7. exec  docker exec -it -u <user> -w <workdir> <container> <shell>
```

## Design decisions

| Concern | Decision |
|---|---|
| Multiplexer | `devcon` owns none. tmux is `tmosh`'s (host); zellij-in-container is a parked `--workspace` mode that swaps only the step-7 exec. |
| Lifecycle scope | Ensure-up + run the declared `postCreateCommand`. No hook state machine — the wellmade images only declare `postCreateCommand`. |
| postCreate timing | **Once per container**, via a marker (label for containers we create + in-container sentinel for the general/compose case). |
| Shell resolution | Auto-detect first (silent on images shipping zsh); prompt only if ambiguous, then persist to `devcon.json`. |
| Workspace dir | Prefer the live container's mount destination (`docker inspect`); else expand `workspaceFolder` variables; else `/workspaces/<basename>`. A strict upgrade over `dcon`, which passes `workspaceFolder` through unexpanded. |
| Config file | `.devcontainer/devcon.json` (its own file; writable so first-run shell choice persists). |
| Distribution | Single static Rust binary, `curl \| sh` installer, `self-update`, CI + release cross-building linux x86_64/aarch64 + macos aarch64. |

## Module layout

| Module | Responsibility |
|---|---|
| `devcontainer` | Locate + parse `devcontainer.json`; JSONC stripping; `${…}` variable expansion; `postCreateCommand` normalization (string/array/object). |
| `codename` | Path → stable name (ported from `dcon`). |
| `docker` | All `docker` CLI interaction: `ps` discovery, host-wide project listing (`list_projects`, grouped by devcontainer/compose labels), `inspect` (mount dest, marker, created-at), `exec` (capture / status / interactive), `rm -f`. |
| `lifecycle` | Interactive "start the stack?"; compose/image bring-up; rebuild-on-drift (recreate when a stack file is newer than the container); run-once `postCreateCommand` + marker stamping. |
| `workspace` | Resolve the `-w` directory (inspect → expand → fallback). |
| `shell` | Resolve the shell (flag → config → detect → prompt+persist). |
| `config` | Load/merge/persist `.devcontainer/devcon.json` + global. |
| `connect` | Final `docker exec -it -w` via `execvp`. |
| `self_update` | Download + atomically replace the binary. |

## Run-once marker

Docker has no supported "add a label to a running container" command, so the
marker is two-pronged:

1. **Label** `dev.devcon.postcreate=1` — set at `docker run` for image-based
   containers `devcon` creates. Read cheaply via `docker inspect`.
2. **Sentinel** `/tmp/.devcon-postcreate-done` — `touch`ed inside the container
   after `postCreate`. Portable: works for compose services `devcon` did not
   create.

`has_marker` returns true if **either** signal is present. Because the wellmade
`postcreate.sh` is idempotent, a missed marker only costs a redundant (safe)
re-run.

## Rebuild on drift

Mirrors VS Code's "Rebuild Container". On connect, `lifecycle` compares the
mtime of the stack-defining files — `devcontainer.json`, every compose file,
and the referenced Dockerfile (`build.dockerfile` / `dockerFile`) — against the
container's `.Created` timestamp. If any is newer, the stack has drifted and
`devcon` prompts (default **No**) to recreate: `up -d --build --force-recreate`
for compose, `rm -f` + re-`run` for image-based. The fresh container has no
run-once markers, so `postCreate`/`postStart` re-run naturally. `--rebuild`
forces it, `--no-rebuild` suppresses it. Drift uses mtime, so fresh clones
(checkout-time mtimes) can false-positive — a content hash would be the robust
upgrade.

## Open questions / future work

- **Rebuild via content hash** (drift is mtime-based today): hash the stack
  files into a label/sentinel at build time so `git clone`/`checkout` mtimes
  don't trigger a spurious rebuild prompt.
- **`devcon down`** (not yet built): stop the current project's stack —
  `docker compose … down` for compose, `rm -f` (via `docker::remove`) for
  image-based. Symmetric with bring-up; the plumbing already exists.
- **`ls` for down projects**: `devcon ls` scans containers, so a fully
  `down` project (no container present) doesn't appear. Listing *startable*
  projects would need a workspace-dir scan for `.devcontainer/` or a registry
  devcon writes on connect.
- **zellij workspace mode** (parked): `--workspace` execs
  `zellij attach -c <codename>` instead of a bare shell. Machinery is already in
  place — only the step-7 command changes.
- **Daemon-unreachable vs. no-container**: currently both surface as a docker
  error; could special-case "socket missing" for a friendlier hint.
- **Compose working directory**: we run `docker compose` from
  `.devcontainer/`; projects that reference compose files relative to the repo
  root may need a smarter base dir. Revisit if a real project needs it.
- **Non-`sleep infinity` images**: image-based bring-up keeps the container
  alive with `sleep infinity`. Images with their own long-running entrypoint
  don't need this; harmless but worth revisiting.
