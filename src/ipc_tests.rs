use super::*;
use std::mem::align_of;

#[test]
fn child_pid_keeps_full_u32_range() {
    // Windows pids can exceed i32::MAX. The atexit hook must see the exact u32
    // value so taskkill targets the right process; storing as AtomicI32 would
    // sign-flip and the `pid > 0` guard would skip the kill entirely.
    let high_pid: u32 = 0xF000_0000;
    CHILD_PID.store(high_pid, Ordering::SeqCst);
    assert_eq!(CHILD_PID.load(Ordering::SeqCst), high_pid);
    // Restore the sentinel so this test cannot affect others.
    CHILD_PID.store(0, Ordering::SeqCst);
}

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
#[should_panic(expected = "string length")]
fn decoder_string_rejects_huge_len() {
    // u64 len = 0xFFFF_FFFF_FFFF_FFFF, no payload follows. The length is now
    // validated in the u64 domain before narrowing to usize (32-bit safety).
    let mut dec = Decoder::new(&[0xff; 8]);
    let _ = dec.string();
}

#[test]
#[should_panic(expected = "string length")]
fn decoder_string_rejects_len_above_usize_max_even_with_payload() {
    // On a 64-bit host this exercises the u64-domain guard directly; on a
    // 32-bit host it additionally proves no wraparound to a small len occurs.
    // 8-byte header declares 0x1_0000_0000 bytes, then one trailing byte.
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x1_0000_0000u64.to_le_bytes());
    buf.push(0x41);
    let mut dec = Decoder::new(&buf);
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
fn wire_primitives_are_little_endian() {
    let mut enc = Encoder::new();
    enc.prim(0x0102_0304u32);
    enc.prim('A');
    assert_eq!(enc.take(), vec![4, 3, 2, 1, b'A', 0, 0, 0]);
    let mut dec = Decoder::new(&[4, 3, 2, 1]);
    assert_eq!(dec.prim::<u32>(), 0x0102_0304);
    let mut dec = Decoder::new(&[b'A', 0, 0, 0]);
    assert_eq!(dec.prim::<char>(), 'A');
}

#[test]
#[should_panic(expected = "list length")]
fn decoder_list_len_min_rejects_huge_count() {
    let mut dec = Decoder::new(&[0xff; 8]);
    let _ = dec.list_len_min(16);
}

#[test]
#[should_panic(expected = "list length")]
fn decoder_list_elements_rejects_huge_count() {
    let mut dec = Decoder::new(&[0xff; 8]);
    // n = u64::MAX elements, no payload -> must be rejected before allocating
    let _ = dec.list_elements::<u64>(u64::MAX as usize);
}

#[test]
fn empty_lists_use_null_pointer() {
    // Every list-decoding API must agree: an empty list has a null pointer,
    // never the dangling-but-aligned pointer of an empty Box<[T]>.
    let mut dec = Decoder::new(&[]);
    let elements = dec.list_elements::<u64>(0);
    assert!(elements.ptr.is_null());
    assert_eq!(elements.len, 0);

    let zero = 0u64.to_le_bytes();
    let mut dec = Decoder::new(&zero);
    let prims = dec.prim_list::<u64>();
    assert!(prims.ptr.is_null());
    assert_eq!(prims.len, 0);
}

#[cfg(unix)]
#[test]
fn unix_transport_is_forced_to_uds_even_with_tcp_endpoint() {
    // Unix builds pin transport to UDS; no environment variable may switch it
    // to TCP. An endpoint override is still honoured, but only as a UDS path.
    std::env::set_var(ENV_ENDPOINT_OVERRIDE, "127.0.0.1:9999");
    let ep = resolve_endpoint(1234);
    assert_eq!(ep.transport, Transport::Unix);
    assert_eq!(ep.addr, "127.0.0.1:9999");
    std::env::remove_var(ENV_ENDPOINT_OVERRIDE);
}

#[cfg(unix)]
#[test]
fn unix_default_endpoint_is_a_uds_path() {
    std::env::remove_var(ENV_ENDPOINT_OVERRIDE);
    let ep = resolve_endpoint(4321);
    assert_eq!(ep.transport, Transport::Unix);
    assert!(
        ep.addr.ends_with("/ipc.sock"),
        "unexpected addr: {}",
        ep.addr
    );
    assert!(ep.addr.len() < 100, "path must fit sun_path: {}", ep.addr);
    // Socket must live in a 0700 private directory so other local users cannot
    // traverse to the briefly-0755 node net.Listen creates before Chmod.
    use std::os::unix::fs::PermissionsExt;
    let dir = std::path::Path::new(&ep.addr).parent().unwrap();
    let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "socket dir must be 0700, got {mode:o}");
}

#[cfg(unix)]
#[test]
fn unix_umask_regression_does_not_need_process_wide_restriction() {
    // Guard the reason the socket is in a private dir: a pre_exec umask would
    // leak into the Go business handlers. This test just asserts the endpoint
    // carries a directory component (so isolation is directory-based).
    std::env::remove_var(ENV_ENDPOINT_OVERRIDE);
    let ep = resolve_endpoint(4322);
    let p = std::path::Path::new(&ep.addr);
    assert!(p.parent().and_then(|d| d.file_name()).is_some());
    assert_eq!(p.file_name().unwrap(), "ipc.sock");
}

#[test]
fn port_override_zero_is_rejected() {
    // Mirrors the `runtime()` port parsing: 0 must fall back, never be used.
    let parsed = "0".parse::<u16>().ok().filter(|v| *v > 0).unwrap_or(7777);
    assert_eq!(parsed, 7777);
    let parsed = "5555"
        .parse::<u16>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(7777);
    assert_eq!(parsed, 5555);
}

// A Read that yields a fixed set of byte chunks, then either blocks forever
// (returning TimedOut) or ends the stream. Used to test mid-frame timeout
// handling in read_exact_frame.
struct ChunkReader {
    chunks: Vec<Vec<u8>>,
    then_timeout: bool,
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(chunk) = self.chunks.first() {
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            let remaining = chunk.len() - n;
            if remaining == 0 {
                self.chunks.remove(0);
            } else {
                self.chunks[0].drain(..n);
            }
            return Ok(n);
        }
        if self.then_timeout {
            Err(IoError::new(ErrorKind::TimedOut, "idle"))
        } else {
            Ok(0)
        }
    }
}

#[test]
fn read_exact_frame_idle_timeout_stays_timed_out() {
    // No bytes consumed before the timeout: must stay TimedOut so the reactor
    // probes child liveness and continues.
    let mut r = ChunkReader {
        chunks: vec![],
        then_timeout: true,
    };
    let mut buf = [0u8; 19];
    let err = read_exact_frame(&mut r, &mut buf, 0).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TimedOut);
}

#[test]
fn read_exact_frame_mid_frame_timeout_becomes_invalid_data() {
    // Some header bytes consumed before the timeout: must be InvalidData so
    // the reactor resyncs the connection instead of parsing frame bytes as a
    // fresh header (which would desynchronise the stream forever).
    let mut r = ChunkReader {
        chunks: vec![vec![0x52, 0x32, 0x47, 0x49]],
        then_timeout: true,
    };
    let mut buf = [0u8; 19];
    let err = read_exact_frame(&mut r, &mut buf, 0).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData);
}

#[test]
fn read_exact_frame_payload_stall_after_header_is_invalid_data() {
    // The 19-byte header is already consumed (frame_offset=19). A timeout
    // before the first payload byte must still be InvalidData, otherwise the
    // reactor treats it as an idle tick and parses payload bytes as a header.
    let mut r = ChunkReader {
        chunks: vec![],
        then_timeout: true,
    };
    let mut buf = [0u8; 8];
    let err = read_exact_frame(&mut r, &mut buf, 19).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData);
}

#[test]
fn read_exact_frame_eof_mid_frame_is_unexpected_eof() {
    let mut r = ChunkReader {
        chunks: vec![vec![0x52, 0x32]],
        then_timeout: false,
    };
    let mut buf = [0u8; 19];
    let err = read_exact_frame(&mut r, &mut buf, 0).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
}

#[test]
fn read_exact_frame_full_read_succeeds() {
    let mut r = ChunkReader {
        chunks: vec![vec![1, 2, 3], vec![4, 5]],
        then_timeout: true,
    };
    let mut buf = [0u8; 5];
    read_exact_frame(&mut r, &mut buf, 0).unwrap();
    assert_eq!(buf, [1, 2, 3, 4, 5]);
}

#[test]
#[should_panic(expected = "restore nesting exceeds")]
fn decoder_depth_guard_stops_runaway_recursion() {
    // Simulates the generated restore helper's enter() on every level. Without
    // the guard, a self-referential schema could recurse until stack overflow
    // (a fatal abort that catch_unwind cannot intercept).
    let mut dec = Decoder::new(&[]);
    for _ in 0..=MAX_RESTORE_DEPTH {
        dec.enter();
    }
}

#[test]
fn decoder_depth_enter_leave_is_balanced() {
    let mut dec = Decoder::new(&[]);
    for _ in 0..MAX_RESTORE_DEPTH {
        dec.enter();
    }
    for _ in 0..MAX_RESTORE_DEPTH {
        dec.leave();
    }
    // Back to zero: a fresh restore chain may use the full budget again.
    for _ in 0..MAX_RESTORE_DEPTH {
        dec.enter();
    }
}

#[test]
#[should_panic(expected = "payload too large")]
fn oneway_oversized_payload_panics_before_initializing_runtime() {
    // The guard must run before runtime() so an oversized oneway payload cannot
    // panic while holding the global writer mutex (which would poison it).
    // A bogus server path proves runtime() is never reached: if it were, spawn
    // would fail with a different error instead of our payload message.
    let huge = vec![0u8; MAX_PAYLOAD + 1];
    dispatch_oneway("/nonexistent/rust2go-ipc-test.sock", 0, 1, &huge);
}
