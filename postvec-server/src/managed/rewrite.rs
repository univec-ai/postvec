// SPDX-License-Identifier: BUSL-1.1

//! Appends the proxy's embedding to `postvec.search(rel, col, text, ...)` and
//! `postvec.embed(text, model)` as their `query_vector` / `vector` argument.
//! The database still runs the functions the client named, under the
//! client's role and privileges; everything else is left byte for byte.

use postvec_core::registry::serialize_vector;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Arg {
    Literal(String),
    Param(u16),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Call {
    start: usize,
    pub end: usize,
    /// Where the closing parenthesis starts: the vector argument goes here.
    close: usize,
    pub embed: bool,
    /// `(relation, column)` for search, `model` for embed.
    pub relation: String,
    pub column: String,
    pub text: Arg,
    /// An undeclared `$n` text reads as sent: bare (the function's argument
    /// makes it text), or cast to text here and wherever else it appears.
    pub untyped: bool,
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Ident(String),
    Str(String),
    Param(u16),
    Punct(&'a str),
}

fn lex(sql: &str, standard_strings: bool) -> Result<Vec<(Tok<'_>, usize, usize)>, String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"--") {
            i = sql[i..].find(['\n', '\r']).map_or(b.len(), |n| i + n);
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated comment".into());
            }
        } else if c == b'\'' || ((c == b'E' || c == b'e') && b.get(i + 1) == Some(&b'\'')) {
            let escapes = c != b'\'' || !standard_strings;
            i += if c == b'\'' { 1 } else { 2 };
            // Bytes: octal and hex escapes may spell UTF-8 byte by byte.
            let mut s = Vec::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err("unterminated string".into());
                };
                if ch == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        s.push(b'\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else if ch == b'\\' && escapes {
                    let e = *b.get(i + 1).ok_or("unterminated string")?;
                    i += 2;
                    let digits = |i: usize, max: usize, radix: u32| {
                        b[i..]
                            .iter()
                            .take(max)
                            .take_while(|d| (**d as char).is_digit(radix))
                            .count()
                    };
                    let number = |from: usize, n: usize, radix: u32| {
                        u32::from_str_radix(&sql[from..from + n], radix).unwrap()
                    };
                    match e {
                        b'n' => s.push(b'\n'),
                        b't' => s.push(b'\t'),
                        b'r' => s.push(b'\r'),
                        b'b' => s.push(8),
                        b'f' => s.push(12),
                        b'0'..=b'7' => {
                            let n = 1 + digits(i, 2, 8);
                            s.push(number(i - 1, n, 8) as u8);
                            i += n - 1;
                        }
                        b'x' if digits(i, 2, 16) > 0 => {
                            let n = digits(i, 2, 16);
                            s.push(number(i, n, 16) as u8);
                            i += n;
                        }
                        b'u' | b'U' => {
                            let n = if e == b'u' { 4 } else { 8 };
                            if digits(i, n, 16) != n {
                                return Err("invalid Unicode escape".into());
                            }
                            let c =
                                char::from_u32(number(i, n, 16)).ok_or("invalid Unicode escape")?;
                            s.extend(c.to_string().bytes());
                            i += n;
                        }
                        // Any other character stands for itself.
                        _ => {
                            let c = sql[i - 1..].chars().next().unwrap();
                            s.extend(c.to_string().bytes());
                            i += c.len_utf8() - 1;
                        }
                    }
                } else {
                    s.push(ch);
                    i += 1;
                }
            }
            let s = String::from_utf8(s).map_err(|_| "string is not valid UTF-8")?;
            out.push((Tok::Str(s), start, i));
        } else if c == b'"' {
            i += 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err("unterminated identifier".into());
                };
                if ch == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        s.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            out.push((Tok::Ident(s), start, i));
        } else if c == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let n = sql[i + 1..].bytes().take_while(u8::is_ascii_digit).count();
            let value: u16 = sql[i + 1..i + 1 + n]
                .parse()
                .map_err(|_| "parameter number out of range")?;
            i += 1 + n;
            out.push((Tok::Param(value), start, i));
        } else if c == b'$' {
            let n = sql[i + 1..]
                .bytes()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
                .count();
            if b.get(i + 1 + n) != Some(&b'$') {
                return Err("unexpected $".into());
            }
            let tag = &sql[i..i + n + 2];
            let body = i + tag.len();
            let close = sql[body..]
                .find(tag)
                .ok_or("unterminated dollar-quoted string")?;
            i = body + close + tag.len();
            out.push((Tok::Str(sql[body..body + close].into()), start, i));
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Tok::Ident(sql[start..i].to_ascii_lowercase()), start, i));
        } else {
            let n = if b[i..].starts_with(b"::") || b[i..].starts_with(b"=>") {
                2
            } else {
                1
            };
            i += n;
            out.push((Tok::Punct(&sql[start..i]), start, i));
        }
    }
    Ok(out)
}

type Toks<'a, 'b> = &'b [(Tok<'a>, usize, usize)];

/// One argument: a string literal or `$n`, in redundant parentheses or cast
/// to text (`text`, `varchar`, `character varying`, never a length that
/// truncates), so the value the proxy reads is the value the database
/// evaluates. Also whether it carries a cast.
fn arg(toks: Toks) -> Option<(Arg, bool)> {
    // Iterative: nesting depth is the client's to choose.
    let open = toks.iter().take_while(|t| t.0 == Tok::Punct("(")).count();
    let value = match &toks.get(open)?.0 {
        Tok::Str(s) => Arg::Literal(s.clone()),
        Tok::Param(n) => Arg::Param(*n),
        _ => return None,
    };
    let (mut rest, mut depth, mut cast) = (&toks[open + 1..], open, false);
    while let Some(t) = rest.first() {
        if t.0 == Tok::Punct(")") && depth > 0 {
            depth -= 1;
            rest = &rest[1..];
        } else {
            rest = &rest[text_cast(rest)?..];
            cast = true;
        }
    }
    (depth == 0).then_some((value, cast))
}

/// The length of a leading `::text`-like cast.
fn text_cast(toks: Toks) -> Option<usize> {
    let n = match toks {
        [(Tok::Punct("::"), ..), (Tok::Ident(a), ..), (Tok::Ident(b), ..), ..]
            if a == "character" && b == "varying" =>
        {
            3
        }
        [(Tok::Punct("::"), ..), (Tok::Ident(t), ..), ..] if t == "text" || t == "varchar" => 2,
        _ => return None,
    };
    (toks.get(n).map(|t| &t.0) != Some(&Tok::Punct("("))).then_some(n)
}

fn literal(toks: Toks) -> Option<String> {
    match arg(toks)?.0 {
        Arg::Literal(s) => Some(s),
        Arg::Param(_) => None,
    }
}

/// Every rewritable call in `sql`, in source order. Other forms (nested
/// expressions, function signatures in DDL) and SQL the lexer cannot follow
/// are left for the database, whose `search()`/`embed()` stubs explain.
pub(super) fn scan_with_strings(sql: &str, standard_strings: bool) -> Vec<Call> {
    let Ok(toks) = lex(sql, standard_strings) else {
        return Vec::new();
    };
    let mut calls = Vec::new();
    let mut i = 0;
    while i + 3 < toks.len() {
        let (embed, open) = match (&toks[i].0, &toks[i + 1].0, &toks[i + 2].0, &toks[i + 3].0) {
            (Tok::Ident(s), Tok::Punct("."), Tok::Ident(f), Tok::Punct("("))
                if s == "postvec"
                    && (f == "search" || f == "embed")
                    && (i == 0 || toks[i - 1].0 != Tok::Punct(".")) =>
            {
                (f == "embed", i + 3)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let mut depth = 0;
        let mut args: Vec<&[(Tok<'_>, usize, usize)]> = Vec::new();
        let mut from = open + 1;
        let mut j = open + 1;
        let Some(close) = (loop {
            let Some(t) = toks.get(j) else {
                break None;
            };
            match t.0 {
                Tok::Punct("(") | Tok::Punct("[") => depth += 1,
                Tok::Punct(")") | Tok::Punct("]") if depth > 0 => depth -= 1,
                Tok::Punct(")") => {
                    args.push(&toks[from..j]);
                    break Some(j);
                }
                Tok::Punct(",") if depth == 0 => {
                    args.push(&toks[from..j]);
                    from = j + 1;
                }
                _ => {}
            }
            j += 1;
        }) else {
            break;
        };
        let call = || {
            let (relation, column, text) = if embed {
                let [text, model] = args[..] else {
                    return None;
                };
                (literal(model)?, String::new(), text)
            } else {
                let [rel, col, text, ..] = args[..] else {
                    return None;
                };
                (literal(rel)?, literal(col)?, text)
            };
            let (value, cast) = arg(text)?;
            let span = (text.first()?.1, text.last()?.2);
            Some((
                Call {
                    start: toks[i].1,
                    end: toks[close].2,
                    close: toks[close].1,
                    embed,
                    relation,
                    column,
                    text: value,
                    untyped: !cast,
                },
                span,
            ))
        };
        calls.extend(call());
        i = close + 1;
    }
    // PostgreSQL types an undeclared `$n` from its first use, so a cast in
    // the call proves text only if every use is a call's text or a cast to text.
    let spans: Vec<_> = calls
        .iter()
        .map(|(c, span)| (c.text.clone(), *span))
        .collect();
    calls
        .into_iter()
        .map(|(mut call, _)| {
            if let (false, Arg::Param(n)) = (call.untyped, &call.text) {
                call.untyped = toks.iter().enumerate().all(|(j, t)| {
                    t.0 != Tok::Param(*n)
                        || text_cast(&toks[j + 1..]).is_some()
                        || spans
                            .iter()
                            .any(|(a, s)| *a == call.text && s.0 <= t.1 && t.2 <= s.1)
                });
            }
            call
        })
        .collect()
}

/// A vector as a SQL literal, or a typed NULL.
pub(super) fn literal_vector(vector: Option<&[f32]>) -> String {
    vector.map_or("NULL::real[]".into(), |v| {
        format!("ARRAY{}::real[]", serialize_vector(v))
    })
}

/// The highest `$n` the statement references.
pub(super) fn max_param(sql: &str, standard_strings: bool) -> u16 {
    lex(sql, standard_strings)
        .map(|toks| {
            toks.iter()
                .filter_map(|t| match t.0 {
                    Tok::Param(n) => Some(n),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Prepared statements the SQL creates or deallocates by name, in statement
/// order: `PREPARE name`, `DEALLOCATE [PREPARE] name`. `ALL` forms are left
/// to their command tags.
pub(super) fn lifecycle(sql: &str, standard_strings: bool) -> Vec<String> {
    let Ok(toks) = lex(sql, standard_strings) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for (i, (tok, ..)) in toks.iter().enumerate() {
        let Tok::Ident(word) = tok else { continue };
        let first = i == 0 || toks[i - 1].0 == Tok::Punct(";");
        if !first || !(word == "prepare" || word == "deallocate") {
            continue;
        }
        let mut next = toks[i + 1..].iter().map(|t| &t.0);
        let name = match next.next() {
            Some(Tok::Ident(p)) if word == "deallocate" && p == "prepare" => next.next(),
            other => other,
        };
        match name {
            Some(Tok::Ident(n)) if n != "all" && !(word == "prepare" && n == "transaction") => {
                names.push(n.clone())
            }
            _ => {}
        }
    }
    names
}

/// Where the statement holding the first call begins: just after the last
/// top-level `;` before it.
pub(super) fn statement_start(sql: &str, calls: &[Call], standard_strings: bool) -> usize {
    let at = calls.first().map_or(0, |c| c.start);
    lex(sql, standard_strings)
        .ok()
        .and_then(|toks| {
            toks.iter()
                .rev()
                .find(|(t, _, end)| *t == Tok::Punct(";") && *end <= at)
                .map(|t| t.2)
        })
        .unwrap_or(0)
}

/// Pass each call its vector: `vectors[i]` is the SQL expression for call `i`.
pub(super) fn render(sql: &str, calls: &[Call], vectors: &[String]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    for (call, vector) in calls.iter().zip(vectors) {
        out.push_str(&sql[cursor..call.close]);
        let name = if call.embed { "vector" } else { "query_vector" };
        out.push_str(&format!(", {name} => {vector}"));
        cursor = call.close;
    }
    out.push_str(&sql[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(sql: &str) -> Vec<Call> {
        scan_with_strings(sql, true)
    }

    #[test]
    fn honors_legacy_string_escaping() {
        let sql = "SELECT postvec.embed('line\\nnext', 'm')";
        assert_eq!(
            scan_with_strings(sql, false)[0].text,
            Arg::Literal("line\nnext".into())
        );
        assert_eq!(scan(sql)[0].text, Arg::Literal("line\\nnext".into()));
    }

    #[test]
    fn rewrites_literals_and_params() {
        let sql = "SELECT * FROM postvec.search('public.docs', 'body', E'reset\\'s', limit_n => 5, filter => '{\"a\":1}') s, postvec.embed($1, 'm') /* postvec.search( */ -- x\nWHERE 'postvec.search(' <> $$postvec.embed($$";
        let calls = scan(sql);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].relation, "public.docs");
        assert_eq!(calls[0].text, Arg::Literal("reset's".into()));
        assert_eq!(calls[1].text, Arg::Param(1));
        assert_eq!(calls[1].relation, "m");
        let out = render(
            sql,
            &calls,
            &[literal_vector(Some(&[1.0, 2.5])), literal_vector(None)],
        );
        assert!(out.starts_with("SELECT * FROM postvec.search('public.docs', 'body', E'reset\\'s', limit_n => 5, filter => '{\"a\":1}', query_vector => ARRAY[1,2.5]::real[]) s, postvec.embed($1, 'm', vector => NULL::real[]) /*"));
        assert!(out.ends_with("WHERE 'postvec.search(' <> $$postvec.embed($$"));
    }

    #[test]
    fn leaves_other_forms_and_accepts_casts() {
        for sql in [
            "SELECT postvec.search(t.name, 'body', 'x') FROM t",
            "SELECT postvec.search('d', 'body', lower('x'))",
            "SELECT postvec.search('d', 'body')",
            "SELECT postvec.embed(ARRAY['a'], 'm')",
            "SELECT postvec.embed('abcd'::char, 'm')",
            "SELECT postvec.search('d', 'body', $1::integer)",
            "SELECT postvec.search('d', 'body', 'x'",
            "SELECT postvec.embed('abcd'::varchar(2), 'm')",
            "SELECT postvec.embed(('abcd')::character varying(2), 'm')",
            "SELECT postvec.embed(E'\\u12', 'm')",
            "GRANT EXECUTE ON FUNCTION postvec.embed(text, text) TO app",
            "SELECT app.postvec.embed('x', 'm')",
            "SELECT 1",
            "SELECT postvec.status()",
        ] {
            assert!(scan(sql).is_empty(), "{sql}");
        }
        assert_eq!(scan("SELECT 1 -- x\rFROM postvec.embed('x', 'm')").len(), 1);
        let calls = scan("SELECT \"postvec\".SEARCH('d'::text, 'body', $2::text)");
        assert_eq!(calls[0].text, Arg::Param(2));
        // Any nesting depth is scanned without recursion.
        let deep = format!(
            "SELECT postvec.embed({}$1{}, 'm')",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        assert_eq!(scan(&deep)[0].text, Arg::Param(1));
        let unbalanced = format!(
            "SELECT postvec.embed({}$1{}, 'm')",
            "(".repeat(3),
            ")".repeat(2)
        );
        assert!(scan(&unbalanced).is_empty());
        // Redundant parentheses, spelled-out casts, escapes elsewhere.
        for (sql, text) in [
            ("SELECT postvec.search('d', 'body', ($1))", Arg::Param(1)),
            ("SELECT postvec.embed(($1)::text, 'm')", Arg::Param(1)),
            (
                "SELECT postvec.embed($1::character varying, 'm')",
                Arg::Param(1),
            ),
            (
                "SELECT E'\\x41', postvec.search('d', 'body', 'x')",
                Arg::Literal("x".into()),
            ),
            (
                "SELECT postvec.embed(E'\\303\\xa9\\q\\u00e9', 'm')",
                Arg::Literal("éqé".into()),
            ),
        ] {
            assert_eq!(
                scan(sql).first().map(|c| c.text.clone()),
                Some(text),
                "{sql}"
            );
        }
        // An undeclared `$n` cast to text is text only if no other use types it.
        let untyped = |sql: &str| scan(sql)[0].untyped;
        assert!(untyped("SELECT postvec.embed($1, 'm') WHERE $1 <> ''"));
        assert!(untyped("SELECT postvec.embed($1::text, 'm'), $1::varchar"));
        assert!(!untyped("SELECT $1::integer, postvec.embed($1::text, 'm')"));
        assert!(!untyped(
            "SELECT postvec.embed($1::text, 'm') WHERE id = $1"
        ));
        assert_eq!(
            lifecycle(
                "DEALLOCATE a; deallocate prepare \"B\"; PREPARE c(text) AS SELECT $1; DEALLOCATE ALL; PREPARE TRANSACTION 'x'; SELECT 'prepare d'",
                true
            ),
            ["a", "B", "c"]
        );
        let sql =
            "BEGIN; UPDATE t SET a = ';'; SELECT * FROM postvec.search('d', 'body', 'x'); SELECT 2";
        let at = statement_start(sql, &scan(sql), true);
        assert_eq!(&sql[at..at + 15], " SELECT * FROM ");
    }
}
