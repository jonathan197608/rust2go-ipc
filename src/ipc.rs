//! Minimal IPC runtime for `rust2go-ipc`.
//!
//! Generated binding stubs call [`dispatch`] for async calls. A reactor thread
//! reads responses and invokes a per-method trampoline generated alongside the
//! stub; that trampoline restores the Ref structure from flat bytes and calls
//! the original rust2go `*_cb` function, which writes into the atomic slot and
//! wakes the awaiting future.

use std::{
    any::Any,
    collections::HashMap,
    env,
    io::{self, Error as IoError, ErrorKind, Read, Write},
    mem::size_of,
    net::{Shutdown, TcpListener, TcpStream},
    panic::{catch_unwind, AssertUnwindSafe},
    process::{Child, Command, Stdio},
    ptr::null,
    slice::from_raw_parts,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        Mutex, Once, OnceLock,
    },
    thread::{self, sleep},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::process::id as process_id;

const MAGIC: [u8; 4] = *b"R2GI";
const VERSION: u16 = 1;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const DEFAULT_PENDING_TTL: u64 = 600;
const SWEEP_INTERVAL: u64 = 30;
/// A hung Go handler must not block a synchronous business thread for the
/// async-scale TTL; sync calls get a shorter, independent timeout.
const DEFAULT_SYNC_TIMEOUT: u64 = 30;
/// A self-referential struct (e.g. `Node { children: Vec<Node> }`) is legal Rust
/// but produces unbounded mutual recursion in the generated restore helpers.
/// A corrupt/hostile 64MB frame could otherwise drive millions of nested calls
/// and overflow the stack — which `catch_unwind` cannot catch (it aborts). Both
/// the Rust `Decoder` and the Go `dec` enforce this same limit symmetrically.
const MAX_RESTORE_DEPTH: u32 = 64;

const ENV_SERVER_OVERRIDE: &str = "RUST2GO_IPC_SERVER_OVERRIDE";
const ENV_SYNC_TIMEOUT: &str = "RUST2GO_IPC_SYNC_TIMEOUT_SECS";
const ENV_ENDPOINT_OVERRIDE: &str = "RUST2GO_IPC_ENDPOINT_OVERRIDE";
const ENV_PORT_OVERRIDE: &str = "RUST2GO_IPC_PORT_OVERRIDE";

/// Transport is chosen from the compile target, not from runtime probing:
/// UDS on Unix, TCP loopback everywhere else (Windows, wasi, ...).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    Tcp,
    #[cfg(unix)]
    Unix,
}

impl Transport {
    fn as_str(self) -> &'static str {
        match self {
            Transport::Tcp => "tcp",
            #[cfg(unix)]
            Transport::Unix => "unix",
        }
    }
}

fn default_transport() -> Transport {
    #[cfg(unix)]
    {
        Transport::Unix
    }
    #[cfg(not(unix))]
    {
        Transport::Tcp
    }
}

#[derive(Clone, Debug)]
struct Endpoint {
    transport: Transport,
    addr: String,
}

fn env_nonempty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

/// Read the synchronous-call timeout, rejecting 0 so a misconfiguration cannot
/// disable the deadline entirely.
fn sync_timeout_secs() -> u64 {
    env_nonempty(ENV_SYNC_TIMEOUT)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SYNC_TIMEOUT)
}

/// Resolve the endpoint shared by the Rust client and the Go server.
/// Overrides: full endpoint, or just the port.
///
/// Transport is fixed by the compile target and cannot be overridden: on Unix
/// it is always UDS. A TCP loopback listener accepts any local process with no
/// peer authentication, whereas a UDS node can be chmod-restricted to the
/// owning uid (the generated Go server does exactly that). Allowing an override
/// back to TCP would silently reopen that attack surface, so it is removed.
fn resolve_endpoint(port: u16) -> Endpoint {
    let transport = default_transport();
    if let Some(addr) = env_nonempty(ENV_ENDPOINT_OVERRIDE) {
        // The Go server fail-closes on >=100-byte UDS endpoints with a panic.
        // Apply the same bound here so Rust fails at startup with a clear
        // message instead of looping through respawns until the 30s deadline.
        #[cfg(unix)]
        {
            if transport == Transport::Unix && addr.len() >= 100 {
                panic!(
                    "rust2go-ipc: {ENV_ENDPOINT_OVERRIDE} Unix socket path must be <100 bytes, got {}",
                    addr.len()
                );
            }
        }
        return Endpoint { transport, addr };
    }
    match transport {
        #[cfg(unix)]
        Transport::Unix => {
            let path = env::temp_dir().join(format!("rust2go-ipc-{}-{}.sock", process_id(), port));
            // sockaddr_un.sun_path is ~108 bytes on Linux / 104 on macOS.
            // Measure the exact string handed to both sides: Rust connects with
            // this `String` and Go measures `len(endpoint)`. Checking raw OS
            // bytes here (before to_string_lossy) would disagree with Go for
            // non-UTF-8 TMPDIR paths and let an over-long path reach bind().
            let mut addr = path.to_string_lossy().into_owned();
            if addr.len() >= 100 {
                addr = format!("/tmp/rust2go-ipc-{}-{}.sock", process_id(), port);
            }
            Endpoint {
                transport: Transport::Unix,
                addr,
            }
        }
        Transport::Tcp => Endpoint {
            transport: Transport::Tcp,
            addr: format!("127.0.0.1:{port}"),
        },
    }
}

/// Unified byte stream over either transport. Both variants implement
/// `Read + Write + try_clone`, so the reactor needs no transport-specific code.
enum IpcStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl IpcStream {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            IpcStream::Tcp(s) => s.try_clone().map(IpcStream::Tcp),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.try_clone().map(IpcStream::Unix),
        }
    }

    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            IpcStream::Tcp(s) => s.set_read_timeout(Some(timeout)),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.set_read_timeout(Some(timeout)),
        }
    }

    fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            IpcStream::Tcp(s) => s.set_write_timeout(Some(timeout)),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.set_write_timeout(Some(timeout)),
        }
    }

    /// Shut down both directions so the reactor's cloned reader observes EOF
    /// immediately and rebuilds the connection. Used after a failed write,
    /// where a partial frame would otherwise desynchronise the peer's read
    /// loop while the reactor (seeing only a live child) keeps the connection.
    fn shutdown_both(&self) {
        match self {
            IpcStream::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            #[cfg(unix)]
            IpcStream::Unix(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            IpcStream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            IpcStream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            IpcStream::Tcp(s) => s.flush(),
            #[cfg(unix)]
            IpcStream::Unix(s) => s.flush(),
        }
    }
}

/// Signature of a generated response trampoline:
/// `(payload_ptr, payload_len, slot, cb)`.
///
/// `C-unwind` (not `C`) is deliberate: a malformed response can make the
/// generated restore code panic. With `extern "C"` that panic cannot unwind and
/// aborts the whole process, which would defeat the reactor's `catch_unwind`.
/// `C-unwind` lets the panic cross back into the reactor, where it is logged
/// and the pending entry dropped.
pub type Trampoline = unsafe extern "C-unwind" fn(*const u8, usize, *const (), *const ());

/// Type-erased destructor for an async slot that will never receive a
/// callback. Generated per method because the concrete `SlotWriter<T, A>`
/// type is needed to release the slot and its attachment.
pub type SlotCleanup = unsafe fn(*const ());

/// Everything the runtime needs to decode a response and, if no response
/// arrives, release the caller's slot. Kept in one value to avoid an 8-argument
/// public API.
#[derive(Clone, Copy)]
pub struct ResponseHandler {
    pub trampoline: Trampoline,
    pub cleanup: Option<SlotCleanup>,
}

impl ResponseHandler {
    pub const fn new(trampoline: Trampoline, cleanup: Option<SlotCleanup>) -> Self {
        Self {
            trampoline,
            cleanup,
        }
    }
}

mod sealed {
    /// Prevents downstream crates from adding wire primitives. Only the
    /// fixed set below is known to be padding-free, which is what makes the
    /// raw byte reads in Encoder/Decoder sound.
    pub trait Sealed {}
}

/// Types that can be bit-copied to/from the IPC wire format.
///
/// Sealed on purpose: implementors are all fixed-width primitives with no
/// padding bytes, so reading `size_of::<T>()` bytes never observes
/// uninitialised padding. `bool` is special-cased to reject invalid bit
/// patterns instead of creating an invalid value (which would be UB).
pub trait WirePrim: Copy + 'static + sealed::Sealed {
    /// Decode one value from a byte slice of exactly `size_of::<Self>()` bytes.
    #[allow(clippy::missing_safety_doc)]
    fn read(bytes: &[u8]) -> Self;

    /// Append this value's wire bytes to `out`.
    fn write(self, out: &mut Vec<u8>);

    /// Validate a raw byte run holding a list of this type (default: no-op).
    /// `bool` overrides this to reject bytes other than 0/1.
    fn validate(_bytes: &[u8]) {}
}

/// All multi-byte primitives are encoded explicitly little-endian. The Go
/// producer uses `binary.LittleEndian`, so relying on native byte order would
/// silently corrupt data on big-endian Rust targets.
macro_rules! impl_wire_prim_le {
    ($($t:ty),+ $(,)?) => {
        $(
            impl sealed::Sealed for $t {}
            impl WirePrim for $t {
                fn read(bytes: &[u8]) -> Self {
                    let expected = size_of::<Self>();
                    assert_eq!(
                        bytes.len(), expected,
                        "rust2go-ipc: primitive wire length mismatch"
                    );
                    let mut arr = [0u8; size_of::<Self>()];
                    arr.copy_from_slice(bytes);
                    Self::from_le_bytes(arr)
                }
                fn write(self, out: &mut Vec<u8>) {
                    out.extend_from_slice(&self.to_le_bytes());
                }
            }
        )+
    };
}

impl_wire_prim_le!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl sealed::Sealed for char {}
impl WirePrim for char {
    fn read(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 4, "rust2go-ipc: char wire length mismatch");
        let mut arr = [0u8; 4];
        arr.copy_from_slice(bytes);
        let scalar = u32::from_le_bytes(arr);
        char::from_u32(scalar)
            .unwrap_or_else(|| panic!("rust2go-ipc: invalid char scalar 0x{scalar:08x}"))
    }
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self as u32).to_le_bytes());
    }
}

impl sealed::Sealed for bool {}
impl WirePrim for bool {
    fn read(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 1, "rust2go-ipc: bool wire length mismatch");
        // Never reinterpret arbitrary bytes as bool (non-0/1 is UB). The wire
        // producer emits 0/1 only; a corrupt frame panics instead of becoming UB.
        match bytes[0] {
            0 => false,
            1 => true,
            v => panic!("rust2go-ipc: invalid bool byte 0x{v:02x} in response"),
        }
    }
    fn write(self, out: &mut Vec<u8>) {
        out.push(u8::from(self));
    }
    fn validate(bytes: &[u8]) {
        assert!(
            bytes.iter().all(|&b| b <= 1),
            "rust2go-ipc: invalid bool byte in list response"
        );
    }
}

struct Pending {
    // Store the slot/cb addresses as integers rather than raw pointers: `*const ()`
    // is neither Send nor Sync, which would force `unsafe impl` on Runtime. The
    // round-trip `ptr as usize as *const ()` is lossless and keeps the unsafe
    // scope confined to the point where the trampoline is actually invoked.
    slot: usize,
    cb: usize,
    trampoline: Trampoline,
    cleanup: Option<SlotCleanup>,
    created: Instant,
}

impl Pending {
    /// Release the slot when no callback will ever consume it. Must be called
    /// after removing the pending entry from the map: waking the future can
    /// re-enter the runtime.
    fn cleanup(self) {
        if let Some(f) = self.cleanup {
            // SAFETY: the slot pointer came from the generated stub's
            // `SlotWriter::into_ptr()` and is only released once.
            unsafe { f(self.slot as *const ()) };
        }
    }
}

struct Runtime {
    server: String,
    port: u16,
    endpoint: Endpoint,
    child: Mutex<Option<Child>>,
    writer: Mutex<IpcStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    /// Monotonic clock for sweep pacing (a wall clock can jump backwards and
    /// stall cleanup indefinitely).
    last_sweep: Mutex<Instant>,
    pending_ttl: Duration,
}

// Runtime is Send + Sync without manual impls: slot/cb addresses are stored as
// `usize`, and Trampoline (a fn pointer) is Send + Sync. The remaining fields
// are Mutex/Atomic/String/Child, all thread-safe.

/// `&'static Runtime` (rather than an owned value) lets the reactor thread
/// receive the runtime directly instead of spin-waiting for `OnceLock` to be
/// populated after this initialization closure returns.
static RUNTIME: OnceLock<&'static Runtime> = OnceLock::new();
static ATEXIT: Once = Once::new();
/// Set by the atexit hook before reaping the child. `reconnect` checks it so a
/// concurrent reactor restart cannot spawn a fresh child that outlives exit.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
/// The child's pid, recorded at spawn time (before `RUNTIME` is populated) so
/// the atexit hook can still kill it during the OnceLock initialization window.
/// 0 means either no child yet or already reaped.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// Serialises child reap/spawn between the atexit hook and the reactor so a
/// newly spawned child can never slip in after the hook has already reaped.
static CHILD_MGMT: Mutex<()> = Mutex::new(());

/// Kill and reap the child, then close the pipe held by the runtime.
fn reap_child(rt: &Runtime) {
    CHILD_PID.store(0, Ordering::SeqCst);
    if let Some(mut child) = lock_or_recover(&rt.child).take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Rust does not drop `static` values at process exit. Without this hook the
/// Go server survives as an orphan, keeping the port/socket occupied.
extern "C" fn cleanup_on_exit() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    let _mgmt = lock_or_recover(&CHILD_MGMT);
    if let Some(rt) = RUNTIME.get() {
        reap_child(rt);
    } else {
        // Initialization window: spawn already happened inside the OnceLock
        // closure but RUNTIME is not published yet, so reap_child has no
        // handle to work with. On Unix, kill by the pid recorded at spawn time;
        // the process is exiting anyway, so the kernel reaps the orphan and the
        // Go side removes any stale socket on its next start. Windows has no
        // libc kill API and this window is microseconds wide, so the normal
        // (RUNTIME-published) reap path is the guarantee there.
        #[cfg(unix)]
        {
            let pid = CHILD_PID.swap(0, Ordering::SeqCst);
            if pid > 0 {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(unix))]
        {
            CHILD_PID.store(0, Ordering::SeqCst);
        }
    }
}

/// Spawn the Go server once and start the reactor. `server` is the path to the
/// compiled Go IPC binary; `port` was reserved by the build script.
///
/// Holds CHILD_MGMT across spawn + pid publication so the atexit hook can never
/// observe a spawned child whose pid has not been recorded yet (which would
/// leave an orphan on process exit).
fn spawn_server(server: &str, port: u16, endpoint: &Endpoint) -> Child {
    let _mgmt = lock_or_recover(&CHILD_MGMT);
    spawn_server_locked(server, port, endpoint)
}

/// Spawn and publish the child pid. The caller must hold CHILD_MGMT; this
/// variant exists for `reconnect`, which holds the lock while reaping the
/// previous child so no new child can slip in after the reap.
fn spawn_server_locked(server: &str, port: u16, endpoint: &Endpoint) -> Child {
    let child = Command::new(server)
        .env("RUST2GO_IPC_PORT", port.to_string())
        .env("RUST2GO_IPC_TRANSPORT", endpoint.transport.as_str())
        .env("RUST2GO_IPC_ENDPOINT", &endpoint.addr)
        // Isolate the child from the parent's stdio. stdin is unused; stdout
        // has no protocol meaning here. stderr is deliberately inherited so
        // a Go-side panic remains diagnosable (the Rust side also logs the
        // child's exit status during startup/respawn).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("rust2go-ipc: cannot spawn Go server {server}: {e}"));
    // Record the pid immediately, before the caller stores the Child handle,
    // so atexit can still kill it during the OnceLock initialization window.
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    child
}

/// kill + wait + clear pid. The caller must hold CHILD_MGMT.
fn kill_child_locked(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
    CHILD_PID.store(0, Ordering::SeqCst);
}

/// Kill and reap a child whose pid was published by spawn_server, clearing the
/// pid under CHILD_MGMT. Keeping kill + wait + clear inside the same lock the
/// atexit hook takes guarantees the hook can never act on a stale (and possibly
/// recycled) pid after this function returns.
fn kill_and_reap(child: &mut Child) {
    let _mgmt = lock_or_recover(&CHILD_MGMT);
    kill_child_locked(child);
}

/// Recover a Mutex after a panic in a previous holder. A poisoning panic must
/// never cascade into the reactor (the only recovery mechanism): returning the
/// inner guard keeps the service alive. All protected state is kept consistent
/// by the callers even on the error paths.
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Apply the timeouts the reactor and dispatch paths rely on. A silent failure
/// here would remove the idle tick that sweeps expired pending entries (reader)
/// or let a wedged write block a dispatch forever (writer).
fn set_stream_timeouts(writer: &mut IpcStream, reader: &mut IpcStream) -> std::io::Result<()> {
    writer.set_write_timeout(Duration::from_secs(30))?;
    reader.set_read_timeout(Duration::from_secs(300))?;
    Ok(())
}

fn runtime(server: &str, port: u16) -> &'static Runtime {
    // Build-time paths live under OUT_DIR and do not exist in a stripped
    // runtime image; let deployment point at the copied binary.
    let server = env_nonempty(ENV_SERVER_OVERRIDE).unwrap_or_else(|| server.to_string());
    let port = env_nonempty(ENV_PORT_OVERRIDE)
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(port);
    ATEXIT.call_once(|| unsafe {
        let _ = libc::atexit(cleanup_on_exit);
    });
    RUNTIME.get_or_init(move || {
        // Mitigate the build-time port reservation TOCTOU: another process may
        // have taken the reserved TCP port between build and runtime. If no
        // full endpoint override is in play and the port is not bindable, pick
        // a fresh one. Both sides use this port (the Go server gets it via
        // env), so no build-time constant changes. This runs once, inside the
        // init closure — doing it on every dispatch would see our own live
        // server holding the port and falsely reselect each call.
        let mut port = port;
        if env_nonempty(ENV_ENDPOINT_OVERRIDE).is_none()
            && default_transport() == Transport::Tcp
            && TcpListener::bind(("127.0.0.1", port)).is_err()
        {
            let fresh = free_port();
            eprintln!("rust2go-ipc: reserved port {port} is unavailable; reselecting {fresh}");
            port = fresh;
        }
        let endpoint = resolve_endpoint(port);
        let mut child = spawn_server(&server, port, &endpoint);
        // A Go server that fails to bind exits quickly (e.g. a stale port/socket
        // or a bad binary). Reap and respawn instead of waiting for a timeout.
        // A total deadline bounds startup so a permanently broken server fails
        // fast instead of hanging the very first dispatch call forever.
        let start_deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            if Instant::now() >= start_deadline {
                kill_and_reap(&mut child);
                panic!("rust2go-ipc: Go server never became ready within 30s");
            }
            // Probe, reap and respawn under ONE CHILD_MGMT critical section.
            // try_wait() reaps an exited child immediately; if CHILD_PID were
            // left set past that point, a concurrent atexit hook could kill a
            // recycled pid. connect_with_retry stays outside the lock.
            {
                let _mgmt = lock_or_recover(&CHILD_MGMT);
                let status = match child.try_wait() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("rust2go-ipc: try_wait during startup: {e}");
                        None
                    }
                };
                if let Some(status) = status {
                    eprintln!(
                        "rust2go-ipc: Go server exited during startup ({status}); respawning"
                    );
                    // atexit may have run while we were starting up (RUNTIME not
                    // yet published, so its hook could only kill by pid). Do not
                    // spawn a replacement that would outlive the exiting process.
                    if SHUTTING_DOWN.load(Ordering::SeqCst) {
                        kill_child_locked(&mut child);
                        panic!("rust2go-ipc: shutting down during startup; aborting respawn");
                    }
                    kill_child_locked(&mut child);
                    child = spawn_server_locked(&server, port, &endpoint);
                }
            }
            if let Some(s) = connect_with_retry(&endpoint, Duration::from_millis(500)) {
                break s;
            }
        };
        let mut reader = stream.try_clone().unwrap_or_else(|e| {
            kill_and_reap(&mut child);
            panic!("rust2go-ipc: cannot clone IPC stream: {e}")
        });
        // A read timeout bounds the reactor's blocking read and is the only
        // periodic tick that sweeps expired pending entries. Do not ignore a
        // failure here: fail fast while we can still reap the child.
        if let Err(e) = set_stream_timeouts(&mut stream, &mut reader) {
            kill_and_reap(&mut child);
            panic!("rust2go-ipc: cannot configure IPC stream timeouts: {e}");
        }
        let ttl = env_nonempty("RUST2GO_IPC_PENDING_TTL_SECS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PENDING_TTL);

        // Box::leak gives the reactor a `&'static Runtime` up front, removing
        // the startup spin-wait on OnceLock and letting a spawn failure below
        // reap the child explicitly before unwinding (a plain `Child` drop
        // does not kill/wait, which would orphan the Go server holding the
        // endpoint and make every later init fail at bind).
        let rt: &'static Runtime = Box::leak(Box::new(Runtime {
            server,
            port,
            endpoint,
            child: Mutex::new(Some(child)),
            writer: Mutex::new(stream),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
            pending_ttl: Duration::from_secs(ttl),
        }));

        if let Err(e) = thread::Builder::new()
            .name("rust2go-ipc-reactor".to_string())
            .spawn(move || reactor_loop(reader, rt))
        {
            // Take CHILD_MGMT so every reap runs under the same lock the atexit
            // hook uses, keeping the invariant uniform even though RUNTIME is
            // not published yet here (so no concurrent reap is possible).
            let _mgmt = lock_or_recover(&CHILD_MGMT);
            reap_child(rt);
            panic!("rust2go-ipc: cannot spawn reactor thread: {e}");
        }

        rt
    })
}

/// Drop pending entries whose response never arrived and release their slots.
/// Without an explicit cleanup the async `SlotWriter` (and the request buffer
/// attached to it) would leak forever: no callback will ever consume it.
fn sweep_pending(rt: &Runtime) {
    let now = Instant::now();
    {
        let mut last = lock_or_recover(&rt.last_sweep);
        if now.duration_since(*last) < Duration::from_secs(SWEEP_INTERVAL) {
            return;
        }
        *last = now;
    }
    let stale_ids: Vec<u64> = {
        let pending = lock_or_recover(&rt.pending);
        pending
            .iter()
            .filter(|(_, p)| p.created.elapsed() >= rt.pending_ttl)
            .map(|(id, _)| *id)
            .collect()
    };
    if stale_ids.is_empty() {
        return;
    }
    let stale: Vec<Pending> = {
        let mut pending = lock_or_recover(&rt.pending);
        stale_ids
            .iter()
            .filter_map(|id| pending.remove(id))
            .collect()
    };
    eprintln!("rust2go-ipc: swept {} stale pending entries", stale.len());
    for p in stale {
        cleanup_pending(p);
    }
}

/// Release a pending slot without letting a panic in `Drop` reach the caller.
/// `sweep_pending` runs on the dispatch thread and `drop_all_pending` runs on
/// the reactor — neither may be killed by a user type's panic.
fn cleanup_pending(p: Pending) {
    let _ = catch_unwind(AssertUnwindSafe(|| p.cleanup()));
}

fn connect_with_retry(endpoint: &Endpoint, timeout: Duration) -> Option<IpcStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let result = match endpoint.transport {
            Transport::Tcp => TcpStream::connect(&endpoint.addr).map(IpcStream::Tcp),
            #[cfg(unix)]
            Transport::Unix => UnixStream::connect(&endpoint.addr).map(IpcStream::Unix),
        };
        match result {
            Ok(s) => return Some(s),
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
}

/// Reserve a free loopback port at build time.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("rust2go-ipc: cannot bind probe port")
        .local_addr()
        .expect("rust2go-ipc: cannot read probe port")
        .port()
}

/// Send one async request. Returns immediately; the tokio worker never blocks.
///
/// # Safety
/// `slot` must be a live atomic-slot writer pointer, `cb` a valid generated
/// callback, and `trampoline` a generated response decoder for `method_id`.
/// All three are captured and invoked later on the reactor thread, so they
/// must stay valid until the response arrives or the pending entry expires.
pub unsafe fn dispatch(
    server: &str,
    port: u16,
    method_id: u32,
    payload: &[u8],
    slot: *const (),
    cb: *const (),
    handler: ResponseHandler,
) {
    // Reject oversized payloads BEFORE allocating a req_id or inserting a
    // pending entry: build_frame's assert runs later on the write path, and a
    // panic there would skip the pending cleanup below, leaking the slot until
    // the TTL sweep (10 min). Failing early keeps the pending map untouched.
    // The upstream stub has already handed SlotWriter ownership to us via its
    // raw pointer, so on this early panic we must release it ourselves — else
    // the slot leaks permanently (no pending entry means no sweep/cleanup).
    if payload.len() > MAX_PAYLOAD {
        if let Some(cleanup) = handler.cleanup {
            unsafe { cleanup(slot) };
        }
        panic!("rust2go-ipc: request payload too large ({})", payload.len());
    }
    let rt = runtime(server, port);
    sweep_pending(rt);
    let req_id = next_req_id(rt);

    // Insert pending and write the frame as one critical section. Lock order
    // is writer -> pending everywhere (reconnect uses the same order), and both
    // guards drop at the semicolon BEFORE any cleanup: cleanup wakes the future
    // (SlotWriter::drop), which may re-enter the runtime and try to take either
    // lock again. Holding writer across insert+write also closes the reconnect
    // race where a request was inserted just before the writer swap, drained by
    // reconnect, yet still sent on the new connection.
    // The displaced entry is only a defensive u64-wraparound case (~1.8e19
    // calls); HashMap::insert would otherwise drop it without running cleanup.
    let (displaced, write_failed): (Option<Pending>, Option<Pending>) = {
        let mut writer = lock_or_recover(&rt.writer);
        let mut pending = lock_or_recover(&rt.pending);
        let displaced = pending.insert(
            req_id,
            Pending {
                slot: slot as usize,
                cb: cb as usize,
                trampoline: handler.trampoline,
                cleanup: handler.cleanup,
                created: Instant::now(),
            },
        );
        if write_frame_locked(&mut writer, req_id, method_id, payload) {
            (displaced, None)
        } else {
            // The frame never left: no response can arrive. Drop the pending
            // entry now instead of holding the slot until the TTL sweep.
            (displaced, pending.remove(&req_id))
        }
    };
    if let Some(old) = displaced {
        cleanup_pending(old);
    }
    if let Some(p) = write_failed {
        cleanup_pending(p);
    }
}

/// `req_id = 0` is reserved for oneway frames. Skip it on u64 wraparound.
fn next_req_id(rt: &Runtime) -> u64 {
    let mut id = rt.next_id.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        id = rt.next_id.fetch_add(1, Ordering::Relaxed);
    }
    id
}

/// Synchronous call with a return value. The rust2go macro reads the stack
/// slot immediately after this function returns, so the response must be
/// decoded and the callback invoked before returning. Uses a dedicated
/// connection to avoid racing the global reactor.
///
/// # Safety
/// `slot` points at a stack `Option<T>` owned by the caller; `cb` and
/// `trampoline` are generated functions.
pub unsafe fn dispatch_sync(
    _server: &str,
    port: u16,
    method_id: u32,
    payload: &[u8],
    slot: *const (),
    cb: *const (),
    handler: ResponseHandler,
) {
    let rt = runtime(_server, port);
    let req_id = next_req_id(rt);
    // Dedicated connection to the resolved endpoint: overrides may differ
    // from the build-time constant passed in `port`.
    // A synchronous rust2go stub always does `slot.take().unwrap()` after this
    // call. Returning with the slot unset (and sync handlers have no cleanup)
    // would panic in the macro with a useless "None" message. There is no error
    // channel in the sync signature, so surface a descriptive panic here with
    // the method id and cause; the generated stub is a plain Rust `unsafe fn`,
    // so unwinding reaches the caller safely.
    let Some(mut stream) = connect_with_retry(&rt.endpoint, Duration::from_secs(10)) else {
        panic!("rust2go-ipc: sync call to method {method_id} failed: cannot connect to Go server");
    };
    // A silent timeout failure would leave this blocking read unbounded: the
    // sync stub has no error channel, so a wedged server would hang the caller
    // forever. Fail fast with the same descriptive panic used for the rest of
    // the sync path (startup/reconnect already treat this as fatal).
    if let Err(e) = stream.set_read_timeout(Duration::from_secs(sync_timeout_secs())) {
        panic!("rust2go-ipc: sync call to method {method_id} failed: cannot set read timeout: {e}");
    }
    if let Err(e) = stream.set_write_timeout(Duration::from_secs(30)) {
        panic!(
            "rust2go-ipc: sync call to method {method_id} failed: cannot set write timeout: {e}"
        );
    }
    let frame = build_frame(req_id, method_id, payload);
    if let Err(e) = stream.write_all(&frame).and_then(|_| stream.flush()) {
        panic!("rust2go-ipc: sync call to method {method_id} failed: cannot write request: {e}");
    }

    let (resp_id, status, resp) = match read_response(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            panic!(
                "rust2go-ipc: sync call to method {method_id} failed: cannot read response: {e}"
            );
        }
    };
    if resp_id != req_id {
        panic!(
            "rust2go-ipc: sync call to method {method_id} failed: response id mismatch ({resp_id} != {req_id})"
        );
    }
    if status != 0 {
        panic!("rust2go-ipc: sync call to method {method_id} failed: Go returned error status");
    }
    // SAFETY: `slot`/`cb`/`trampoline` come from the generated stub's safety
    // contract; `resp` is a live owned buffer for the duration of this call.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        (handler.trampoline)(resp.as_ptr(), resp.len(), slot, cb);
    }));
    if let Err(panic) = result {
        if let Some(f) = handler.cleanup {
            unsafe { f(slot) };
        }
        // Sync has no error channel: returning with the slot unset would make
        // the macro's `slot.take().unwrap()` panic with a useless "None" message.
        // Surface a descriptive panic with the method id, keeping the original
        // decode panic text as detail when it is a string payload.
        let detail = if let Some(s) = panic.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        panic!(
            "rust2go-ipc: sync call to method {method_id} (req {req_id}) failed: response decode panicked: {detail}"
        );
    }
}

/// Read exactly `buf.len()` bytes, preserving the reactor's idle-timeout
/// protocol: a timeout before ANY byte of the whole frame is consumed stays a
/// TimedOut/WouldBlock (the reactor probes child liveness and `continue`s). A
/// timeout after a partial read is upgraded to InvalidData, because the stream
/// is now desynchronised and continuing would parse frame bytes as a new header.
///
/// `frame_offset` is the number of frame bytes already consumed by earlier
/// calls (the 19-byte header). It must be passed for the payload segment so a
/// header-then-stall is recognised as mid-frame even if no payload byte landed.
fn read_exact_frame<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    frame_offset: usize,
) -> io::Result<()> {
    let mut off = frame_offset;
    while off < frame_offset + buf.len() {
        match reader.read(&mut buf[off - frame_offset..]) {
            Ok(0) => {
                return Err(IoError::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                ))
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e)
                if (e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock)
                    && off > 0 =>
            {
                // Bytes already consumed: the only safe recovery is a full
                // reconnect. Report InvalidData so the reactor resyncs instead
                // of treating this as an idle tick.
                return Err(IoError::new(
                    ErrorKind::InvalidData,
                    format!("timeout mid-frame after {off} bytes: {e}"),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_response<R: Read>(reader: &mut R) -> io::Result<(u64, u8, Vec<u8>)> {
    let mut header = [0u8; 19];
    read_exact_frame(reader, &mut header, 0)?;
    if header[..4] != MAGIC {
        return Err(IoError::new(ErrorKind::InvalidData, "bad response magic"));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != VERSION {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("unsupported response version {version}"),
        ));
    }
    let req_id = u64::from_le_bytes(header[6..14].try_into().expect("8 bytes"));
    let status = header[14];
    let len = u32::from_le_bytes(header[15..19].try_into().expect("4 bytes")) as usize;
    if len > MAX_PAYLOAD {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("response too large ({len})"),
        ));
    }
    let mut payload = vec![0u8; len];
    // The 19-byte header was already consumed. If the payload stalls before
    // its first byte, that is still mid-frame (not an idle tick): passing 19
    // makes the timeout upgrade to InvalidData so the reactor resyncs.
    read_exact_frame(reader, &mut payload, 19)?;
    Ok((req_id, status, payload))
}

/// Fire-and-forget call: no pending entry, no response expected.
///
/// Safe: nothing captures caller pointers and no response is decoded. The Go
/// server must be running for the frame to be delivered, but that is a
/// liveness concern, not a memory-safety one.
pub fn dispatch_oneway(server: &str, port: u16, method_id: u32, payload: &[u8]) {
    // Mirror the oversized-payload guard of `dispatch`/`dispatch_sync`, but run
    // it BEFORE `runtime()` so the panic cannot happen while holding the global
    // writer mutex (build_frame's assert would otherwise poison it). oneway has
    // no slot to clean up, so there is no further bookkeeping to undo.
    assert!(
        payload.len() <= MAX_PAYLOAD,
        "rust2go-ipc: request payload too large ({})",
        payload.len()
    );
    let rt = runtime(server, port);
    let _ = write_frame(rt, 0, method_id, payload);
}

fn build_frame(req_id: u64, method_id: u32, payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() <= MAX_PAYLOAD,
        "rust2go-ipc: request payload too large ({})",
        payload.len()
    );
    let mut frame = Vec::with_capacity(22 + payload.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&VERSION.to_le_bytes());
    frame.extend_from_slice(&req_id.to_le_bytes());
    frame.extend_from_slice(&method_id.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Write a request frame. Write/flush failures are logged rather than panicked:
/// panicking here would poison the writer mutex and (via the reactor's reconnect
/// path) kill the only recovery thread. On failure the stream is shut down so
/// the reactor's cloned reader sees EOF and rebuilds both ends; merely returning
/// false is not enough, because a partial frame would desynchronise the Go read
/// loop while the reactor (which only probes child liveness on idle) keeps the
/// broken connection. In-flight futures are released by `drop_all_pending`.
fn write_frame(rt: &Runtime, req_id: u64, method_id: u32, payload: &[u8]) -> bool {
    let mut writer = lock_or_recover(&rt.writer);
    write_frame_locked(&mut writer, req_id, method_id, payload)
}

/// Write a frame while the caller already holds `rt.writer`.
///
/// Dispatch uses this to make "insert pending" and "write frame" one critical
/// section with reconnect's writer swap. Otherwise reconnect could drain
/// pending between the insert and the write, wake the future with an error and
/// still send the request on the new connection.
fn write_frame_locked(writer: &mut IpcStream, req_id: u64, method_id: u32, payload: &[u8]) -> bool {
    let frame = build_frame(req_id, method_id, payload);
    match writer.write_all(&frame).and_then(|_| writer.flush()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "rust2go-ipc: cannot write request frame (req {req_id}): {e}; closing connection"
            );
            writer.shutdown_both();
            false
        }
    }
}

/// Take every pending entry out of the map without cleaning it up. The caller
/// must run `cleanup_pending` for each entry after releasing the locks it holds
/// (cleanup wakes futures and may re-enter the runtime).
fn take_all_pending(rt: &Runtime) -> Vec<Pending> {
    lock_or_recover(&rt.pending)
        .drain()
        .map(|(_, p)| p)
        .collect()
}

/// Remove every pending entry and release its slot. Called when the transport
/// dies: no late response can arrive on a dead connection.
fn drop_all_pending(rt: &Runtime) {
    for p in take_all_pending(rt) {
        cleanup_pending(p);
    }
}

/// Re-establish the connection: rebuild the socket pair, and restart the Go
/// child only if it has actually exited. The child handle is stored in
/// `rt.child` immediately after spawning so a panic in this function can never
/// orphan it. Returns false only on recoverable connect failures.
fn reconnect(rt: &Runtime, reader: &mut IpcStream) -> bool {
    // All child reap/spawn happens under CHILD_MGMT so the atexit hook cannot
    // interleave: after it sets SHUTTING_DOWN we refuse to spawn a new child.
    // A dropped connection does NOT imply a dead child — the Go serve loop may
    // have closed an idle/half-open connection while the process is healthy.
    // Reaping it here would discard in-process state such as a one-way Init.
    // Probe under the same lock as reap so the atexit hook cannot reap between
    // our probe and a spawn decision.
    {
        let _mgmt = lock_or_recover(&CHILD_MGMT);
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return false;
        }
        let alive = lock_or_recover(&rt.child)
            .as_mut()
            .map(|c| matches!(c.try_wait(), Ok(None)))
            .unwrap_or(false);
        if !alive {
            reap_child(rt);
            // CHILD_MGMT is already held above: use the caller-must-hold-lock
            // variant, otherwise spawn_server's internal lock re-enters the
            // same non-reentrant Mutex and deadlocks the reactor (and the
            // atexit hook waiting on it).
            let child = spawn_server_locked(&rt.server, rt.port, &rt.endpoint);
            *lock_or_recover(&rt.child) = Some(child);
        }
    }

    // A live child whose endpoint is temporarily unreachable (not listening
    // yet, socket removed, backlog full) must not trap us in an endless
    // reconnect. Bound the attempts for this rebuild; on expiry force a
    // restart so the caller's retry loop re-enters with a fresh child.
    // Without the bound, switching from "always respawn" to "reuse a live
    // child" could hang forever on a child that never comes back to listen.
    let connect_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // If the child died before listening, bail out so the caller's
        // retry loop re-enters and spawns again (child management stays in one
        // place, under CHILD_MGMT).
        {
            let alive = lock_or_recover(&rt.child)
                .as_mut()
                .map(|c| matches!(c.try_wait(), Ok(None)))
                .unwrap_or(false);
            if !alive || SHUTTING_DOWN.load(Ordering::SeqCst) {
                return false;
            }
        }
        if Instant::now() >= connect_deadline {
            eprintln!("rust2go-ipc: child alive but not accepting; forcing restart");
            let _mgmt = lock_or_recover(&CHILD_MGMT);
            if SHUTTING_DOWN.load(Ordering::SeqCst) {
                return false;
            }
            reap_child(rt);
            // CHILD_MGMT already held above (line 976): use the locked variant
            // to avoid re-entering the non-reentrant Mutex.
            let child = spawn_server_locked(&rt.server, rt.port, &rt.endpoint);
            *lock_or_recover(&rt.child) = Some(child);
            return false; // caller's loop re-enters and connects to the fresh child
        }
        if let Some(mut stream) = connect_with_retry(&rt.endpoint, Duration::from_millis(500)) {
            match stream.try_clone() {
                Ok(mut r) => {
                    if let Err(e) = set_stream_timeouts(&mut stream, &mut r) {
                        eprintln!("rust2go-ipc: cannot set reconnected stream timeouts: {e}");
                        return false;
                    }
                    // Swap both ends while holding the writer lock, and drain
                    // pending in the same critical section. The reactor already
                    // called drop_all_pending before entering reconnect, but a
                    // dispatch may have inserted a request during connect; its
                    // frame went to the old stream, whose reader is about to be
                    // dropped. Cleanup must happen AFTER releasing the writer
                    // lock because it wakes futures that can re-enter dispatch.
                    let stale = {
                        let mut writer = lock_or_recover(&rt.writer);
                        let stale = take_all_pending(rt);
                        *writer = stream;
                        *reader = r;
                        stale
                    };
                    for p in stale {
                        cleanup_pending(p);
                    }
                    return true;
                }
                Err(e) => eprintln!("rust2go-ipc: cannot clone reconnected stream: {e}"),
            }
        }
        sleep(Duration::from_millis(200));
    }
}

fn reactor_loop(mut reader: IpcStream, rt: &'static Runtime) {
    loop {
        let (req_id, status, payload) = match read_response(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
                    // Idle read timeout: probe child liveness. A healthy but
                    // quiet server must not be killed just because no response
                    // frame arrived during the window.
                    let alive = lock_or_recover(&rt.child)
                        .as_mut()
                        .map(|c| matches!(c.try_wait(), Ok(None)))
                        .unwrap_or(false);
                    if alive {
                        // The idle read timeout is the only periodic tick a
                        // quiet reactor gets. Reclaim timed-out pending slots
                        // here so a lost response followed by a long idle
                        // period cannot leak the SlotWriter and its request
                        // attachment. sweep_pending is internally throttled.
                        sweep_pending(rt);
                        continue;
                    }
                }
                // A malformed frame (InvalidData) means the stream is
                // desynchronised: the 19-byte header was consumed but its
                // payload was not, so `continue` would read from the middle of
                // a frame forever. Like a disconnect, the only way to resync
                // is to rebuild the connection.
                if e.kind() == ErrorKind::InvalidData {
                    eprintln!("rust2go-ipc: invalid response frame ({e}); resyncing connection");
                } else {
                    eprintln!("rust2go-ipc: connection lost ({e}); restarting Go server");
                }
                drop_all_pending(rt);
                // The only recovery thread must survive a panic in reconnect.
                loop {
                    match catch_unwind(AssertUnwindSafe(|| reconnect(rt, &mut reader))) {
                        Ok(true) => break,
                        Ok(false) => eprintln!("rust2go-ipc: reconnect failed; retrying"),
                        Err(_) => eprintln!("rust2go-ipc: reconnect panicked; retrying"),
                    }
                    sleep(Duration::from_secs(1));
                }
                continue;
            }
        };

        let pending = lock_or_recover(&rt.pending).remove(&req_id);

        match pending {
            Some(p) if status == 0 => {
                let slot = p.slot;
                let cb = p.cb;
                let trampoline = p.trampoline;
                let cleanup = p.cleanup;
                // A malformed response must not kill the reactor thread.
                // SAFETY: slot/cb are usize round-trips of the pointers
                // captured in dispatch(); trampoline is a generated fn in
                // this binary.
                let result = catch_unwind(AssertUnwindSafe(|| unsafe {
                    trampoline(
                        payload.as_ptr(),
                        payload.len(),
                        slot as *const (),
                        cb as *const (),
                    );
                }));
                if result.is_err() {
                    eprintln!("rust2go-ipc: response trampoline panicked for req {req_id}");
                    if let Some(f) = cleanup {
                        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { f(slot as *const ()) }));
                    }
                }
            }
            Some(p) => {
                eprintln!("rust2go-ipc: Go returned error status for req {req_id}");
                cleanup_pending(p);
            }
            None => eprintln!("rust2go-ipc: response for unknown req {req_id}"),
        }
    }
}

/// Public mirror of rust2go's opaque `StringRef`. Layout-identical
/// (`ptr`, `len`) so transmute to the derive-generated Ref type is sound.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RawStringRef {
    pub ptr: *const u8,
    pub len: usize,
}

/// Public mirror of rust2go's opaque `ListRef`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct RawListRef {
    pub ptr: *const (),
    pub len: usize,
}

/// Byte sink used by generated `flatten_*` functions.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prim<T: WirePrim>(&mut self, v: T) {
        v.write(&mut self.buf);
    }

    pub fn string(&mut self, s: RawStringRef) {
        let len = s.len;
        self.prim(len as u64);
        if len == 0 {
            return;
        }
        let bytes = unsafe { from_raw_parts(s.ptr, len) };
        self.buf.extend_from_slice(bytes);
    }

    pub fn list_header(&mut self, len: usize) {
        self.prim(len as u64);
    }

    /// Write a primitive list as `[u64 len][len little-endian T values]`.
    pub fn prim_list<T: WirePrim>(&mut self, ptr: *const (), len: usize) {
        self.prim(len as u64);
        if len == 0 {
            return;
        }
        let nbytes = len.checked_mul(size_of::<T>()).unwrap_or_else(|| {
            panic!(
                "rust2go-ipc: primitive list size overflow ({len} x {})",
                size_of::<T>()
            )
        });
        assert!(
            nbytes <= isize::MAX as usize,
            "rust2go-ipc: primitive list too large ({nbytes})"
        );
        // SAFETY: ptr comes from an upstream `ListRef` for `Vec<T>`, i.e. a
        // `Vec<T>::as_ptr()` with T alignment and `len` initialised elements.
        let items = unsafe { from_raw_parts(ptr as *const T, len) };
        for &item in items {
            item.write(&mut self.buf);
        }
    }

    pub fn take(self) -> Vec<u8> {
        self.buf
    }
}

/// Owns every allocation a restored Ref graph points into. Kept alive by the
/// generated trampoline until the rust2go callback has copied data into owned
/// Rust values.
#[derive(Default)]
pub struct Arena {
    owned: Vec<Box<dyn Any>>,
}

impl Arena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy bytes into a stable allocation, returning (ptr, len).
    pub fn bytes(&mut self, bytes: &[u8]) -> (*const u8, usize) {
        let boxed: Box<[u8]> = bytes.into();
        let ptr = boxed.as_ptr();
        let len = boxed.len();
        self.owned.push(Box::new(boxed) as Box<dyn Any>);
        (ptr, len)
    }

    /// Copy a typed element array into a stable allocation.
    pub fn elements<T: Copy + 'static>(&mut self, elems: &[T]) -> *const T {
        let boxed: Box<[T]> = elems.into();
        let ptr = boxed.as_ptr();
        self.owned.push(Box::new(boxed) as Box<dyn Any>);
        ptr
    }
}

/// Cursor over a flat payload, paired with an [`Arena`] for restored pointers.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: u32,
    pub arena: Arena,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            depth: 0,
            arena: Arena::new(),
        }
    }

    /// Enter one level of struct/Ref restoration. Generated `restore` helpers
    /// call this on entry and [`leave`](Self::leave) on return; a self-referential
    /// schema or a hostile frame cannot recurse past [`MAX_RESTORE_DEPTH`] and
    /// overflow the stack (a fatal abort that `catch_unwind` cannot intercept).
    pub fn enter(&mut self) {
        self.depth += 1;
        assert!(
            self.depth <= MAX_RESTORE_DEPTH,
            "rust2go-ipc: restore nesting exceeds {MAX_RESTORE_DEPTH} levels"
        );
    }

    /// Leave one restoration level. Paired with [`enter`](Self::enter).
    pub fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Consume `n` bytes with checked arithmetic and an explicit bounds check.
    /// Malformed/corrupt frames panic with a clear message instead of wrapping
    /// an offset or slicing out of range; the reactor's C-unwind trampoline
    /// turns that panic into a logged, dropped pending entry.
    fn take(&mut self, n: usize) -> &'a [u8] {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .unwrap_or_else(|| {
                panic!(
                    "rust2go-ipc: decode out of bounds (pos {}, need {n}, have {})",
                    self.pos,
                    self.buf.len()
                )
            });
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        bytes
    }

    pub fn prim<T: WirePrim>(&mut self) -> T {
        let bytes = self.take(size_of::<T>());
        T::read(bytes)
    }

    pub fn string(&mut self) -> RawStringRef {
        // Validate in the wire's u64 domain BEFORE narrowing to usize. A
        // corrupt/forged frame carrying len > usize::MAX on a 32-bit target
        // would otherwise wrap to a small value and desynchronise every
        // subsequent field (checked take() prevents OOB but not wrong data).
        let len64 = self.prim::<u64>();
        assert!(
            len64 <= self.buf.len() as u64,
            "rust2go-ipc: string length {len64} exceeds remaining bytes"
        );
        let len = len64 as usize;
        if len == 0 {
            return RawStringRef {
                ptr: null(),
                len: 0,
            };
        }
        let bytes = self.take(len);
        let (ptr, len) = self.arena.bytes(bytes);
        RawStringRef { ptr, len }
    }

    /// Read a list length and bound it by the minimum wire width of one
    /// element, so a corrupt count cannot drive an unbounded allocation.
    pub fn list_len_min(&mut self, min_width: usize) -> usize {
        // Same u64-domain validation as string(): narrow only after proving
        // the count fits both usize and the remaining wire bytes, so a 32-bit
        // target cannot wrap a huge count into a passing bounds check.
        let n64 = self.prim::<u64>();
        let max = self.buf.len().saturating_sub(self.pos) / min_width.max(1);
        assert!(
            n64 <= max as u64,
            "rust2go-ipc: list length {n64} exceeds {max} remaining elements"
        );
        n64 as usize
    }

    /// Bound by the size of the restored Ref element (`RawStringRef` / custom
    /// struct Ref), which is what `ref_list` allocates.
    pub fn list_len_of<T: Copy + 'static>(&mut self) -> usize {
        self.list_len_min(size_of::<T>())
    }

    pub fn list_len(&mut self) -> usize {
        self.list_len_min(1)
    }

    pub fn list_elements<T: WirePrim>(&mut self, n: usize) -> RawListRef {
        if n == 0 {
            // Match prim_list/ref_list/string: an empty list has a null pointer,
            // not the dangling-but-aligned pointer of an empty Box<[T]>.
            return RawListRef {
                ptr: null(),
                len: 0,
            };
        }
        // Bound the element count against the remaining bytes first: a single
        // corrupt u64 length must not drive an unbounded allocation loop.
        let max = self.buf.len().saturating_sub(self.pos) / size_of::<T>().max(1);
        assert!(
            n <= max,
            "rust2go-ipc: list length {n} exceeds {max} remaining elements"
        );
        let items: Vec<T> = (0..n).map(|_| self.prim::<T>()).collect();
        let ptr = self.arena.elements(&items);
        RawListRef {
            ptr: ptr as *const (),
            len: n,
        }
    }

    /// Read a primitive list written by [`Encoder::prim_list`].
    pub fn prim_list<T: WirePrim>(&mut self) -> RawListRef {
        // Guard by the element's wire width, not a blanket 1 byte: a corrupt
        // count in (remaining/size_of::<T>(), remaining] would otherwise pass
        // the guard and make take(n * size) panic deep inside the decoder.
        let n = self.list_len_min(size_of::<T>());
        if n == 0 {
            return RawListRef {
                ptr: null(),
                len: 0,
            };
        }
        let nbytes = n.checked_mul(size_of::<T>()).unwrap_or_else(|| {
            panic!(
                "rust2go-ipc: primitive list size overflow ({n} x {})",
                size_of::<T>()
            )
        });
        let bytes = self.take(nbytes);
        // Validate bit patterns (bool) before any consumer reads them.
        T::validate(bytes);
        // Use arena.elements so the allocation is aligned to T. A Box<[u8]>
        // (align 1) would be UB when the consumer reads it as *const T for
        // types with alignment > 1 (i64/f64/...). Parse each element through
        // WirePrim::read so the byte view becomes a real, aligned T array.
        let width = size_of::<T>();
        let items: Vec<T> = (0..n)
            .map(|i| T::read(&bytes[i * width..(i + 1) * width]))
            .collect();
        let ptr = self.arena.elements(&items);
        RawListRef {
            ptr: ptr as *const (),
            len: n,
        }
    }

    /// Allocate a restored Ref-element array in the arena, returning a
    /// `ListRef` whose `ptr` points at it.
    pub fn ref_list<T: Copy + 'static>(&mut self, items: Vec<T>) -> RawListRef {
        let n = items.len();
        if n == 0 {
            return RawListRef {
                ptr: null(),
                len: 0,
            };
        }
        let ptr = self.arena.elements(&items);
        RawListRef {
            ptr: ptr as *const (),
            len: n,
        }
    }
}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
