//! Small shared helpers: wall-clock time and Python-`%g`-style float formatting.

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock seconds since the Unix epoch, as a float -- the Rust analogue of
/// `time.time()`. Used for age arithmetic against the float mtimes/atimes we read.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Format a float the way Python's `{x:g}` does for the values roomba prints: an
/// integral value renders without a decimal point (`48`, `1`), otherwise the shortest
/// round-tripping form (`47.9`, `0.5`).
pub fn fmt_g(x: f64) -> String {
    if x.is_finite() && x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        // Rust's default float Display already emits the shortest form without
        // trailing zeros, which matches %g for the small hour values we format.
        format!("{x}")
    }
}
