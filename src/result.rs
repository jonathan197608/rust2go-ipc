//! Error type and timeout configuration for the optional `r2g_result` wrapper.
//!
//! The `#[r2g_result]` attribute (from the companion `rust2go-ipc-result` crate)
//! generates thin wrappers around `#[r2g]` async methods that turn an expired
//! deadline into [`IpcError`] instead of an unresolvable future. This module is
//! dependency-free (no tokio) so the core runtime stays lean; the generated
//! wrapper owns the `tokio::time::timeout` call and only needs these symbols.

use std::{env, fmt, time::Duration};

/// Timeout applied by generated `#[r2g_result]` async wrappers (seconds).
pub const ENV_ASYNC_TIMEOUT: &str = "RUST2GO_IPC_ASYNC_TIMEOUT_SECS";

/// Default async-call timeout: long enough for heavy reports, short enough to
/// bound a wedged Go handler's blast radius.
pub const DEFAULT_ASYNC_TIMEOUT: u64 = 60;

/// Upper bound, consistent with the IPC sync/TTL knobs (24 h).
pub const MAX_ASYNC_TIMEOUT: u64 = 86_400;

/// Error returned by generated `GolangCallResult` async wrappers.
///
/// This is a transport-level error only: a successful response carrying a
/// business error (e.g. a non-empty `err_msg`) still yields `Ok(resp)` — the
/// business layer keeps its existing error handling untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// The call did not return a response within the configured deadline.
    ///
    /// Dropping the inner future releases the slot, so this error is a clean
    /// convergence point instead of a permanently pending future.
    Timeout {
        /// Method name, for diagnostics.
        method: &'static str,
        /// Deadline that elapsed, in seconds.
        secs: u64,
    },
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Timeout { method, secs } => {
                write!(f, "async call `{method}` timed out after {secs}s")
            }
        }
    }
}

impl std::error::Error for IpcError {}

/// Read the async-call timeout, clamped to `1..=MAX_ASYNC_TIMEOUT`.
///
/// Mirrors the runtime's `bounded_env_secs`: 0 disables the deadline and an
/// absurdly large value turns it into an unbounded wait, so both are rejected
/// with a warning and fall back. Never panics — this runs on call paths.
pub fn async_timeout_secs() -> u64 {
    match env::var(ENV_ASYNC_TIMEOUT).ok().filter(|v| !v.is_empty()) {
        Some(v) => match v.parse::<u64>() {
            Ok(n) if (1..=MAX_ASYNC_TIMEOUT).contains(&n) => n,
            _ => {
                eprintln!(
                    "rust2go-ipc: invalid {ENV_ASYNC_TIMEOUT}={v:?} (expected 1..={MAX_ASYNC_TIMEOUT}); using {DEFAULT_ASYNC_TIMEOUT}"
                );
                DEFAULT_ASYNC_TIMEOUT
            }
        },
        None => DEFAULT_ASYNC_TIMEOUT,
    }
}

/// Deadline as a [`Duration`], for direct use with `tokio::time::timeout`.
pub fn async_timeout() -> Duration {
    Duration::from_secs(async_timeout_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_error_display_includes_method_and_secs() {
        let e = IpcError::Timeout {
            method: "esDump",
            secs: 60,
        };
        assert_eq!(e.to_string(), "async call `esDump` timed out after 60s");
    }

    #[test]
    fn async_timeout_is_positive_and_bounded() {
        let secs = async_timeout_secs();
        assert!((1..=MAX_ASYNC_TIMEOUT).contains(&secs));
        assert_eq!(async_timeout(), Duration::from_secs(secs));
    }
}
