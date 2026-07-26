# devcon — Dev Container Connect (VS Code-free)

Bring up a project's dev container and drop into its shell — from the command
line, without VS Code. `devcon` is the CLI counterpart to opening a project in
an IDE: it starts the stack if it's down, runs the one-time `postCreateCommand`
that VS Code would normally run, then `exec`s you into a shell inside the
container.

It is the evolution of [`dcon`](https://github.com/totophe/remote-code-toolbox)
for a fully remote, terminal-first workflow.

## Quick start

```sh
# Install (downloads the right binary for your OS/arch into ~/.local/bin):
curl -fsSL https://raw.githubusercontent.com/totophe/devcon/main/install.sh | sh

# Then, from any project with a .devcontainer folder:
cd ~/your/project
devcon
```

`devcon` brings the stack up (asking first if it's down), runs the one-time
`postCreateCommand`, and drops you into a shell inside the container.

## Mental model

You log into a remote host over SSH/[mosh](https://mosh.org/) and land in a
tmux session (e.g. via [tmosh](https://github.com/totophe/tmosh)). From there:

```
tmux  (host — owned by tmosh at login)
  └── docker exec        ← devcon: ensure the container is up + postCreate ran
        └── your shell   ← devcon execs you in (later: zellij workspace)
```

`devcon` deliberately **does not touch the multiplexer**. It never starts its
own tmux session, so there's no tmux-in-tmux. It just guarantees the container
is alive and gets you inside — then gets out of the way (`exec`, no lingering
wrapper).

## What it does

Run `devcon` at a project root:

1. Finds `.devcontainer/` by walking up from the current directory.
2. Parses `devcontainer.json` (JSONC — comments and trailing commas welcome).
3. Finds the running container (via the `devcontainer.local_folder` label).
4. **If the stack is down**, asks `Start it? [Y/n]`, then brings it up
   (`docker compose up -d` for compose stacks, `docker run` for image-based)
   and runs the declared `postCreateCommand` **once**.
5. **If the stack is up but a stack file changed** (`devcontainer.json`, a
   compose file, or the Dockerfile) since the container was built, asks
   `Rebuild it? [y/N]` and recreates it (see [Rebuilding](#rebuilding)).
6. Resolves the container-side workspace directory (`docker exec -w`).
7. Resolves which shell to use, then `exec`s `docker exec -it -w … <shell>`.

## Install

The [Quick start](#quick-start) one-liner is all you need. The installer honors
a couple of environment knobs:

| Variable | Default | Purpose |
|---|---|---|
| `DEVCON_INSTALL_DIR` | `~/.local/bin` | where to put the binary |
| `DEVCON_VERSION` | `latest` | install a specific tag, e.g. `v0.1.0` |

Update in place any time:

```sh
devcon self update
```

## Requirements

- `docker`
- A `.devcontainer/devcontainer.json` at (or above) the project root

No VS Code, no Node, no `@devcontainers/cli` — `devcon` is a single static
binary.

## Usage

```
devcon                Bring the stack up (asking first) and drop into a shell
devcon -y             Start the stack without asking if it's down
devcon --rebuild      Force a rebuild+recreate, then connect
devcon --no-rebuild   Skip the drift check; connect to the container as-is
devcon --shell /bin/bash   Override the shell for this run
devcon ls             List dev-container projects on this host
devcon ls --all       …including every compose project, not just dev containers
devcon down           Stop this project's stack (compose down / remove container)
devcon down --stop    Stop but keep the container(s) for a fast reconnect
devcon self update    Update to the latest release
devcon --help         Show help
```

## Shell resolution

`devcon` figures out which shell to drop you into, and remembers the answer:

1. `--shell` flag
2. `.devcontainer/devcon.json` (persisted from a previous run)
3. auto-detect inside the container (`$SHELL`, then probe `zsh → bash → sh`)
4. if detection is ambiguous, ask **once** and save the choice

On the standard wellmade images this is silent — they ship `zsh`, so step 3
resolves immediately.

## Lifecycle hooks

Because VS Code isn't in the loop, the lifecycle hooks it normally runs never
fire. `devcon` runs them itself, with the spec's timing — important because
these dev containers `sleep infinity` and stay up across many connects, so a
`devcon` connect is an *attach*, not a *start*.

**`postCreateCommand` — once per container creation.**

- Containers `devcon` creates carry a `dev.devcon.postcreate` label.
- Any container (including compose services it didn't create) also gets an
  in-container sentinel at `/tmp/.devcon-postcreate-done`.
- Either signal makes later launches skip it. Survives restarts.

**`postStartCommand` — once per container *start*.**

- Keyed on the container's `State.StartedAt` (sentinel
  `/tmp/.devcon-poststart-<startedAt>`).
- Skipped on re-connects to the same running container; **re-runs
  automatically after a real `docker restart`** (new `StartedAt` → new
  sentinel). This is what keeps start-time setup (e.g. wellmade's
  `post-start.sh`) applied in a VS-Code-free flow.

Hooks run as the declared `remoteUser` (if that user exists in the container —
see below) in the resolved workspace folder. The wellmade scripts are
idempotent anyway, so the markers are an optimization, not a correctness
crutch.

## Stopping a stack

`devcon down`, from a project root, tears the stack down — the counterpart to
the bring-up `devcon` does on connect:

- **compose stacks:** `docker compose … down` (stops and removes every service
  and the network).
- **image-based stacks:** `docker rm -f` on the project's container.

Add `--stop` (`-s`) to *stop but keep* the container(s) instead
(`docker compose stop` / `docker stop`) — a later `devcon` reconnects to the
same container without recreating it, which is faster and preserves any
in-container state:

```
devcon down          # remove the stack (compose down / rm -f)
devcon down --stop   # just stop it; reconnect later without a rebuild
```

If nothing is running, `devcon down` is a no-op with a friendly notice.

## Listing projects

`devcon ls` (alias `devcon ps`) shows the dev-container projects present on the
host — running or stopped — by scanning containers for the labels dev
containers carry (`devcontainer.local_folder`, the compose project, and
devcon's own marker):

```
$ devcon ls
up       wellmade-os   compose    (3 containers)
up       devcon        container  *
stopped  some-api      compose

* created by devcon
```

Each row is one project: status, name, kind (a `compose` stack or a single
`container`), and — for compose stacks — how many containers it spans. A `*`
marks containers `devcon` itself created.

By default only dev containers are listed. Add `--all` (`-a`) to include *every*
compose project on the host, not just ones that look like dev containers.

**Limitation:** a project that's fully **down** (`compose down`, or its
container removed) has nothing to list, so it won't appear — `ls` reports
what's *present* on the host (running or merely stopped), not every project
that could be started.

## Rebuilding

VS Code offers a **Rebuild Container** command when you change the stack
definition. `devcon` gives you the same thing, automatically.

Each time you connect, `devcon` compares the mtime of the stack-defining files
against the running container's creation time:

- `.devcontainer/devcontainer.json`
- every `dockerComposeFile`
- the `Dockerfile` referenced by `build.dockerfile` (or the legacy `dockerFile`)

If any of them is **newer than the container**, the stack has *drifted* — the
container no longer reflects its definition — and `devcon` asks:

```
devcon: .devcontainer/docker-compose.yml changed since this container was
        built. Rebuild it? [y/N]
```

The default is **No** (rebuilding drops and recreates the container, so it's
opt-in). On yes:

- **compose stacks:** `docker compose … up -d --build --force-recreate` —
  `--build` picks up Dockerfile edits, `--force-recreate` picks up compose /
  `devcontainer.json` edits even when the image is unchanged.
- **image-based stacks:** the old container is `docker rm -f`'d and re-run.

Either way the fresh container starts with no run-once markers, so
`postCreateCommand` (and `postStartCommand`) re-run on it automatically.

Flags:

| Flag | Effect |
|---|---|
| *(none)* | Detect drift and prompt (the default). |
| `--rebuild` | Always recreate, even with no drift. |
| `--no-rebuild` | Never recreate; connect as-is, no prompt. |

In a non-interactive context (piped, no TTY) `devcon` never drops a container
behind your back: it prints a one-line notice that the stack changed and
connects to the existing container. Pass `--rebuild` (or `-y`) to opt in from a
script.

**Caveat — fresh clones.** Drift is detected by file mtime, and `git clone` /
`git checkout` stamps files with the checkout time. So on a freshly cloned repo
whose container was built earlier, the stack files can look "newer" and trigger
a spurious prompt. The prompt defaults to No and `--no-rebuild` silences it; a
content-hash marker (immune to this) is a candidate future upgrade.

## remoteUser

`devcon` execs as the `remoteUser` from `devcontainer.json` — but only after
verifying that user actually exists in the running container (`id <user>`). If
the running image doesn't provide it, `devcon` warns and falls back to the
image's default user instead of failing with
`unable to find user … in passwd file`.

## Configuration

Per-project config lives at `.devcontainer/devcon.json`:

```json
{
  "shell": "/bin/zsh"
}
```

Global fallback: `~/.config/devcon/config.json`. Precedence: project over
global; `--shell` beats both.

## Codename derivation

The container name (for containers `devcon` creates) is derived from the
project path, same rule as `dcon`:

| `pwd` | Codename |
|---|---|
| `/home/user/workspaces/totophe/devcon` | `totophe_devcon` |
| `/home/user/workspaces/myproject` | `myproject` |
| `/home/user/projects/myapp` | `myapp` |

## Roadmap

- **zellij workspace mode** — a `--workspace` flag that execs
  `zellij attach -c <codename>` instead of a bare shell, completing the
  `tmux → docker → zellij` vision. Parked; the machinery is already in place
  (it only swaps the final exec command).

## Building from source

```sh
cargo build --release
cargo test --all
cargo clippy --all-targets -- -D warnings
```

See [docs/plan.md](docs/plan.md) for the design and module layout.

## License

MIT — see [LICENSE](LICENSE).
