//! Drop-in `rust2go` replacement: Rust stays in the main process, Go runs in a
//! child process, and calls cross the boundary over IPC instead of cgo FFI.
//!
//! The public API mirrors `rust2go` v0.4 so existing business code compiles
//! unchanged. Switch a dependency with a Cargo alias:
//!
//! ```toml
//! rust2go = { package = "rust2go-ipc", version = "0.2" }
//! ```
//!
//! For calls that must not hang or panic on a wedged Go handler, the companion
//! `rust2go-ipc-result` crate adds `#[r2g_result]`, which generates parallel
//! wrappers returning `Result<Resp, IpcError>` (also for sync/oneway methods)
//! with timeouts.
//!
//! Runtime (non-build) symbols are re-exported from upstream `rust2go` because
//! proc-macro expansions from `rust2go-macro` reference `::rust2go::*` paths,
//! which resolve through this crate under the alias.

pub use rust2go::{
    g2r, max_mem_type, new_atomic_slot, r2g, r2g_include_binding, r2g_struct_tag, CopyStruct,
    DataView, FromRef, ListRef, MemType, ResponseFuture, ResponseFutureWithoutReq, SlotReader,
    SlotWriter, StringRef, ToRef, Writer, R2G,
};

pub const DEFAULT_BINDING_FILE: &str = "_go_bindings.rs";

/// Transport-level error and timeout config for the optional `#[r2g_result]`
/// wrapper (companion crate `rust2go-ipc-result`). Dependency-free by design.
pub mod result;
pub use result::{async_timeout, async_timeout_secs, IpcError, DEFAULT_ASYNC_TIMEOUT};

#[cfg(feature = "build")]
mod builder;
#[cfg(feature = "build")]
pub use builder::RegenArgs;
#[cfg(feature = "build")]
pub use builder::{Builder, CopyLib, CustomArgGoCompiler, DefaultGoCompiler, GoCompiler, LinkType};

/// IPC transport runtime, used by generated binding stubs.
pub mod ipc;
