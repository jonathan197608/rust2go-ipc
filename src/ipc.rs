//! Minimal IPC runtime for `rust2go-ipc`.
//!
//! Generated binding stubs call [`dispatch`] for async calls. A reactor thread
//! reads responses and invokes a per-method trampoline generated alongside the
//! stub; that trampoline restores the Ref structure from flat bytes and calls
//! the original rust2go `*_cb` function, which writes into the atomic slot and
//! wakes the awaiting future.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const MAGIC: [u8; 4] = *b"R2GI";
const VERSION: u16 = 1;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const DEFAULT_PENDING_TTL: u64 = 600;
const SWEEP_INTERVAL: u64 = 30;

/// Signature of a generated response trampoline:
/// `(payload_ptr, payload_len, slot, cb)`.
pub type Trampoline = unsafe extern "C" fn(*const u8, usize, *const (), *const ());

struct Pending {
    slot: *const (),
    cb: *const (),
    trampoline: Trampoline,
    created: Instant,
}

struct Runtime {
    server: String,
    port: u16,
    child: Mutex<Option<Child>>,
    writer: Mutex<TcpStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    last_sweep: AtomicU64,
    pending_ttl: Duration,
}

// SAFETY: slot pointers are owned by atomic slots whose writer is kept alive
// until the callback runs; cb and trampoline are `#[no_mangle]` functions in
// the Rust binary. Each pending entry is removed and consumed exactly once.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Spawn the Go server once and start the reactor. `server` is the path to the
/// compiled Go IPC binary; `port` was reserved by the build script.
fn spawn_server(server: &str, port: u16) -> Child {
    Command::new(server)
        .env("RUST2GO_IPC_PORT", port.to_string())
        .stdin(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("rust2go-ipc: cannot spawn Go server {server}: {e}"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn runtime(server: &str, port: u16) -> &'static Runtime {
    // Build-time paths live under OUT_DIR and do not exist in a stripped
    // runtime image; let deployment point at the copied binary.
    let server = std::env::var("RUST2GO_IPC_SERVER_OVERRIDE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| server.to_string());
    let port = std::env::var("RUST2GO_IPC_PORT_OVERRIDE")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(port);
    RUNTIME.get_or_init(move || {
        let child = spawn_server(&server, port);
        let stream = connect_with_retry(port, Duration::from_secs(10))
            .unwrap_or_else(|| panic!("rust2go-ipc: Go server never listened on port {port}"));
        let reader = stream
            .try_clone()
            .expect("rust2go-ipc: cannot clone IPC stream");
        let ttl = std::env::var("RUST2GO_IPC_PENDING_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PENDING_TTL);

        let rt = Runtime {
            server,
            port,
            child: Mutex::new(Some(child)),
            writer: Mutex::new(stream),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            last_sweep: AtomicU64::new(now_secs()),
            pending_ttl: Duration::from_secs(ttl),
        };

        std::thread::Builder::new()
            .name("rust2go-ipc-reactor".to_string())
            .spawn(move || {
                let rt = loop {
                    if let Some(rt) = RUNTIME.get() {
                        break rt;
                    }
                    std::hint::spin_loop();
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
    let mut pending = rt
        .pending
        .lock()
        .expect("rust2go-ipc: pending lock poisoned");
    let before = pending.len();
    pending.retain(|_, p| p.created.elapsed() < rt.pending_ttl);
    let removed = before - pending.len();
    if removed > 0 {
        eprintln!("rust2go-ipc: swept {removed} stale pending entries");
    }
}

fn connect_with_retry(port: u16, timeout: Duration) -> Option<TcpStream> {
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect(&addr) {
            Ok(s) => return Some(s),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
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

    rt.pending
        .lock()
        .expect("rust2go-ipc: pending lock poisoned")
        .insert(
            req_id,
            Pending {
                slot,
                cb,
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
    // Use the runtime port: RUST2GO_IPC_PORT_OVERRIDE may differ from the
    // build-time constant passed in `port`.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", rt.port))
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
    (trampoline)(resp.as_ptr(), resp.len(), slot, cb);
}

fn read_response<R: Read>(reader: &mut R) -> std::io::Result<(u64, u8, Vec<u8>)> {
    use std::io::ErrorKind;
    let mut header = [0u8; 19];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "bad response magic",
        ));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("unsupported response version {version}"),
        ));
    }
    let req_id = u64::from_le_bytes(header[6..14].try_into().expect("8 bytes"));
    let status = header[14];
    let len = u32::from_le_bytes(header[15..19].try_into().expect("4 bytes")) as usize;
    if len > MAX_PAYLOAD {
        return Err(std::io::Error::new(
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
/// # Safety
/// The Go server must be running; no Rust pointers are retained by this call.
pub unsafe fn dispatch_oneway(server: &str, port: u16, method_id: u32, payload: &[u8]) {
    let rt = runtime(server, port);
    write_frame(rt, 0, method_id, payload);
}

fn build_frame(req_id: u64, method_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(22 + payload.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&VERSION.to_le_bytes());
    frame.extend_from_slice(&req_id.to_le_bytes());
    frame.extend_from_slice(&method_id.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn write_frame(rt: &Runtime, req_id: u64, method_id: u32, payload: &[u8]) {
    let frame = build_frame(req_id, method_id, payload);
    let mut writer = rt.writer.lock().expect("rust2go-ipc: writer lock poisoned");
    writer
        .write_all(&frame)
        .expect("rust2go-ipc: cannot write request frame");
    writer
        .flush()
        .expect("rust2go-ipc: cannot flush request frame");
}

fn reactor_loop(mut reader: TcpStream, rt: &'static Runtime) {
    loop {
        let (req_id, status, payload) = match read_response(&mut reader) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("rust2go-ipc: connection lost ({e}); restarting Go server");
                // In-flight futures stay pending until their caller timeout;
                // a late response can never reach a stale slot because the
                // connection is gone.
                rt.pending
                    .lock()
                    .expect("rust2go-ipc: pending lock poisoned")
                    .clear();
                if let Some(mut child) = rt.child.lock().expect("child lock poisoned").take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                // Keep one child handle across retries. Spawning on every
                // failed connect leaks processes and races for the port.
                let mut child = spawn_server(&rt.server, rt.port);
                loop {
                    if let Some(status) = child.try_wait().expect("rust2go-ipc: try_wait failed") {
                        eprintln!(
                            "rust2go-ipc: Go server exited during reconnect ({status}); respawning"
                        );
                        child = spawn_server(&rt.server, rt.port);
                    }
                    match connect_with_retry(rt.port, Duration::from_secs(2)) {
                        Some(stream) => {
                            reader = stream
                                .try_clone()
                                .expect("rust2go-ipc: cannot clone reconnected stream");
                            *rt.writer.lock().expect("writer lock poisoned") = stream;
                            *rt.child.lock().expect("child lock poisoned") = Some(child);
                            break;
                        }
                        None => {
                            eprintln!("rust2go-ipc: reconnect failed; retrying in 1s");
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
                continue;
            }
        };

        let pending = rt
            .pending
            .lock()
            .expect("rust2go-ipc: pending lock poisoned")
            .remove(&req_id);

        match pending {
            Some(p) if status == 0 => {
                // A malformed response must not kill the reactor thread.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    (p.trampoline)(payload.as_ptr(), payload.len(), p.slot, p.cb);
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

    pub fn prim<T: Copy>(&mut self, v: T) {
        let bytes = unsafe {
            std::slice::from_raw_parts(&v as *const T as *const u8, std::mem::size_of::<T>())
        };
        self.buf.extend_from_slice(bytes);
    }

    pub fn string(&mut self, s: RawStringRef) {
        let len = s.len;
        self.prim(len as u64);
        if len == 0 {
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, len) };
        self.buf.extend_from_slice(bytes);
    }

    pub fn list_header(&mut self, len: usize) {
        self.prim(len as u64);
    }

    /// Write a primitive list as `[u64 len][len * size_of::<T>() raw bytes]`.
    pub fn prim_list<T: Copy>(&mut self, ptr: *const (), len: usize) {
        self.prim(len as u64);
        if len == 0 {
            return;
        }
        let nbytes = len * std::mem::size_of::<T>();
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, nbytes) };
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
    owned: Vec<Box<dyn std::any::Any>>,
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
        self.owned.push(Box::new(boxed) as Box<dyn std::any::Any>);
        (ptr, len)
    }

    /// Copy a typed element array into a stable allocation.
    pub fn elements<T: Copy + 'static>(&mut self, elems: &[T]) -> *const T {
        let boxed: Box<[T]> = elems.into();
        let ptr = boxed.as_ptr();
        self.owned.push(Box::new(boxed) as Box<dyn std::any::Any>);
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

    pub fn prim<T: Copy>(&mut self) -> T {
        let n = std::mem::size_of::<T>();
        let bytes = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) }
    }

    pub fn string(&mut self) -> RawStringRef {
        let len = self.prim::<u64>() as usize;
        if len == 0 {
            return RawStringRef {
                ptr: std::ptr::null(),
                len: 0,
            };
        }
        let bytes = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        let (ptr, len) = self.arena.bytes(bytes);
        RawStringRef { ptr, len }
    }

    pub fn list_len(&mut self) -> usize {
        self.prim::<u64>() as usize
    }

    pub fn list_elements<T: Copy + 'static>(&mut self, n: usize) -> RawListRef {
        let items: Vec<T> = (0..n).map(|_| self.prim::<T>()).collect();
        let ptr = self.arena.elements(&items);
        RawListRef {
            ptr: ptr as *const (),
            len: n,
        }
    }

    /// Read a primitive list written by [`Encoder::prim_list`].
    pub fn prim_list<T: Copy + 'static>(&mut self) -> RawListRef {
        let n = self.list_len();
        if n == 0 {
            return RawListRef {
                ptr: std::ptr::null(),
                len: 0,
            };
        }
        let nbytes = n * std::mem::size_of::<T>();
        let bytes = &self.buf[self.pos..self.pos + nbytes];
        self.pos += nbytes;
        let ptr = self.arena.bytes(bytes).0 as *const ();
        RawListRef { ptr, len: n }
    }

    /// Allocate a restored Ref-element array in the arena, returning a
    /// `ListRef` whose `ptr` points at it.
    pub fn ref_list<T: Copy + 'static>(&mut self, items: Vec<T>) -> RawListRef {
        let n = items.len();
        if n == 0 {
            return RawListRef {
                ptr: std::ptr::null(),
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
