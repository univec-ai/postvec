//! Parsing and rendering of the two list-shaped settings the CLI touches.
//!
//! They are not the same grammar:
//! - `shared_preload_libraries` is file names (`SplitDirectoriesString`):
//!   comma-separated, optional quotes, no case folding. `MyLib` names a
//!   file called `MyLib`. Folding it would point a working cluster at a
//!   library that does not exist.
//! - `postvec.database`, `postvec.grpc_endpoints`, `postvec.http_endpoints`
//!   and `postvec.embedded_models` are `split(',')` + trim in the
//!   extension, with no quoting.
//!
//! [`validate_library_list`] is the write path; the lenient splitter is
//! only for reading broken configs.

/// The library name postvec is loaded under, and the `$libdir`-qualified form
/// PostgreSQL also accepts.
pub const POSTVEC_LIBRARY: &str = "postvec";
const POSTVEC_LIBDIR_FORM: &str = "$libdir/postvec";

/// Split a `shared_preload_libraries` value into its items, leniently.
///
/// A state machine, because a quoted item may itself contain a comma.
///
/// Lenient on purpose: it recovers items from input the postmaster would
/// reject (an unterminated quote, an empty item). That is the right
/// behaviour for reading: `doctor` must be able to describe a cluster whose
/// configuration is broken. Every path that renders a list back into a file
/// validates it first with [`validate_library_list`].
pub fn parse_library_list(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = raw.chars().peekable();
    loop {
        // Skip whitespace and empty items.
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        if chars.peek() == Some(&'"') {
            chars.next();
            let mut item = String::new();
            loop {
                match chars.next() {
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            item.push('"');
                        } else {
                            break;
                        }
                    }
                    Some(c) => item.push(c),
                    // Unterminated quote: take what we have, as the server's
                    // parser would report an error and we only need to avoid
                    // losing the operator's other entries.
                    None => break,
                }
            }
            if !item.is_empty() {
                items.push(item);
            }
        } else {
            let mut item = String::new();
            while let Some(c) = chars.peek() {
                if *c == ',' {
                    break;
                }
                item.push(*c);
                chars.next();
            }
            // No case folding: these are file names (see the module comment).
            let item = item.trim();
            if !item.is_empty() {
                items.push(item.to_string());
            }
        }
    }
    items
}

/// Whether postvec is an item of a parsed preload list. Exact item match:
/// `my_postvec_test` is a different library.
pub fn list_contains_postvec(items: &[String]) -> bool {
    items
        .iter()
        .any(|i| i == POSTVEC_LIBRARY || i == POSTVEC_LIBDIR_FORM)
}

/// Append postvec to a preload list unless it is already there, preserving the
/// existing order (load order can matter to other extensions).
pub fn merge_postvec(items: &[String]) -> Vec<String> {
    let mut merged = items.to_vec();
    if !list_contains_postvec(&merged) {
        merged.push(POSTVEC_LIBRARY.to_string());
    }
    merged
}

/// Remove postvec from a preload list, in both accepted spellings.
pub fn remove_postvec(items: &[String]) -> Vec<String> {
    items
        .iter()
        .filter(|i| *i != POSTVEC_LIBRARY && *i != POSTVEC_LIBDIR_FORM)
        .cloned()
        .collect()
}

/// Render a library list canonically. An item is quoted when it would not
/// survive the server's parser unchanged — a comma, whitespace, a quote, or a
/// character outside the plain file-name set. Case alone needs no quoting:
/// unquoted items are not folded.
pub fn render_library_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| {
            if needs_quoting(item) {
                format!("\"{}\"", item.replace('"', "\"\""))
            } else {
                item.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn needs_quoting(item: &str) -> bool {
    item.is_empty()
        || item.chars().any(|c| {
            !(c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '/' | '.' | '-' | '+' | '~'))
        })
}

/// Why a `shared_preload_libraries` value is not something the postmaster
/// would accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// `"foo` — the postmaster reports "invalid list syntax".
    UnterminatedQuote,
    /// `"foo"bar` — a closing quote must be followed by a comma or the end.
    TextAfterQuote,
    /// `a,,b`, `,a`, `a,` — an empty item anywhere is a syntax error.
    EmptyItem,
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            Self::UnterminatedQuote => "a quoted item is never closed",
            Self::TextAfterQuote => "text follows a closing quote",
            Self::EmptyItem => "an item is empty (a doubled, leading or trailing comma)",
        };
        write!(
            f,
            "invalid shared_preload_libraries syntax: {detail}. \
             PostgreSQL would refuse to start with this value"
        )
    }
}

/// Accept exactly what the postmaster accepts.
///
/// The lenient parser above recovers items from a malformed value. Writing
/// that recovery back would turn a configuration the server refuses into one
/// it accepts, and the operator would not learn that what they wrote was
/// wrong. Anything sourced from outside is checked against the real grammar
/// first, and a malformed value is refused with the reason.
///
/// The corpus in the tests was produced by starting PostgreSQL 18 with each
/// value and recording whether it reported `invalid list syntax`.
pub fn validate_library_list(raw: &str) -> Result<(), ListError> {
    // An empty (or whitespace-only) setting is legal — it means "preload
    // nothing" — but a value that contains only separators is not.
    if raw.trim().is_empty() {
        return if raw.contains(',') {
            Err(ListError::EmptyItem)
        } else {
            Ok(())
        };
    }

    for item in split_top_level(raw) {
        let item = item.trim();
        if item.is_empty() {
            return Err(ListError::EmptyItem);
        }
        if !item.starts_with('"') {
            continue;
        }
        // A quoted item: find its closing quote, honouring `""` escapes, then
        // require nothing but whitespace after it.
        let mut chars = item.char_indices().skip(1);
        let mut closed = None;
        while let Some((index, c)) = chars.next() {
            if c != '"' {
                continue;
            }
            if item[index + 1..].starts_with('"') {
                chars.next();
                continue;
            }
            closed = Some(index);
            break;
        }
        match closed {
            None => return Err(ListError::UnterminatedQuote),
            Some(index) if !item[index + 1..].trim().is_empty() => {
                return Err(ListError::TextAfterQuote)
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Split on commas that are outside quotes, returning the raw items —
/// including empty ones, which is the whole point.
fn split_top_level(raw: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (index, c) in raw.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                items.push(&raw[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(&raw[start..]);
    items
}

/// Parse a comma-separated postvec list GUC exactly as the extension does.
pub fn parse_extension_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Render a comma-separated postvec list GUC. No quoting exists in this
/// grammar; callers must have rejected commas in the values already
/// (`validate::database_name`, `validate::model_list`).
pub fn render_extension_list(items: &[String]) -> String {
    items.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_lists() {
        assert_eq!(
            parse_library_list("pg_stat_statements,postvec"),
            ["pg_stat_statements", "postvec"]
        );
        assert_eq!(
            parse_library_list("  pg_stat_statements ,  postvec  "),
            ["pg_stat_statements", "postvec"]
        );
        assert_eq!(parse_library_list(""), Vec::<String>::new());
        assert_eq!(parse_library_list("   "), Vec::<String>::new());
        // Lenient on purpose — see the function's own comment; the strict
        // grammar lives in `validate_library_list`.
        assert_eq!(parse_library_list(",,a,,"), ["a"]);
    }

    /// `shared_preload_libraries` holds file names, so case is significant.
    /// Folding `MyLib` to `mylib` and writing that back would point a working
    /// cluster at a library that does not exist. (Verified against
    /// PostgreSQL 18: `MyLib` makes the postmaster look for a file `MyLib`.)
    #[test]
    fn unquoted_items_keep_their_case() {
        assert_eq!(parse_library_list("MyLib"), ["MyLib"]);
        assert_eq!(
            parse_library_list("PG_Stat_Statements,postvec"),
            ["PG_Stat_Statements", "postvec"]
        );
        // …and therefore `PostVec` is a *different* library from `postvec`.
        assert!(!list_contains_postvec(&parse_library_list("PostVec")));
        assert!(list_contains_postvec(&parse_library_list("postvec")));
    }

    /// The corpus: each value was fed to a real PostgreSQL 18 postmaster and
    /// classified by whether it reported `invalid list syntax`.
    #[test]
    fn validation_matches_the_server() {
        for accepted in [
            "",
            "   ",
            "postvec",
            "pg_stat_statements,postvec",
            " pg_stat_statements , postvec ",
            "\"MyLib\"",
            "\"my,lib\",postvec",
            "\"my\"\"lib\"",
            "$libdir/postvec",
            "PG_Stat_Statements",
        ] {
            assert_eq!(
                validate_library_list(accepted),
                Ok(()),
                "PostgreSQL accepts {accepted:?}"
            );
        }

        for (rejected, why) in [
            ("\"postvec", ListError::UnterminatedQuote),
            ("\"pg_stat\"statements", ListError::TextAfterQuote),
            ("a,,b", ListError::EmptyItem),
            (",postvec", ListError::EmptyItem),
            ("postvec,", ListError::EmptyItem),
            (",", ListError::EmptyItem),
            ("postvec,,", ListError::EmptyItem),
        ] {
            assert_eq!(
                validate_library_list(rejected),
                Err(why),
                "PostgreSQL rejects {rejected:?}"
            );
        }
    }

    /// The behaviour the validator exists to prevent: parsing alone would turn
    /// a value the server refuses into one it accepts, with nothing said.
    #[test]
    fn a_malformed_value_is_never_silently_repaired() {
        let broken = "postvec,,";
        assert_eq!(
            render_library_list(&merge_postvec(&parse_library_list(broken))),
            "postvec",
            "the lenient parser would repair it…"
        );
        assert!(
            validate_library_list(broken).is_err(),
            "…which is exactly why writing paths must validate first"
        );
    }

    #[test]
    fn preserves_quoted_items_verbatim() {
        assert_eq!(
            parse_library_list("\"Weird,Lib\",postvec"),
            ["Weird,Lib", "postvec"]
        );
        assert_eq!(parse_library_list("\"a\"\"b\""), ["a\"b"]);
        assert_eq!(parse_library_list("\"PostVec\""), ["PostVec"]);
    }

    #[test]
    fn similar_names_are_not_postvec() {
        for value in ["my_postvec_test", "postvec_extra", "prepostvec", "postve"] {
            assert!(
                !list_contains_postvec(&parse_library_list(value)),
                "{value:?} must not count as postvec"
            );
        }
        assert!(list_contains_postvec(&parse_library_list(
            "$libdir/postvec"
        )));
    }

    #[test]
    fn merging_is_idempotent_and_order_preserving() {
        let base = parse_library_list("pg_stat_statements,auto_explain");
        let merged = merge_postvec(&base);
        assert_eq!(merged, ["pg_stat_statements", "auto_explain", "postvec"]);
        assert_eq!(
            merge_postvec(&merged),
            merged,
            "second merge changes nothing"
        );
        assert_eq!(
            merge_postvec(&parse_library_list("postvec,pg_stat_statements")),
            ["postvec", "pg_stat_statements"]
        );
    }

    #[test]
    fn removal_handles_both_spellings() {
        assert_eq!(
            remove_postvec(&parse_library_list("a,postvec,b")),
            ["a", "b"]
        );
        assert_eq!(
            remove_postvec(&parse_library_list("a,$libdir/postvec")),
            ["a"]
        );
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        for input in [
            "pg_stat_statements,postvec",
            "\"Weird,Lib\",postvec",
            "\"Mixed Case\",a-b.c,$libdir/postvec",
            "MyLib,postvec",
        ] {
            let parsed = parse_library_list(input);
            let rendered = render_library_list(&parsed);
            assert_eq!(
                parse_library_list(&rendered),
                parsed,
                "rendering {input:?} as {rendered:?} lost information"
            );
        }
    }

    #[test]
    fn rendering_quotes_only_when_required() {
        assert_eq!(
            render_library_list(&["pg_stat_statements".into(), "postvec".into()]),
            "pg_stat_statements,postvec"
        );
        // Case alone needs no quoting: unquoted items are not folded.
        assert_eq!(render_library_list(&["MyLib".into()]), "MyLib");
        assert_eq!(render_library_list(&["a b".into()]), "\"a b\"");
        assert_eq!(
            render_library_list(&["$libdir/postvec".into()]),
            "$libdir/postvec"
        );
    }

    #[test]
    fn extension_lists_use_the_extensions_own_grammar() {
        assert_eq!(parse_extension_list(" a , b ,, "), ["a", "b"]);
        // No quoting: a quoted name would be taken literally by the extension.
        assert_eq!(parse_extension_list("\"a\""), ["\"a\""]);
        assert_eq!(render_extension_list(&["a".into(), "b".into()]), "a,b");
    }
}
