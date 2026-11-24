//! Error type and timeout configuration for the optional `r2g_result` wrapper.
//!
//! The `#[r2g_result]` attribute (from the companion `rust2go-ipc-result` crate)
//! generates thin wrappers around `#[r2g]` async methods that turn an expired
//! deadline into [`IpcError`] instead of an unresolvable future. This module is
//! dependency-free (no tokio) so the core runtime stays lean; the generated
//! wrapper owns the `tokio::time::timeout` call and only needs these symbols.

use std::{env, fmt, sync::Mutex, time::Duration};

/// Timeout applied by generated `#[r2g_result]` async wrappers (seconds).
pub const ENV_ASYNC_TIMEOUT: &str = "RUST2GO_IPC_ASYNC_TIMEOUT_SECS";

/// Default async-call timeout: long enough for heavy reports (10 minutes),
/// still bounded so a wedged Go handler cannot pin a slot forever.
pub const DEFAULT_ASYNC_TIMEOUT: u64 = 600;

/// Upper bound, consistent with the IPC sync/TTL knobs (24 h).
pub const MAX_ASYNC_TIMEOUT: u64 = 86_400;

/// Keys whose invalid value has already been reported. A bad environment
/// variable is a deployment mistake, not a per-call event: printing a warning
/// on every dispatch can flood stderr in a high-QPS service and hide the real
/// diagnostics. Warn once per key per process, then silently fall back.
static WARNED_ENVS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub(crate) fn warn_invalid_env_once(key: &'static str, raw: &str, max: u64, default: u64) {
    let mut warned = WARNED_ENVS.lock().unwrap_or_else(|e| e.into_inner());
    if warned.contains(&key) {
        return;
    }
    warned.push(key);
    eprintln!("rust2go-ipc: invalid {key}={raw:?} (expected 1..={max}); using {default}");
}

/// Error returned by generated `GolangCallResult` async wrappers.
///
/// This is a transport-level error only: a successful response carrying a
/// business error (e.g. a non-empty `err_msg`) still yields `Ok(resp)` — the
/// business layer keeps its existing error handling untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// The async call did not return a response within the configured deadline.
    ///
    /// Dropping the inner future releases the slot, so this error is a clean
    /// convergence point instead of a permanently pending future.
    Timeout {
        /// Method name, for diagnostics.
        method: &'static str,
        /// Deadline that elapsed, in seconds.
        secs: u64,
    },
    /// A synchronous call did not receive a response before its read deadline.
    SyncTimeout {
        /// Method name, for diagnostics.
        method: String,
        /// Deadline that elapsed, in seconds.
        secs: u64,
        /// Where the deadline expired. Currently always `read`: connect and
        /// write failures map to the dedicated `Connect`/`Write` variants.
        stage: String,
    },
    /// The Go server could not be reached.
    Connect {
        /// Method name, for diagnostics.
        method: String,
    },
    /// Socket setup or request write failed.
    Write {
        /// Method name, for diagnostics.
        method: String,
        /// Underlying OS error text.
        detail: String,
    },
    /// Reading the response failed (including the sync read deadline).
    Read {
        /// Method name, for diagnostics.
        method: String,
        /// Underlying OS error text.
        detail: String,
    },
    /// Response was malformed or did not match the request.
    Protocol {
        /// Method name, for diagnostics.
        method: String,
        /// Human-readable mismatch description.
        detail: String,
    },
    /// Response decode panicked (malformed frame or a panicking `FromRef`).
    ///
    /// Only produced when the binary uses `panic = "unwind"`: the runtime wraps
    /// the generated trampoline in `catch_unwind` and maps the panic here.
    /// Under `panic = "abort"` a decode panic is **not** recoverable — the
    /// process terminates before any `Result` can be returned. The library
    /// cannot change that without coupling itself to business response types.
    Decode {
        /// Method name, for diagnostics.
        method: String,
        /// Panic payload text.
        detail: String,
    },
    /// The server replied but no value reached the slot.
    NoResponse {
        /// Method name, for diagnostics.
        method: String,
    },
    /// Request exceeded the protocol payload limit.
    PayloadTooLarge {
        /// Method name, for diagnostics.
        method: String,
        /// Actual payload length.
        len: usize,
    },
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Timeout { method, secs } => {
                write!(f, "async call `{method}` timed out after {secs}s")
            }
            IpcError::SyncTimeout {
                method,
                secs,
                stage,
            } => write!(
                f,
                "sync call `{method}` timed out after {secs}s while {stage}"
            ),
            IpcError::Connect { method } => {
                write!(f, "sync call `{method}` cannot connect to Go server")
            }
            IpcError::Write { method, detail } => {
                write!(f, "sync call `{method}` write failed: {detail}")
            }
            IpcError::Read { method, detail } => {
                write!(f, "sync call `{method}` read failed: {detail}")
            }
            IpcError::Protocol { method, detail } => {
                write!(f, "sync call `{method}` protocol error: {detail}")
            }
            IpcError::Decode { method, detail } => {
                write!(f, "sync call `{method}` response decode panicked: {detail}")
            }
            IpcError::NoResponse { method } => {
                write!(f, "sync call `{method}` returned no response")
            }
            IpcError::PayloadTooLarge { method, len } => {
                // Async wrappers can produce this too (pre-flight check), so
                // keep the wording transport-neutral.
                write!(f, "call `{method}` request payload too large ({len})")
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
                warn_invalid_env_once(
                    ENV_ASYNC_TIMEOUT,
                    &v,
                    MAX_ASYNC_TIMEOUT,
                    DEFAULT_ASYNC_TIMEOUT,
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

    #[test]
    fn payload_too_large_display_is_transport_neutral() {
        let e = IpcError::PayloadTooLarge {
            method: "upload".to_string(),
            len: 64 * 1024 * 1024 + 1,
        };
        assert_eq!(
            e.to_string(),
            "call `upload` request payload too large (67108865)"
        );
    }

    #[test]
    fn default_async_timeout_is_ten_minutes() {
        assert_eq!(DEFAULT_ASYNC_TIMEOUT, 600);
    }

    #[test]
    fn sync_error_display_includes_method_and_stage() {
        let e = IpcError::SyncTimeout {
            method: "newID".to_string(),
            secs: 30,
            stage: "read".to_string(),
        };
        assert_eq!(
            e.to_string(),
            "sync call `newID` timed out after 30s while read"
        );
        assert_eq!(
            IpcError::NoResponse {
                method: "newID".to_string()
            }
            .to_string(),
            "sync call `newID` returned no response"
        );
    }
}
