//! Typed errors, exit codes, and secret redaction.
//!
//! Redaction is applied at the boundary where a message is *built*, not where
//! it is printed, so a value that never entered an error string cannot leak
//! through a later formatting path.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
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

/// `registry-schema`'s archive errors carry the same two classes and the
/// same operator-facing fix, so the conversion is total and lossless: the
/// shared reader/writer keeps the exit code its caller would have chosen.
impl From<registry_schema::archive::ArchiveError> for Error {
    fn from(e: registry_schema::archive::ArchiveError) -> Self {
        use registry_schema::archive::ArchiveError;
        match e {
            ArchiveError::Precondition {
                message,
                remediation,
            } => Error::Precondition {
                message,
                remediation,
            },
            ArchiveError::Apply {
                message,
                remediation,
            } => Error::Apply {
                message,
                remediation,
            },
        }
    }
}

impl Error {
    pub fn usage(message: impl Into<String>) -> Self {
        Error::Usage {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn precondition(message: impl Into<String>) -> Self {
        Error::Precondition {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn apply(message: impl Into<String>) -> Self {
        Error::Apply {
            message: message.into(),
            remediation: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::Internal(message.into())
    }

    /// Attach the operator-facing fix for this failure.
    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        let fix = fix.into();
        match &mut self {
            Error::Usage { remediation, .. }
            | Error::Precondition { remediation, .. }
            | Error::Apply { remediation, .. } => *remediation = Some(fix),
            Error::Internal(_) => {}
        }
        self
    }

    pub fn remediation(&self) -> Option<&str> {
        match self {
            Error::Usage { remediation, .. }
            | Error::Precondition { remediation, .. }
            | Error::Apply { remediation, .. } => remediation.as_deref(),
            Error::Internal(_) => None,
        }
    }

    /// Stable machine-readable kind for the JSON error envelope.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Usage { .. } => "usage",
            Error::Precondition { .. } => "precondition",
            Error::Apply { .. } => "apply",
            Error::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Internal(message)
            | Error::Usage { message, .. }
            | Error::Precondition { message, .. }
            | Error::Apply { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Internal(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Internal(format!("json: {e}"))
    }
}

/// Strip anything that could be a credential from a message destined for a
/// human, a log, or JSON output.
///
/// Two independent passes, because secrets reach us in two shapes:
/// `scheme://user:pass@host` userinfo, and `key=value` pairs in connection
/// strings and query parameters.
pub fn redact(message: &str) -> String {
    redact_key_values(&redact_userinfo(message))
}

/// `postgres://alice:s3cret@host/db` -> `postgres://alice:REDACTED@host/db`.
fn redact_userinfo(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let bytes = message.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find "://" and then the authority's end; redact a password inside it.
        if bytes[i..].starts_with(b"://") {
            out.push_str("://");
            i += 3;
            let authority_end = bytes[i..]
                .iter()
                .position(|b| matches!(b, b'/' | b'?' | b'#' | b' ' | b'"' | b'\'' | b')'))
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            let authority = &message[i..authority_end];
            match authority.rsplit_once('@') {
                Some((userinfo, host)) => {
                    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
                    out.push_str(user);
                    out.push_str(":REDACTED@");
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            i = authority_end;
            continue;
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&message[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Keys whose values are never safe to print. The `x-amz-*` entries cover
/// presigned S3 URLs: signed query strings are capabilities and must
/// never reach output. Registry code additionally strips whole query
/// strings before building messages, so this is the second line of
/// defence.
const SECRET_KEYS: [&str; 11] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "sslpassword",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
];

/// `password=hunter2 host=x` -> `password=REDACTED host=x`. Works for
/// libpq keyword strings and URL query parameters alike.
fn redact_key_values(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut redactions: Vec<(usize, usize)> = Vec::new();
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(key) {
            let start = from + rel;
            from = start + key.len();
            // The key must be a whole token, not a substring of another word:
            // `no_password_file` is a filename, not a secret.
            if start > 0 {
                let prev = bytes[start - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
            }
            // Accept both `key=value` (libpq, query strings) and
            // `"key": "value"` (JSON error bodies).
            let mut cursor = from;
            skip(bytes, &mut cursor, |b| b.is_ascii_whitespace() || b == b'"');
            if cursor >= bytes.len() || !matches!(bytes[cursor], b'=' | b':') {
                continue;
            }
            cursor += 1;
            skip(bytes, &mut cursor, |b| b.is_ascii_whitespace() || b == b'"');
            let value_start = cursor;
            while cursor < bytes.len()
                && !matches!(
                    bytes[cursor],
                    b' ' | b'\t' | b'\n' | b'\r' | b'&' | b';' | b'"' | b'\'' | b',' | b'}' | b')'
                )
            {
                cursor += 1;
            }
            if cursor > value_start {
                redactions.push((value_start, cursor));
                from = cursor;
            }
        }
    }
    if redactions.is_empty() {
        return message.to_string();
    }
    redactions.sort_unstable();
    let mut out = String::with_capacity(message.len());
    let mut cursor = 0;
    for (start, end) in redactions {
        if start < cursor {
            continue;
        }
        out.push_str(&message[cursor..start]);
        out.push_str("REDACTED");
        cursor = end;
    }
    out.push_str(&message[cursor..]);
    out
}

fn skip(bytes: &[u8], cursor: &mut usize, predicate: impl Fn(u8) -> bool) {
    while *cursor < bytes.len() && predicate(bytes[*cursor]) {
        *cursor += 1;
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_uri_password() {
        assert_eq!(
            redact("connect failed: postgres://alice:s3cret@db.example:5432/univec"),
            "connect failed: postgres://alice:REDACTED@db.example:5432/univec"
        );
    }

    #[test]
    fn keeps_uri_without_userinfo() {
        assert_eq!(
            redact("GET https://192.0.2.2:22222/config failed"),
            "GET https://192.0.2.2:22222/config failed"
        );
    }

    #[test]
    fn redacts_keyword_values() {
        assert_eq!(
            redact("host=db port=5432 password=hunter2 dbname=univec"),
            "host=db port=5432 password=REDACTED dbname=univec"
        );
        assert_eq!(
            redact("https://h/config?api_key=abc123&x=1"),
            "https://h/config?api_key=REDACTED&x=1"
        );
    }

    #[test]
    fn does_not_redact_similar_words() {
        assert_eq!(
            redact("no_password_file=/etc/x"),
            "no_password_file=/etc/x",
            "`password` inside another identifier is not a secret key"
        );
    }

    #[test]
    fn redacts_json_error_bodies() {
        assert_eq!(
            redact(r#"{"message":"nope","token":"s3cret"}"#),
            r#"{"message":"nope","token":"REDACTED"}"#
        );
        assert_eq!(
            redact(r#"{"password": "hunter2", "host": "db"}"#),
            r#"{"password": "REDACTED", "host": "db"}"#
        );
    }

    #[test]
    fn redacts_multiple_secrets_and_preserves_unicode() {
        assert_eq!(
            redact("naïve token=t1 secret=s2 end"),
            "naïve token=REDACTED secret=REDACTED end"
        );
    }
}
