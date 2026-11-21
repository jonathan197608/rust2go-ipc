# rust2go-ipc

Drop-in replacement for [rust2go](https://crates.io/crates/rust2go) that keeps Rust in the main process and moves Go into a child process. Calls cross the boundary over a local TCP socket instead of in-process cgo FFI.

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

The `build` feature pulls in `syn`, `quote`, and `proc-macro2`. The runtime feature set has no extra dependencies beyond upstream `rust2go`.

## How it works

The rust2go macro expands each Go call into one external symbol, `binding::CGolangCall_xxx(req_ref, slot, cb)`. The callback pointer is passed in at runtime. That is the entire seam:

1. The stub flattens the Ref pointer graph into a self-contained byte buffer.
2. `dispatch` assigns a request id, stores `(slot, cb, trampoline)` in a pending table, writes one frame, and returns immediately. Tokio workers are never blocked.
3. A reactor thread reads response frames, restores the response Ref graph into an arena, and invokes the generated trampoline.
4. The trampoline calls the original `*_cb`, which writes into the atomic slot and wakes the future.

Wire format version is 1. The magic bytes are `R2GI`. The client and server must be built from the same crate version.

`i128` and `u128` fields are rejected at build time because their Rust and Go wire widths differ.

## Configuration

| Variable | Purpose |
|---|---|
| `RUST2GO_IPC_SERVER_OVERRIDE` | Path to the Go server binary. Use this in stripped runtime images where the build-time `OUT_DIR` path does not exist. |
| `RUST2GO_IPC_PORT_OVERRIDE` | Loopback port override. Both sides must agree; the value is forwarded to the child process. |
| `RUST2GO_IPC_PENDING_TTL_SECS` | Pending table entry lifetime in seconds. Default `600`. |

## Failure semantics

The generated Go dispatcher recovers handler panics and returns an error status. The Rust side logs it and drops the pending entry; the awaiting future is resolved by the caller's own timeout, since the rust2go callback signature has no error channel.

If the connection drops, the reactor clears pending entries, restarts the Go server, and reconnects. In-flight futures rely on caller timeouts. Rust atomic slots remain memory-safe when a late callback observes a dropped reader.

## Limitations

- Transport is TCP loopback. It behaves identically on Windows development machines and Linux containers, but a UDS transport would avoid the loopback stack in production.
- Request/response payloads are capped at 64 MiB.
- `usize`/`isize` fields are encoded at native width; this assumes a 64-bit target on both sides.
- The Go server may still use cgo for unrelated business dependencies. This crate only removes cgo from the rust2go call path.

## License

MIT
