//! `forwardPorts`, done the way VS Code does it: as a *relay*, not a publish.
//!
//! Docker never learns about these ports. We listen on `127.0.0.1:<hostPort>`
//! and, for each connection, spawn `docker exec -i <container> <helper>` where
//! the helper dials the target from *inside* the container's network namespace
//! and pipes bytes back over stdio. Two things fall out of that, both of which
//! `-p` cannot give you:
//!
//!   * the app sees the connection arrive from `localhost`, so binding
//!     `127.0.0.1` inside the container is fine (the spec's warning about
//!     needing `0.0.0.0` applies to `appPort`, not here);
//!   * the target can be a sibling compose service (`"db:5432"`), resolved by
//!     compose's own DNS rather than by anything on the host.
//!
//! Lifetime: `connect::shell` *execs*, so devcon leaves no process behind to
//! host this. The relay therefore runs as a detached child that outlives the
//! `devcon` invocation and shuts itself down when the container goes away. If
//! it dies for any other reason the next `devcon` notices the stale state file
//! and starts a fresh one.

use crate::codename;
use crate::devcontainer::{Devcontainer, PortForward};
use crate::docker::{self, Container};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// How often the watchdog asks whether the container is still up.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// How long `ensure_running` waits for a freshly spawned relay to publish its
/// mappings before giving up on reporting them (it keeps starting regardless).
const STARTUP_POLLS: u32 = 30;
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// In-container helper
// ---------------------------------------------------------------------------

/// The program we run inside the container to reach the target port. We can't
/// assume any particular one exists, so we probe for whichever is there.
///
/// (VS Code sidesteps this by copying its own server binary in. Doing the same
/// — shipping a static helper and `docker cp`-ing it — would remove both the
/// image assumption and the half-close caveat below; until then, one of these
/// five is present in practically every dev image.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    Socat,
    Ncat,
    Nc,
    /// bash's `/dev/tcp` pseudo-device — no external binary needed.
    BashDevTcp,
    Python3,
}

impl Helper {
    /// Probe order: correct stream semantics first, availability second.
    ///
    /// The discriminator is [`Helper::half_closes`] — socat and python3 both
    /// propagate a client's half-close, so they lead. The `nc` family only
    /// manages it on some flavours (OpenBSD `nc` needs `-N`, which busybox
    /// rejects), and bash cannot do it at all, so those are fallbacks.
    const PROBE_ORDER: &'static [Helper] = &[
        Helper::Socat,
        Helper::Python3,
        Helper::Ncat,
        Helper::Nc,
        Helper::BashDevTcp,
    ];

    /// The binary whose presence we test for.
    fn binary(self) -> &'static str {
        match self {
            Helper::Socat => "socat",
            Helper::Ncat => "ncat",
            Helper::Nc => "nc",
            Helper::BashDevTcp => "bash",
            Helper::Python3 => "python3",
        }
    }

    /// Does this helper pass a client's half-close (`shutdown(SHUT_WR)`) on to
    /// the target, rather than holding the write side open?
    ///
    /// It matters for the minority of protocols where the server reads until
    /// EOF before replying. HTTP, dev servers and databases all reply without
    /// waiting, which is why the ones that can't do this are still useful — but
    /// against a read-to-EOF server they will hang, so they go last.
    fn half_closes(self) -> bool {
        match self {
            // socat shuts the socket's write side down on stdin EOF; the python
            // helper below does the same explicitly.
            Helper::Socat | Helper::Python3 => true,
            // bash's /dev/tcp gives one read-write fd with no way to call
            // shutdown(2), and the background reader holds a dup of it, so the
            // FIN never goes out. The nc family varies by flavour.
            Helper::Ncat | Helper::Nc | Helper::BashDevTcp => false,
        }
    }
}

/// The argv (after `docker exec -i <container>`) that connects to
/// `host:port` and relays it over stdio.
fn relay_command(helper: Helper, host: &str, port: u16) -> Vec<String> {
    let s = |v: &str| v.to_string();
    match helper {
        Helper::Socat => vec![s("socat"), s("-"), format!("TCP:{host}:{port}")],
        Helper::Ncat => vec![s("ncat"), s(host), port.to_string()],
        Helper::Nc => vec![s("nc"), s(host), port.to_string()],
        // Open the socket on fd 3, pump it to stdout in the background and
        // stdin into it in the foreground. `exec 3>&-` drops *our* reference on
        // stdin EOF, but the background `cat` still holds a dup, so no FIN goes
        // out — see Helper::half_closes. We keep reading until the server
        // closes, which is how nearly every real protocol ends anyway.
        Helper::BashDevTcp => vec![
            s("bash"),
            s("-c"),
            format!("exec 3<>/dev/tcp/{host}/{port}; cat <&3 & r=$!; cat >&3; exec 3>&-; wait $r"),
        ],
        Helper::Python3 => vec![
            s("python3"),
            s("-c"),
            format!(
                "import socket,sys,threading\n\
                 s=socket.create_connection(({host:?},{port}))\n\
                 def up():\n\
                 \x20   try:\n\
                 \x20       while True:\n\
                 \x20           d=sys.stdin.buffer.read1(65536)\n\
                 \x20           if not d: break\n\
                 \x20           s.sendall(d)\n\
                 \x20   finally:\n\
                 \x20       try: s.shutdown(socket.SHUT_WR)\n\
                 \x20       except OSError: pass\n\
                 threading.Thread(target=up,daemon=True).start()\n\
                 while True:\n\
                 \x20   d=s.recv(65536)\n\
                 \x20   if not d: break\n\
                 \x20   sys.stdout.buffer.write(d); sys.stdout.buffer.flush()\n"
            ),
        ],
    }
}

/// Probe the container for a usable helper, best first.
fn detect_helper(container: &Container) -> Option<Helper> {
    Helper::PROBE_ORDER.iter().copied().find(|h| {
        docker::exec_capture(
            container,
            &["sh", "-c", &format!("command -v {}", h.binary())],
        )
        .map(|out| !out.is_empty())
        .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Host-side binding
// ---------------------------------------------------------------------------

/// Bind a loopback listener for `desired`, shifting to a nearby free port if
/// it's taken (VS Code does the same — nothing is published, so the exact host
/// port is a preference, not a contract).
///
/// Returns the listener rather than a port number on purpose: reading the port
/// back off a bound socket leaves no window for someone else to take it.
fn pick_host_port(desired: u16) -> io::Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", desired)) {
        Ok(l) => return Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e),
    }
    let end = desired.saturating_add(64);
    for candidate in desired.saturating_add(1)..=end {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", candidate)) {
            return Ok(l);
        }
    }
    // Give up on staying near the requested port and take whatever is free.
    TcpListener::bind(("127.0.0.1", 0))
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

/// Accept forever, handing each connection to its own thread.
fn serve(listener: TcpListener, container_id: String, helper: Helper, target: PortForward) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let argv = relay_command(helper, &target.target_host, target.container_port);
        let id = container_id.clone();
        thread::spawn(move || {
            if let Err(e) = pump(stream, &id, &argv) {
                eprintln!("devcon: relay connection failed: {e}");
            }
        });
    }
}

/// Splice one accepted connection to a `docker exec` helper process.
fn pump(stream: TcpStream, container_id: &str, argv: &[String]) -> io::Result<()> {
    let child = Command::new("docker")
        .arg("exec")
        .arg("-i")
        .arg(container_id)
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited, not swallowed: if the helper can't reach the target this
        // is the only place that says so (the log file, when detached).
        .stderr(Stdio::inherit())
        .spawn()?;
    splice(stream, child)
}

/// Hand every chunk from `src` to `dst` as soon as it arrives, until either
/// end hangs up.
///
/// Deliberately an explicit loop rather than [`io::copy`]: copying a socket
/// into a pipe, `io::copy` held the bytes back until the source reached EOF.
/// That's invisible for request/response traffic where the client half-closes,
/// and a hang for everything else — a browser, curl, any keep-alive protocol
/// keeps its write side open, so EOF never comes and the request never lands.
fn forward_stream(src: &mut impl Read, dst: &mut impl Write) {
    let mut buf = [0u8; 32 * 1024];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() || dst.flush().is_err() {
                    return;
                }
            }
        }
    }
}

/// Pump bytes both ways between a socket and a child's stdio until either end
/// hangs up. Split out from [`pump`] so the byte plumbing — the fiddly part —
/// can be tested against a local process instead of a container.
fn splice(stream: TcpStream, mut child: std::process::Child) -> io::Result<()> {
    let mut child_stdin = child.stdin.take().expect("stdin piped");
    let mut child_stdout = child.stdout.take().expect("stdout piped");
    let mut from_host = stream.try_clone()?;
    let mut to_host = stream;

    // Host → container. Dropping `child_stdin` on the way out gives the helper
    // an EOF, which is how it learns the client hung up.
    let uploader = thread::spawn(move || {
        forward_stream(&mut from_host, &mut child_stdin);
    });

    // Container → host, on this thread.
    forward_stream(&mut child_stdout, &mut to_host);

    // The helper is done talking, so the connection is over. Shutting the
    // socket down unblocks the uploader if it's still parked in read().
    let _ = to_host.shutdown(Shutdown::Both);
    let _ = child.wait();
    let _ = uploader.join();
    Ok(())
}

// ---------------------------------------------------------------------------
// State file
// ---------------------------------------------------------------------------

/// One live mapping, as recorded for `devcon forward --list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mapping {
    /// The port actually bound on the host (may differ from what was asked).
    pub host_port: u16,
    /// What was asked for, so we can point out when it shifted.
    pub desired_host_port: u16,
    pub target_host: String,
    pub container_port: u16,
}

/// Host-local runtime state for one project's relay. Deliberately *not* in
/// `.devcontainer/devcon.json` — that file is committed project config, this is
/// a pid that means nothing on another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    pid: u32,
    container_id: String,
    mappings: Vec<Mapping>,
}

fn state_dir() -> Option<PathBuf> {
    // dirs-next 2 has no state_dir(); data_dir() (~/.local/share) is the
    // closest thing it offers.
    dirs_next::data_dir().map(|d| d.join("devcon"))
}

fn state_path(dc: &Devcontainer) -> Option<PathBuf> {
    let codename = codename::derive(&dc.project_root);
    state_dir().map(|d| d.join(format!("forward-{codename}.json")))
}

/// Where a detached relay's diagnostics go, since it has no terminal.
fn log_path(dc: &Devcontainer) -> Option<PathBuf> {
    let codename = codename::derive(&dc.project_root);
    state_dir().map(|d| d.join(format!("forward-{codename}.log")))
}

fn read_state(path: &PathBuf) -> Option<State> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_state(path: &PathBuf, state: &State) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
    fs::write(path, format!("{json}\n"))
}

/// Is that pid still a live devcon relay?
///
/// On Linux we read the cmdline as well as checking existence, so a recycled
/// pid now belonging to something unrelated doesn't read as "already running".
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => String::from_utf8_lossy(&cmdline).contains("devcon"),
        Err(_) => false,
    }
}

/// Elsewhere, existence is all we can cheaply establish.
#[cfg(not(target_os = "linux"))]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Print the mappings the way the rest of devcon talks.
fn report(mappings: &[Mapping]) {
    for m in mappings {
        let target = if m.target_host == "localhost" {
            m.container_port.to_string()
        } else {
            format!("{}:{}", m.target_host, m.container_port)
        };
        let shifted = if m.host_port == m.desired_host_port {
            String::new()
        } else {
            format!(" \x1b[90m({} was busy)\x1b[0m", m.desired_host_port)
        };
        eprintln!(
            "\x1b[36mdevcon:\x1b[0m forwarding localhost:{} → {target}{shifted}",
            m.host_port
        );
    }
}

/// Make sure a relay is running for this project, starting one if not.
///
/// Never fails the caller: forwarding is a convenience, and no port is worth
/// standing between someone and their shell.
pub fn ensure_running(dc: &Devcontainer, container: &Container) {
    if dc.forward_ports.is_empty() {
        return;
    }
    let Some(path) = state_path(dc) else {
        eprintln!("\x1b[33mdevcon:\x1b[0m no state directory — skipping port forwarding.");
        return;
    };

    // Already up for *this* container? Then just restate the mappings.
    if let Some(state) = read_state(&path) {
        if state.container_id == container.id && pid_alive(state.pid) {
            report(&state.mappings);
            return;
        }
        // Stale: a dead relay, or one bound to a container we've since rebuilt.
        let _ = fs::remove_file(&path);
    }

    if let Err(e) = spawn_detached(dc) {
        eprintln!("\x1b[33mdevcon:\x1b[0m could not start port forwarding: {e}");
        return;
    }

    // The child publishes its mappings once it has bound; wait briefly so we
    // can show them, but don't hold up the shell if it's slow.
    for _ in 0..STARTUP_POLLS {
        thread::sleep(STARTUP_POLL_INTERVAL);
        if let Some(state) = read_state(&path) {
            report(&state.mappings);
            return;
        }
    }
    let log = log_path(dc)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    eprintln!("\x1b[33mdevcon:\x1b[0m port forwarding is still starting; see {log}");
}

/// Launch `devcon forward --detached` in the background.
///
/// It inherits nothing from our stdio — the user is about to get a shell on
/// this terminal — so its diagnostics go to a log file instead.
fn spawn_detached(dc: &Devcontainer) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let log = match log_path(dc) {
        Some(p) => {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            Stdio::from(fs::File::create(p)?)
        }
        None => Stdio::null(),
    };
    Command::new(exe)
        .arg("forward")
        .arg("--detached")
        .current_dir(&dc.project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map(|_| ())
}

/// Run the relay in the foreground until the container goes away.
///
/// This is both `devcon forward` (run it yourself, watch it work) and what the
/// detached child executes — one implementation, two ways in.
pub fn run(dc: &Devcontainer, container: &Container) -> Result<(), Error> {
    if dc.forward_ports.is_empty() {
        eprintln!("\x1b[36mdevcon:\x1b[0m no forwardPorts declared — nothing to forward.");
        return Ok(());
    }

    let helper = detect_helper(container).ok_or_else(|| {
        Error::NoHelper(dc.forward_ports.iter().map(|p| p.container_port).collect())
    })?;
    if !helper.half_closes() {
        // Better to say so up front than to have someone debug a hang later.
        eprintln!(
            "\x1b[33mdevcon:\x1b[0m forwarding via {} — it can't signal end-of-request, so a \
             server that reads until EOF will hang. Add socat to the image to avoid this.",
            helper.binary()
        );
    }

    let mut listeners = Vec::new();
    let mut mappings = Vec::new();
    for target in &dc.forward_ports {
        let listener = pick_host_port(target.desired_host_port).map_err(Error::Io)?;
        let host_port = listener.local_addr().map_err(Error::Io)?.port();
        mappings.push(Mapping {
            host_port,
            desired_host_port: target.desired_host_port,
            target_host: target.target_host.clone(),
            container_port: target.container_port,
        });
        listeners.push((listener, target.clone()));
    }

    if let Some(path) = state_path(dc) {
        let state = State {
            pid: std::process::id(),
            container_id: container.id.clone(),
            mappings: mappings.clone(),
        };
        if let Err(e) = write_state(&path, &state) {
            eprintln!("\x1b[33mdevcon:\x1b[0m could not record forward state: {e}");
        }
    }
    report(&mappings);

    for (listener, target) in listeners {
        let id = container.id.clone();
        thread::spawn(move || serve(listener, id, helper, target));
    }

    // Outliving the container would leave listeners accepting into nothing.
    while docker::container_running(container) {
        thread::sleep(WATCHDOG_INTERVAL);
    }
    eprintln!("\x1b[36mdevcon:\x1b[0m container stopped — port forwarding ended.");
    stop(dc);
    Ok(())
}

/// Print the mappings a running relay has published, if any.
pub fn list(dc: &Devcontainer) {
    let state = state_path(dc).and_then(|p| read_state(&p));
    match state {
        Some(state) if pid_alive(state.pid) => report(&state.mappings),
        _ => eprintln!("\x1b[36mdevcon:\x1b[0m no ports are being forwarded for this project."),
    }
}

/// Tear down this project's relay: kill it if it's someone else, and clear the
/// state file either way. Called on `devcon down` and by the watchdog on exit.
pub fn stop(dc: &Devcontainer) {
    let Some(path) = state_path(dc) else { return };
    if let Some(state) = read_state(&path) {
        if state.pid != std::process::id() && pid_alive(state.pid) {
            let _ = Command::new("kill")
                .arg(state.pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ = fs::remove_file(&path);
}

// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    /// No program inside the container can open a TCP connection for us.
    NoHelper(Vec<u16>),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoHelper(ports) => {
                let list = ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "cannot forward {list}: the container has none of socat, ncat, nc, bash \
                     or python3, and one of them is needed to open the connection from inside. \
                     Add socat to the image to enable forwarding."
                )
            }
            Error::Io(e) => write!(f, "i/o error setting up port forwarding: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::SocketAddr;

    /// Serve exactly one connection by splicing it to `argv`, and hand back the
    /// address to connect to. Stands in for the `docker exec` half of [`pump`].
    fn serve_once(argv: Vec<String>) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let child = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            splice(sock, child).unwrap();
        });
        (addr, handle)
    }

    /// A local TCP server standing in for "the app inside the container".
    ///
    /// `wait_for_eof` picks which kind: `false` replies as soon as it has bytes
    /// and then closes, the way HTTP and database servers behave; `true` drains
    /// to EOF first, which only works if the helper propagates a half-close.
    fn echo_server(wait_for_eof: bool) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            if wait_for_eof {
                sock.read_to_end(&mut buf).unwrap();
            } else {
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            sock.write_all(&buf).unwrap();
            // Closing is what ends the exchange for a server-driven protocol.
        });
        (port, handle)
    }

    fn have(binary: &str) -> bool {
        Command::new("sh")
            .args(["-c", &format!("command -v {binary}")])
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Drive one full round trip: connect, send, half-close, read the reply.
    fn round_trip(addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(payload).unwrap();
        // Half-close so the far end sees EOF and knows the request is complete.
        client.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        got
    }

    /// Like [`round_trip`], but the client keeps its write side open — which is
    /// what browsers, curl and every keep-alive client actually do. The
    /// exchange ends because the *server* closes, not because we signalled EOF.
    fn round_trip_client_stays_open(addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(payload).unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        got
    }

    // The three tests below pin the bug that `io::copy` hid: bytes must reach
    // the far side *as they arrive*, not once the sender reaches EOF. Every one
    // of them hangs rather than fails if that regresses, so they deliberately
    // keep the client's write side open the way a real client does.

    #[test]
    fn download_reaches_the_client_before_any_request_is_sent() {
        // The helper talks first and the client has sent nothing at all.
        let (addr, _s) = serve_once(vec!["sh".into(), "-c".into(), "echo READY; cat".into()]);
        let mut client = TcpStream::connect(addr).unwrap();
        let mut got = [0u8; 6];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"READY\n");
    }

    #[test]
    fn upload_reaches_the_helper_without_waiting_for_eof() {
        // `head -c 14` only speaks once it has all 14 bytes, and the client
        // never closes its write side — so this passes only if each chunk is
        // forwarded on arrival.
        let (addr, _s) = serve_once(vec![
            "sh".into(),
            "-c".into(),
            "head -c 14 >/dev/null; echo GOT".into(),
        ]);
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(b"0123456789abcd").unwrap();
        let mut got = [0u8; 4];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"GOT\n");
    }

    #[test]
    fn helper_relays_when_the_client_never_half_closes() {
        // The end-to-end shape of an HTTP request: client keeps the connection
        // open, server replies and closes.
        let helper = Helper::Python3;
        if !have(helper.binary()) {
            eprintln!("skipping: python3 not installed");
            return;
        }
        let (target_port, echo) = echo_server(false);
        let argv = relay_command(helper, "127.0.0.1", target_port);
        let (addr, server) = serve_once(argv);
        assert_eq!(
            round_trip_client_stays_open(addr, b"GET / HTTP/1.1"),
            b"GET / HTTP/1.1"
        );
        server.join().unwrap();
        echo.join().unwrap();
    }

    #[test]
    fn splice_round_trips_and_terminates_on_half_close() {
        // `cat` echoes stdin to stdout and exits on EOF, so this exercises the
        // whole shape: upload EOF → helper exits → download EOF → we return.
        let (addr, server) = serve_once(vec!["cat".into()]);
        assert_eq!(round_trip(addr, b"hello relay"), b"hello relay");
        server.join().unwrap();
    }

    #[test]
    fn splice_handles_a_payload_larger_than_one_pipe_buffer() {
        let (addr, server) = serve_once(vec!["cat".into()]);
        let payload = vec![b'x'; 1 << 20];
        assert_eq!(round_trip(addr, &payload).len(), payload.len());
        server.join().unwrap();
    }

    /// The helper commands are hand-written shell and python, so run them for
    /// real against a local server rather than trusting them by eye.
    ///
    /// `wait_for_eof` must only be set for helpers that [`Helper::half_closes`];
    /// the others would hang here, which is exactly the property being pinned.
    fn assert_helper_relays(helper: Helper, wait_for_eof: bool) {
        if !have(helper.binary()) {
            eprintln!("skipping {helper:?}: {} not installed", helper.binary());
            return;
        }
        let (target_port, echo) = echo_server(wait_for_eof);
        let argv = relay_command(helper, "127.0.0.1", target_port);
        let (addr, server) = serve_once(argv);
        assert_eq!(
            round_trip(addr, b"through the helper"),
            b"through the helper"
        );
        server.join().unwrap();
        echo.join().unwrap();
    }

    #[test]
    fn bash_dev_tcp_helper_relays_a_server_driven_exchange() {
        assert_helper_relays(Helper::BashDevTcp, false);
    }

    #[test]
    fn python3_helper_relays_and_propagates_half_close() {
        assert_helper_relays(Helper::Python3, true);
    }

    #[test]
    fn socat_helper_relays_and_propagates_half_close() {
        assert_helper_relays(Helper::Socat, true);
    }

    #[test]
    fn helpers_that_half_close_are_probed_first() {
        // Ordering is what keeps the bash fallback from being chosen over a
        // fully-correct helper that's also installed.
        let first_without = Helper::PROBE_ORDER
            .iter()
            .position(|h| !h.half_closes())
            .unwrap();
        let last_with = Helper::PROBE_ORDER
            .iter()
            .rposition(|h| h.half_closes())
            .unwrap();
        assert!(
            last_with < first_without,
            "probe order mixes the two classes: {:?}",
            Helper::PROBE_ORDER
        );
    }

    #[test]
    fn relay_command_socat_targets_host_and_port() {
        assert_eq!(
            relay_command(Helper::Socat, "localhost", 3000),
            vec!["socat", "-", "TCP:localhost:3000"]
        );
    }

    #[test]
    fn relay_command_nc_family_takes_bare_host_port() {
        assert_eq!(
            relay_command(Helper::Nc, "db", 5432),
            vec!["nc", "db", "5432"]
        );
        assert_eq!(
            relay_command(Helper::Ncat, "db", 5432),
            vec!["ncat", "db", "5432"]
        );
    }

    #[test]
    fn relay_command_bash_uses_dev_tcp_for_the_target() {
        let argv = relay_command(Helper::BashDevTcp, "db", 5432);
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("/dev/tcp/db/5432"), "{}", argv[2]);
    }

    #[test]
    fn relay_command_python_quotes_the_host() {
        let argv = relay_command(Helper::Python3, "db", 5432);
        assert_eq!(argv[0], "python3");
        // The host has to reach python as a string literal, not a bare name.
        assert!(argv[2].contains("(\"db\",5432)"), "{}", argv[2]);
    }

    #[test]
    fn pick_host_port_takes_the_requested_port_when_free() {
        // Ask the OS for a free port, release it, then claim it by number.
        // There's an unavoidable gap there in which a concurrently-running test
        // can take the port, so retry rather than let a lost race read as a bug.
        let kept_the_request = (0..8).any(|_| {
            let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let free = probe.local_addr().unwrap().port();
            drop(probe);
            pick_host_port(free).unwrap().local_addr().unwrap().port() == free
        });
        assert!(
            kept_the_request,
            "never returned the free port it was asked for"
        );
    }

    #[test]
    fn pick_host_port_shifts_when_the_port_is_taken() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = occupied.local_addr().unwrap().port();

        let listener = pick_host_port(taken).unwrap();
        let got = listener.local_addr().unwrap().port();
        assert_ne!(got, taken, "should not have handed back the busy port");
        assert!(got > taken && got <= taken.saturating_add(64), "got {got}");
    }

    #[test]
    fn mapping_round_trips_through_the_state_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("forward-x.json");
        let state = State {
            pid: 4242,
            container_id: "abc123".into(),
            mappings: vec![Mapping {
                host_port: 3001,
                desired_host_port: 3000,
                target_host: "db".into(),
                container_port: 5432,
            }],
        };
        write_state(&path, &state).unwrap();
        let back = read_state(&path).unwrap();
        assert_eq!(back.pid, 4242);
        assert_eq!(back.container_id, "abc123");
        assert_eq!(back.mappings, state.mappings);
    }

    #[test]
    fn read_state_tolerates_a_missing_or_corrupt_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nope.json");
        assert!(read_state(&missing).is_none());

        let junk = tmp.path().join("junk.json");
        fs::write(&junk, "not json").unwrap();
        assert!(read_state(&junk).is_none());
    }
}
