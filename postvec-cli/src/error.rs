//! Typed errors, exit codes and secret redaction.
//!
//! Redact when the message is built, not when it is printed. A secret that
//! never entered an error string cannot leak through a later format path.

use std::fmt;

pub type Result<T> = std::result::Result<T, CliError>;

/// The stable exit-code contract. Also documented in the CLI's `--help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum Exit {
    /// Command completed; a doctor report contained no FAIL (and no WARN
    /// under `--strict`).
    Success = 0,
    /// Apply failure, postcondition failure, or a doctor FAIL.
    Failure = 1,
    /// Invalid invocation, or a prompt that could not be answered.
    Usage = 2,
    /// Multi-database command where at least one target was not changed.
    Partial = 3,
    /// Changes are valid and in place, but the required restart was deferred.
    RestartRequired = 4,
}

impl Exit {
    pub fn code(self) -> i32 {
        self as i32
    }

    /// Map a recorded exit code back to its variant before the command returns.
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => Exit::Success,
            2 => Exit::Usage,
            3 => Exit::Partial,
            4 => Exit::RestartRequired,
            _ => Exit::Failure,
        }
    }
}

#[derive(Debug)]
pub enum CliError {
    /// Bad invocation or a refused prompt. Exit 2.
    Usage {
        message: String,
        remediation: Option<String>,
    },
    /// A precondition the operator must fix before the command can run.
    Precondition {
        message: String,
        remediation: Option<String>,
    },
    /// Something failed while applying changes.
    Apply {
        message: String,
        remediation: Option<String>,
    },
    /// An unexpected internal failure (IO, protocol, driver).
    Internal(String),
}

/// Archive errors use the same classes and fix text, so this conversion
/// keeps the caller's exit code.
impl From<registry_schema::archive::ArchiveError> for CliError {
    fn from(e: registry_schema::archive::ArchiveError) -> Self {
        use registry_schema::archive::ArchiveError;
        match e {
            ArchiveError::Precondition {
                message,
                remediation,
            } => CliError::Precondition {
                message,
                remediation,
            },
            ArchiveError::Apply {
                message,
                remediation,
            } => CliError::Apply {
                message,
                remediation,
            },
        }
    }
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        CliError::Usage {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn precondition(message: impl Into<String>) -> Self {
        CliError::Precondition {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn apply(message: impl Into<String>) -> Self {
        CliError::Apply {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        CliError::Internal(message.into())
    }

    /// Attach the operator-facing fix for this failure.
    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        let fix = fix.into();
        match &mut self {
            CliError::Usage { remediation, .. }
            | CliError::Precondition { remediation, .. }
            | CliError::Apply { remediation, .. } => *remediation = Some(fix),
            CliError::Internal(_) => {}
        }
        self
    }

    pub fn remediation(&self) -> Option<&str> {
        match self {
            CliError::Usage { remediation, .. }
            | CliError::Precondition { remediation, .. }
            | CliError::Apply { remediation, .. } => remediation.as_deref(),
            CliError::Internal(_) => None,
        }
    }

    pub fn exit(&self) -> Exit {
        match self {
            CliError::Usage { .. } => Exit::Usage,
            _ => Exit::Failure,
        }
    }

    /// Stable machine-readable kind for the JSON error envelope.
    pub fn kind(&self) -> &'static str {
        match self {
            CliError::Usage { .. } => "usage",
            CliError::Precondition { .. } => "precondition",
            CliError::Apply { .. } => "apply",
            CliError::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Internal(message)
            | CliError::Usage { message, .. }
            | CliError::Precondition { message, .. }
            | CliError::Apply { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Internal(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::Internal(format!("json: {e}"))
    }
}

impl From<sqlx::Error> for CliError {
    fn from(e: sqlx::Error) -> Self {
        CliError::Internal(format!("postgres: {}", redact(&e.to_string())))
    }
}

pub use postvec_registry::error::redact;

/// Registry errors use the same classes, so exit codes survive this conversion.
impl From<postvec_registry::error::Error> for CliError {
    fn from(e: postvec_registry::error::Error) -> Self {
        use postvec_registry::error::Error as E;
        match e {
            E::Usage {
                message,
                remediation,
            } => CliError::Usage {
                message,
                remediation,
            },
            E::Precondition {
                message,
                remediation,
            } => CliError::Precondition {
                message,
                remediation,
            },
            E::Apply {
                message,
                remediation,
            } => CliError::Apply {
                message,
                remediation,
            },
            E::Internal(m) => CliError::Internal(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_round_trip() {
        for exit in [
            Exit::Success,
            Exit::Failure,
            Exit::Usage,
            Exit::Partial,
            Exit::RestartRequired,
        ] {
            assert_eq!(Exit::from_code(exit.code()), exit);
        }
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Failure.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Partial.code(), 3);
        assert_eq!(Exit::RestartRequired.code(), 4);
    }
}
