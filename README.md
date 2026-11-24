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
rust2go = { package = "rust2go-ipc", version = "0.2" }

[build-dependencies]
rust2go = { package = "rust2go-ipc", version = "0.2", features = ["build"] }
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
| `RUST2GO_IPC_PORT_OVERRIDE` | Port override (`0` is rejected). Both sides must agree; forwarded to the child process. On Unix it does not change the transport (still UDS) but does change the private socket directory name (`rust2go-ipc-{pid}-{port}.d`), so both processes still need the same value. |
| `RUST2GO_IPC_ENDPOINT_OVERRIDE` | Full endpoint: socket path on Unix, `host:port` elsewhere. Both sides must agree. |
| `RUST2GO_IPC_PENDING_TTL_SECS` | Pending table entry lifetime in seconds. Default `600`. `0` is rejected. |
| `RUST2GO_IPC_SYNC_TIMEOUT_SECS` | Response deadline for synchronous calls, in seconds. Default `30`, independent of the async pending TTL. `0` is rejected. |
| `RUST2GO_IPC_ASYNC_TIMEOUT_SECS` | Deadline applied by `#[r2g_result]` async wrappers. Default `600` (10 minutes). `0` is rejected. |
| `RUST2GO_IPC_MAX_CONCURRENCY` | Max in-flight Go handlers per server. Default `NumCPU*4` (min 16). Lower it to bound downstream load; the read loop applies back-pressure when full. |

The Rust side reads the timeout env vars on each call. The Go server samples `RUST2GO_IPC_SYNC_TIMEOUT_SECS` once at startup to size its backpressure wait; changing that value therefore needs a server restart (a reconnect alone reuses the running child).

## Failure semantics

The generated Go dispatcher recovers handler panics and returns an error status. The Rust side logs it and drops the pending entry; the awaiting future is resolved by the caller's own timeout, since the rust2go callback signature has no error channel.

**Raw `#[r2g]` calls keep their panic contract.** As of 0.2.2 the old public `dispatch_sync` / `dispatch_oneway` functions are removed; both generated stub families now call `dispatch_sync_result` / `dispatch_oneway_result`. The legacy stub (a `#[r2g]` trait without `#[r2g_result]`) panics locally on `Err` with a descriptive message, preserving drop-in behavior; otherwise upstream `slot.take().unwrap()` would panic with a useless "None" message. For non-panicking callers, add `#[r2g_result]` and call `GolangCallResult::{method}(...)`: the generated wrapper returns `Result<Resp, IpcError>` (or `Result<(), IpcError>` for oneway), which is safe even under `panic = "abort"`. Do not rely on `catch_unwind` in `panic = "abort"` binaries.

**Boundaries.** `IpcError` covers transport/protocol/timeout failures only. A malformed response that panics inside the generated decoder is mapped to `IpcError::Decode` when the binary uses `panic = "unwind"`; under `panic = "abort"` that panic terminates the process before any `Result` is returned — the library cannot recover it without coupling to business response types. Initial Go-server startup failures (spawn, 30s readiness, socket setup) are also process-fatal by design; validate the endpoint and Go binary before calling into IPC to turn those into a controlled exit.

If the connection drops, the reactor clears pending entries and rebuilds the socket pair. It restarts the Go child **only if the process has actually exited** (`try_wait` under the child-management lock); a healthy child that merely had an idle connection closed is reused, so in-process state (a one-way `Init`, caches, pools) survives reconnects. In-flight futures rely on caller timeouts. Rust atomic slots remain memory-safe when a late callback observes a dropped reader. A failed request write shuts the connection down immediately so the reactor rebuilds both ends rather than keeping a frame-desynchronised stream.

## Security model

The IPC endpoint is a **local, intra-host trust boundary**, not an authenticated network service. Treat every local process running as the same user as trusted.

- **Unix (UDS)**: the socket node is `chmod 0600` immediately after `Listen`, so only the owning uid (the process that spawned the server) can connect. A different local user cannot reach it. The endpoint path is under the user's `TMPDIR` (falling back to `/tmp` when too long for `sun_path`).
- **Other platforms (TCP loopback)**: there is **no peer authentication** — any process that can open a loopback connection to the port can dispatch calls. This is acceptable only when the process boundary is itself a trust boundary (e.g. a container, or a single-user host). Do not expose this transport on a multi-tenant host where untrusted local code can reach `127.0.0.1`.
- Neither transport encrypts payloads; both carry method arguments and return values in plaintext over the local channel.
- Endpoint path / port overrides are configuration, not a security boundary: anything that can set the parent's environment already controls the process.

If you need isolation from other local users on a non-Unix platform, run the whole application in a container / dedicated user account rather than relying on the IPC layer.

## Limitations

- Transport is fixed by compile target and cannot be overridden: **UDS on Unix, TCP loopback elsewhere**. On Unix the socket node is created with mode `0600`, and the Go server fails closed if it is ever asked to bind TCP. There is no `RUST2GO_IPC_TRANSPORT_OVERRIDE`; the unsuffixed `RUST2GO_IPC_TRANSPORT` / `RUST2GO_IPC_ENDPOINT` variables are forwarded to the Go child for internal use.
- Request/response payloads are capped at 64 MiB. Go responses over the cap are converted to error status. With `#[r2g_result]`, oversized async requests are rejected before the upstream future is created and surface as `Err(IpcError::PayloadTooLarge)`; raw `#[r2g]` async calls still panic at poll time.
- `i128`/`u128`, nested lists, and lists of empty structs are intentionally unsupported and fail at build time with an explicit message (an empty struct occupies zero wire bytes, so a non-empty list cannot round-trip).
- The Rust process kills and reaps the Go child on normal exit via an atexit hook. A `SIGKILL`-style hard kill cannot be covered.
- Go handlers run concurrently on the shared connection and responses may arrive out of order; they are matched by request id. Do not rely on handler execution order — make shared Go state concurrency-safe, as with the original cgo path.
- The Go server may still use cgo for unrelated business dependencies. This crate only removes cgo from the rust2go call path.

## License

MIT
