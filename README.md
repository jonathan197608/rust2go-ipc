# rust2go-ipc

Drop-in replacement for [rust2go](https://crates.io/crates/rust2go) that keeps Rust in the main process and moves Go into a child process. Calls cross the boundary over a local IPC channel (a Unix domain socket on Unix, a TCP loopback socket elsewhere) instead of in-process cgo FFI.

This removes the Go runtime from the host process, which fixes crashes caused by conflicting signal handlers when the host is a JVM (for example `non-Go code set up signal handler without SA_ONSTACK flag`).

## Why

rust2go embeds the Go runtime into the Rust process via cgo. That is fast, but it means Go's signal handlers coexist with any other runtime in the same address space. In a JVM host that combination crashes under load.

`rust2go-ipc` keeps the same public API and generated-symbol contract, so business code does not change. The Go side runs as a separate process and only the generated glue changes.

## Usage

Change two dependency lines. Build scripts and business sources stay byte-identical.

```toml
[dependencies]
rust2go = { package = "rust2go-ipc", version = "0.1" }

[build-dependencies]
rust2go = { package = "rust2go-ipc", version = "0.1", features = ["build"] }
```

The `package =` alias makes `use rust2go::{...}`, `#[r2g]`, `#[derive(R2G)]`, `r2g_include_binding!()` and the `Builder` API resolve to this crate without touching any source file.

A Go toolchain must be available at build time. The build script runs `go build` to compile the IPC server and writes:

- `$OUT_DIR/_go_bindings.rs` — mirrored `#[repr(C)]` Ref structs, stubs, and response trampolines
- `<go_src>/gen.go` — pure Go Ref structs, conversion helpers, and the method dispatcher
- `<go_src>/ipc_main.go` — the IPC server entry point
- `target/<triple>/<profile>/go-ipc-server` — a stable binary path for container image builds

The `build` feature pulls in `syn`, `quote`, and `proc-macro2`. Beyond upstream `rust2go`, the runtime depends only on `libc`.

## How it works

The rust2go macro expands each Go call into one external symbol, `binding::CGolangCall_xxx(req_ref, slot, cb)`. The callback pointer is passed in at runtime. That is the entire seam:

1. The stub flattens the Ref pointer graph into a self-contained byte buffer.
2. `dispatch` assigns a request id, stores `(slot, cb, trampoline)` in a pending table, writes one frame, and returns immediately. Tokio workers are never blocked.
3. The Go server dispatches each request in its own goroutine, so a slow handler never serialises later requests on the shared connection. Responses are written back out of order; the Rust reactor matches them by request id. Concurrency is bounded by a semaphore with back-pressure (both cancellable on teardown and time-boxed after 60 s (longer than the 30 s Rust sync timeout) so a wedged slot cannot leak a goroutine), and response writes are serialised to keep frames intact. The server's 5-minute read deadline is a keep-alive tick, not a disconnect: idle expiry renews it and keeps waiting, so an idle or long-running handler never causes the connection to close. On teardown, in-flight handlers get a bounded 2-second window to release their slot (a hung handler never blocks exit, and the Rust pending TTL is the backstop).
4. A reactor thread reads response frames, restores the response Ref graph into an arena, and invokes the generated trampoline.
5. The `C-unwind` trampoline performs the callback effect directly (sync: write the stack `Option`; async: `SlotWriter::write`), so no panic can cross an `extern "C"` boundary and abort the process. Pending slots are explicitly released on timeout, disconnect, error status, or trampoline panic.

Wire format version is 1. The magic bytes are `R2GI`. All multi-byte primitives are little-endian. The client and server must be built from the same crate version.

`i128`, `u128` and nested `Vec<Vec<T>>` fields are rejected at build time. `usize`/`isize` require a 64-bit target (checked at compile time). Zero-argument, multi-argument, non-struct parameter/return types, `char`, `&T` parameters, `#[go_pass_struct]` and multiple `#[r2g]` traits are supported.

## Configuration

| Variable | Purpose |
|---|---|
| `RUST2GO_IPC_SERVER_OVERRIDE` | Path to the Go server binary. Use this in stripped runtime images where the build-time `OUT_DIR` path does not exist. |
| `RUST2GO_IPC_PORT_OVERRIDE` | Loopback port override. Both sides must agree; the value is forwarded to the child process. |
| `RUST2GO_IPC_TRANSPORT_OVERRIDE` | Force `tcp` or `unix` regardless of compile target. |
| `RUST2GO_IPC_ENDPOINT_OVERRIDE` | Full endpoint (port or socket path). Both sides must agree. |
| `RUST2GO_IPC_PENDING_TTL_SECS` | Pending table entry lifetime in seconds. Default `600`. `0` is rejected. |
| `RUST2GO_IPC_SYNC_TIMEOUT_SECS` | Response deadline for synchronous calls, in seconds. Default `30`, independent of the async pending TTL. `0` is rejected. |
| `RUST2GO_IPC_MAX_CONCURRENCY` | Max in-flight Go handlers per server. Default `NumCPU*4` (min 16). Lower it to bound downstream load; the read loop applies back-pressure when full. |

## Failure semantics

The generated Go dispatcher recovers handler panics and returns an error status. The Rust side logs it and drops the pending entry; the awaiting future is resolved by the caller's own timeout, since the rust2go callback signature has no error channel.

**Synchronous calls have no error channel either**: connect/write/read failures, a bad response id, an error status, or the sync timeout make `dispatch_sync` panic with a descriptive message (method id and cause) instead of returning with the slot unset — otherwise the macro's `slot.take().unwrap()` would panic with a useless "None" message. The generated stub is a plain Rust `unsafe fn`, so this panic unwinds normally to the caller. If you cannot tolerate a panic, wrap the call in `catch_unwind`.

If the connection drops, the reactor clears pending entries and rebuilds the socket pair. It restarts the Go child **only if the process has actually exited** (`try_wait` under the child-management lock); a healthy child that merely had an idle connection closed is reused, so in-process state (a one-way `Init`, caches, pools) survives reconnects. In-flight futures rely on caller timeouts. Rust atomic slots remain memory-safe when a late callback observes a dropped reader. A failed request write shuts the connection down immediately so the reactor rebuilds both ends rather than keeping a frame-desynchronised stream.

## Limitations

- Transport is selected by compile target: UDS on Unix, TCP loopback elsewhere (override from the Rust side with `RUST2GO_IPC_TRANSPORT_OVERRIDE` / `RUST2GO_IPC_ENDPOINT_OVERRIDE`; the unsuffixed `RUST2GO_IPC_TRANSPORT` / `RUST2GO_IPC_ENDPOINT` variables are forwarded to the Go child).
- Request/response payloads are capped at 64 MiB. Go responses over the cap are converted to error status.
- `i128`/`u128`, nested lists, and lists of empty structs are intentionally unsupported and fail at build time with an explicit message (an empty struct occupies zero wire bytes, so a non-empty list cannot round-trip).
- The Rust process kills and reaps the Go child on normal exit via an atexit hook. A `SIGKILL`-style hard kill cannot be covered.
- Go handlers run concurrently on the shared connection and responses may arrive out of order; they are matched by request id. Do not rely on handler execution order — make shared Go state concurrency-safe, as with the original cgo path.
- The Go server may still use cgo for unrelated business dependencies. This crate only removes cgo from the rust2go call path.

## License

MIT
