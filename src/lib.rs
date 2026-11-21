//! Drop-in `rust2go` replacement: Rust stays in the main process, Go runs in a
//! child process, and calls cross the boundary over IPC instead of cgo FFI.
//!
//! The public API mirrors `rust2go` v0.4 so existing business code compiles
//! unchanged. Switch a dependency with a Cargo alias:
//!
//! ```toml
//! rust2go = { package = "rust2go-ipc", version = "0.1" }
//! ```
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

#[cfg(feature = "build")]
mod builder;
#[cfg(feature = "build")]
pub use builder::RegenArgs;
#[cfg(feature = "build")]
pub use builder::{Builder, CopyLib, CustomArgGoCompiler, DefaultGoCompiler, GoCompiler, LinkType};

/// IPC transport runtime, used by generated binding stubs.
pub mod ipc;
