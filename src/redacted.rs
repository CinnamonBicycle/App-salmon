//! Wrapper for secret values so they never end up in `Debug`/`tracing` output by accident.
//!
//! Deliberately does not implement `serde::Serialize` — HTTP response DTOs that must reveal a
//! secret (e.g. a freshly generated cluster password) hold a plain `String` and call
//! [`Redacted::expose`] explicitly when building the DTO, so leaking a secret into a log line
//! requires going out of your way rather than happening as a side effect of `#[derive(Debug)]`.

use std::fmt;

/// A secret value that never appears in `Debug`/`Display`/logging output by accident. See the
/// module docs for the intended usage pattern.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted<T>(
    /// The wrapped secret value, never exposed by `Debug`/`Display`.
    T,
);

impl<T> Redacted<T> {
    /// Wraps `value` so it can't leak into `Debug`/`Display`/logging output by accident.
    ///
    /// # Arguments
    ///
    /// - `value`: the secret value to wrap.
    ///
    /// # Returns
    ///
    /// The wrapped value.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Deliberately exposes the wrapped secret — the one sanctioned way to read it back out, so
    /// doing so is visible at the call site rather than an accidental side effect.
    ///
    /// # Returns
    ///
    /// A reference to the wrapped value.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Deliberately exposes the wrapped secret by consuming `self` and returning it by value.
    ///
    /// # Returns
    ///
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Redacted<T> {
    /// Always writes the literal string `Redacted(..)`, regardless of the wrapped value.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Redacted(..)")
    }
}

impl<T> fmt::Display for Redacted<T> {
    /// Always writes the literal string `[REDACTED]`, regardless of the wrapped value.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::Redacted;

    #[test]
    fn debug_never_reveals_the_value() {
        let secret = Redacted::new("super-secret-password".to_string());
        assert_eq!(format!("{secret:?}"), "Redacted(..)");
    }

    #[test]
    fn display_never_reveals_the_value() {
        let secret = Redacted::new("super-secret-password".to_string());
        assert_eq!(format!("{secret}"), "[REDACTED]");
    }

    #[test]
    fn expose_returns_the_real_value() {
        let secret = Redacted::new("super-secret-password".to_string());
        assert_eq!(secret.expose(), "super-secret-password");
    }

    #[test]
    fn into_inner_returns_the_real_value() {
        let secret = Redacted::new(42_i32);
        assert_eq!(secret.into_inner(), 42);
    }

    #[test]
    fn clone_and_eq_work_normally() {
        let a = Redacted::new("x".to_string());
        let b = a.clone();
        assert_eq!(a, b);
    }
}
