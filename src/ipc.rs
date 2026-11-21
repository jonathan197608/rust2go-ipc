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
    hint::spin_loop,
    io::{self, Error as IoError, ErrorKind, Read, Write},
    mem::size_of,
    net::{TcpListener, TcpStream},
    panic::{catch_unwind, AssertUnwindSafe},
    process::{Child, Command, Stdio},
    ptr::{null, read_unaligned},
    slice::from_raw_parts,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    thread::{self, sleep},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::id as process_id;

const MAGIC: [u8; 4] = *b"R2GI";
const VERSION: u16 = 1;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const DEFAULT_PENDING_TTL: u64 = 600;
const SWEEP_INTERVAL: u64 = 30;

const ENV_SERVER_OVERRIDE: &str = "RUST2GO_IPC_SERVER_OVERRIDE";
const ENV_ENDPOINT_OVERRIDE: &str = "RUST2GO_IPC_ENDPOINT_OVERRIDE";
const ENV_TRANSPORT_OVERRIDE: &str = "RUST2GO_IPC_TRANSPORT_OVERRIDE";
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

/// Resolve the endpoint shared by the Rust client and the Go server.
/// Overrides: full endpoint (plus optional transport), or just the port.
fn resolve_endpoint(port: u16) -> Endpoint {
    if let Some(addr) = env_nonempty(ENV_ENDPOINT_OVERRIDE) {
        let transport = match env_nonempty(ENV_TRANSPORT_OVERRIDE).as_deref() {
            Some("tcp") => Transport::Tcp,
            #[cfg(unix)]
            Some("unix") => Transport::Unix,
            _ => default_transport(),
        };
        return Endpoint { transport, addr };
    }
    match default_transport() {
        #[cfg(unix)]
        Transport::Unix => {
            let mut path =
                env::temp_dir().join(format!("rust2go-ipc-{}-{}.sock", process_id(), port));
            // sockaddr_un.sun_path is ~108 bytes on Linux / 104 on macOS.
            // Long TMPDIR paths (common in CI) would fail to bind.
            if path.to_string_lossy().len() >= 100 {
                path = PathBuf::from(format!("/tmp/rust2go-ipc-{}-{}.sock", process_id(), port));
            }
            Endpoint {
                transport: Transport::Unix,
                addr: path.to_string_lossy().into_owned(),
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

macro_rules! impl_wire_prim_plain {
    ($($t:ty),+ $(,)?) => {
        $(
            impl sealed::Sealed for $t {}
            impl WirePrim for $t {
                fn read(bytes: &[u8]) -> Self {
                    debug_assert_eq!(bytes.len(), size_of::<Self>());
                    // SAFETY: $t is a fixed-width primitive with no padding;
                    // every bit pattern is valid. `bytes` has exactly
                    // size_of::<$t>() bytes, sliced by Decoder::prim.
                    unsafe { read_unaligned(bytes.as_ptr() as *const $t) }
                }
                fn write(self, out: &mut Vec<u8>) {
                    // SAFETY: $t has no padding, so the full byte range of the
                    // local `self` is initialised and safe to read.
                    let bytes =
                        unsafe { from_raw_parts(&self as *const $t as *const u8, size_of::<$t>()) };
                    out.extend_from_slice(bytes);
                }
            }
        )+
    };
}

impl_wire_prim_plain!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl sealed::Sealed for bool {}
impl WirePrim for bool {
    fn read(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), 1);
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
    created: Instant,
}

struct Runtime {
    server: String,
    port: u16,
    endpoint: Endpoint,
    child: Mutex<Option<Child>>,
    writer: Mutex<IpcStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    last_sweep: AtomicU64,
    pending_ttl: Duration,
}

// Runtime is Send + Sync without manual impls: slot/cb addresses are stored as
// `usize`, and Trampoline (a fn pointer) is Send + Sync. The remaining fields
// are Mutex/Atomic/String/Child, all thread-safe.

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Spawn the Go server once and start the reactor. `server` is the path to the
/// compiled Go IPC binary; `port` was reserved by the build script.
fn spawn_server(server: &str, port: u16, endpoint: &Endpoint) -> Child {
    Command::new(server)
        .env("RUST2GO_IPC_PORT", port.to_string())
        .env("RUST2GO_IPC_TRANSPORT", endpoint.transport.as_str())
        .env("RUST2GO_IPC_ENDPOINT", &endpoint.addr)
        .stdin(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("rust2go-ipc: cannot spawn Go server {server}: {e}"))
}

/// Recover a Mutex after a panic in a previous holder. A poisoning panic must
/// never cascade into the reactor (the only recovery mechanism): returning the
/// inner guard keeps the service alive. All protected state is kept consistent
/// by the callers even on the error paths.
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn runtime(server: &str, port: u16) -> &'static Runtime {
    // Build-time paths live under OUT_DIR and do not exist in a stripped
    // runtime image; let deployment point at the copied binary.
    let server = env_nonempty(ENV_SERVER_OVERRIDE).unwrap_or_else(|| server.to_string());
    let port = env_nonempty(ENV_PORT_OVERRIDE)
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(port);
    let endpoint = resolve_endpoint(port);
    RUNTIME.get_or_init(move || {
        let mut child = spawn_server(&server, port, &endpoint);
        let stream = connect_with_retry(&endpoint, Duration::from_secs(10)).unwrap_or_else(|| {
            // Reap the child before panicking so a failed start leaves no zombie.
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "rust2go-ipc: Go server never listened on {} {}",
                endpoint.transport.as_str(),
                endpoint.addr
            )
        });
        let reader = stream
            .try_clone()
            .expect("rust2go-ipc: cannot clone IPC stream");
        let ttl = env::var("RUST2GO_IPC_PENDING_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PENDING_TTL);

        let rt = Runtime {
            server,
            port,
            endpoint,
            child: Mutex::new(Some(child)),
            writer: Mutex::new(stream),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            last_sweep: AtomicU64::new(now_secs()),
            pending_ttl: Duration::from_secs(ttl),
        };

        thread::Builder::new()
            .name("rust2go-ipc-reactor".to_string())
            .spawn(move || {
                let rt = loop {
                    if let Some(rt) = RUNTIME.get() {
                        break rt;
                    }
                    spin_loop();
                };
                reactor_loop(reader, rt);
            })
            .expect("rust2go-ipc: cannot spawn reactor thread");

        rt
    })
}

/// Drop pending entries whose response never arrived. Their slots are still
/// safe: rust2go's atomic slot releases itself when a late callback observes
/// a dropped reader. This only bounds the bookkeeping table.
fn sweep_pending(rt: &Runtime) {
    let now = now_secs();
    let last = rt.last_sweep.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SWEEP_INTERVAL
        || rt
            .last_sweep
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let mut pending = lock_or_recover(&rt.pending);
    let before = pending.len();
    pending.retain(|_, p| p.created.elapsed() < rt.pending_ttl);
    let removed = before - pending.len();
    if removed > 0 {
        eprintln!("rust2go-ipc: swept {removed} stale pending entries");
    }
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
    trampoline: Trampoline,
) {
    let rt = runtime(server, port);
    sweep_pending(rt);
    let req_id = rt.next_id.fetch_add(1, Ordering::Relaxed);

    lock_or_recover(&rt.pending).insert(
        req_id,
        Pending {
            slot: slot as usize,
            cb: cb as usize,
            trampoline,
            created: Instant::now(),
        },
    );

    write_frame(rt, req_id, method_id, payload);
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
    trampoline: Trampoline,
) {
    let rt = runtime(_server, port);
    let req_id = rt.next_id.fetch_add(1, Ordering::Relaxed);
    // Dedicated connection to the resolved endpoint: overrides may differ
    // from the build-time constant passed in `port`.
    let mut stream = connect_with_retry(&rt.endpoint, Duration::from_secs(10))
        .expect("rust2go-ipc: cannot connect for synchronous call");
    let frame = build_frame(req_id, method_id, payload);
    stream
        .write_all(&frame)
        .expect("rust2go-ipc: cannot write sync request");
    stream
        .flush()
        .expect("rust2go-ipc: cannot flush sync request");

    let (resp_id, status, resp) =
        read_response(&mut stream).expect("rust2go-ipc: cannot read sync response");
    assert_eq!(resp_id, req_id, "rust2go-ipc: sync response id mismatch");
    assert_eq!(status, 0, "rust2go-ipc: Go returned error for sync call");
    // SAFETY: `slot`/`cb`/`trampoline` come from the generated stub's safety
    // contract; `resp` is a live owned buffer for the duration of this call.
    unsafe {
        trampoline(resp.as_ptr(), resp.len(), slot, cb);
    }
}

fn read_response<R: Read>(reader: &mut R) -> io::Result<(u64, u8, Vec<u8>)> {
    let mut header = [0u8; 19];
    reader.read_exact(&mut header)?;
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
    reader.read_exact(&mut payload)?;
    Ok((req_id, status, payload))
}

/// Fire-and-forget call: no pending entry, no response expected.
///
/// Safe: nothing captures caller pointers and no response is decoded. The Go
/// server must be running for the frame to be delivered, but that is a
/// liveness concern, not a memory-safety one.
pub fn dispatch_oneway(server: &str, port: u16, method_id: u32, payload: &[u8]) {
    let rt = runtime(server, port);
    write_frame(rt, 0, method_id, payload);
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
/// path) kill the only recovery thread. The reactor detects the dead connection
/// through its read side and respawns the server; in-flight futures time out via
/// the pending TTL.
fn write_frame(rt: &Runtime, req_id: u64, method_id: u32, payload: &[u8]) {
    let frame = build_frame(req_id, method_id, payload);
    let mut writer = lock_or_recover(&rt.writer);
    if let Err(e) = writer.write_all(&frame).and_then(|_| writer.flush()) {
        eprintln!("rust2go-ipc: cannot write request frame (req {req_id}): {e}");
    }
}

fn reactor_loop(mut reader: IpcStream, rt: &'static Runtime) {
    loop {
        let (req_id, status, payload) = match read_response(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("rust2go-ipc: connection lost ({e}); restarting Go server");
                // In-flight futures stay pending until their caller timeout;
                // a late response can never reach a stale slot because the
                // connection is gone.
                lock_or_recover(&rt.pending).clear();
                if let Some(mut child) = lock_or_recover(&rt.child).take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                // Keep one child handle across retries. Spawning on every
                // failed connect leaks processes and races for the port.
                let mut child = spawn_server(&rt.server, rt.port, &rt.endpoint);
                loop {
                    if let Some(status) = child.try_wait().expect("rust2go-ipc: try_wait failed") {
                        eprintln!(
                            "rust2go-ipc: Go server exited during reconnect ({status}); respawning"
                        );
                        child = spawn_server(&rt.server, rt.port, &rt.endpoint);
                    }
                    match connect_with_retry(&rt.endpoint, Duration::from_secs(2)) {
                        Some(stream) => {
                            reader = stream
                                .try_clone()
                                .expect("rust2go-ipc: cannot clone reconnected stream");
                            *lock_or_recover(&rt.writer) = stream;
                            *lock_or_recover(&rt.child) = Some(child);
                            break;
                        }
                        None => {
                            eprintln!("rust2go-ipc: reconnect failed; retrying in 1s");
                            sleep(Duration::from_secs(1));
                        }
                    }
                }
                continue;
            }
        };

        let pending = lock_or_recover(&rt.pending).remove(&req_id);

        match pending {
            Some(p) if status == 0 => {
                // A malformed response must not kill the reactor thread.
                // SAFETY: p.slot/p.cb are the usize round-trips of the pointers
                // captured in dispatch(); their validity is that call's safety
                // contract. trampoline is a generated fn in this binary.
                let result = catch_unwind(AssertUnwindSafe(|| unsafe {
                    (p.trampoline)(
                        payload.as_ptr(),
                        payload.len(),
                        p.slot as *const (),
                        p.cb as *const (),
                    );
                }));
                if result.is_err() {
                    eprintln!("rust2go-ipc: response trampoline panicked for req {req_id}");
                }
            }
            Some(_) => eprintln!("rust2go-ipc: Go returned error status for req {req_id}"),
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

    /// Write a primitive list as `[u64 len][len * size_of::<T>() raw bytes]`.
    pub fn prim_list<T: WirePrim>(&mut self, ptr: *const (), len: usize) {
        self.prim(len as u64);
        if len == 0 {
            return;
        }
        // SAFETY: the list pointer/length come from a rust2go `ListRef` whose
        // elements are T; WirePrim guarantees T is padding-free so the byte
        // range is fully initialised. Validity/lifetime is the stub's contract.
        let bytes = unsafe { from_raw_parts(ptr as *const u8, len * size_of::<T>()) };
        self.buf.extend_from_slice(bytes);
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
    pub arena: Arena,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            arena: Arena::new(),
        }
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
        let len = self.prim::<u64>() as usize;
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

    pub fn list_len(&mut self) -> usize {
        self.prim::<u64>() as usize
    }

    pub fn list_elements<T: WirePrim>(&mut self, n: usize) -> RawListRef {
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
        let n = self.list_len();
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
mod tests {
    use super::*;
    use std::mem::align_of;

    #[test]
    #[should_panic(expected = "decode out of bounds")]
    fn decoder_prim_panics_on_truncation() {
        let mut dec = Decoder::new(&[0x01, 0x02]);
        let _ = dec.prim::<u64>();
    }

    #[test]
    #[should_panic(expected = "invalid bool byte")]
    fn decoder_rejects_invalid_bool() {
        let mut dec = Decoder::new(&[0x02]);
        let _ = dec.prim::<bool>();
    }

    #[test]
    fn decoder_accepts_valid_bool() {
        let mut dec = Decoder::new(&[0x00, 0x01]);
        assert!(!dec.prim::<bool>());
        assert!(dec.prim::<bool>());
    }

    #[test]
    #[should_panic(expected = "decode out of bounds")]
    fn decoder_string_rejects_huge_len() {
        // u64 len = 0xFFFF_FFFF_FFFF_FFFF, no payload follows.
        let mut dec = Decoder::new(&[0xff; 8]);
        let _ = dec.string();
    }

    #[test]
    fn decoder_prim_list_is_aligned() {
        // 2 x i64 = 16 bytes. The restored pointer must be 8-byte aligned.
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u64.to_le_bytes());
        buf.extend_from_slice(&1i64.to_le_bytes());
        buf.extend_from_slice(&2i64.to_le_bytes());
        let mut dec = Decoder::new(&buf);
        let list = dec.prim_list::<i64>();
        assert_eq!(list.len, 2);
        assert_eq!(list.ptr as usize % align_of::<i64>(), 0);
        // SAFETY: prim_list restored an aligned, owned i64 array in the arena.
        let vals = unsafe { from_raw_parts(list.ptr as *const i64, list.len) };
        assert_eq!(vals, &[1i64, 2i64]);
    }

    #[test]
    #[should_panic(expected = "list length")]
    fn decoder_list_elements_rejects_huge_count() {
        let mut dec = Decoder::new(&[0xff; 8]);
        // n = u64::MAX elements, no payload -> must be rejected before allocating
        let _ = dec.list_elements::<u64>(u64::MAX as usize);
    }
}
