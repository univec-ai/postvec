// SPDX-License-Identifier: BUSL-1.1

//! Rewrites `postvec.search(rel, col, text, ...)` and `postvec.embed(text,
//! model)` into their vector-taking forms. Everything else in the statement
//! is left byte for byte.

use postvec_core::registry::serialize_vector;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Arg {
    Literal(String),
    Param(u16),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Call {
    start: usize,
    end: usize,
    pub embed: bool,
    /// `(relation, column)` for search, `model` for embed.
    pub relation: String,
    pub column: String,
    pub text: Arg,
    rest: String,
}

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Ident(String),
    Str(String),
    Param(u16),
    Punct(&'a str),
}

const HINT: &str = "Use literals or $n parameters for postvec.search(relation, column, text) and postvec.embed(text, model) through the proxy; other forms need search_with_vector().";

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
            i = sql[i..].find('\n').map_or(b.len(), |n| i + n);
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
            let mut s = String::new();
            loop {
                let Some(&ch) = b.get(i) else {
                    return Err("unterminated string".into());
                };
                if ch == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        s.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else if ch == b'\\' && escapes {
                    let e = *b.get(i + 1).ok_or("unterminated string")?;
                    i += 2;
                    s.push(match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'\\' | b'\'' | b'"' => e as char,
                        _ => return Err("unsupported escape in E'' string".into()),
                    });
                } else {
                    let ch = sql[i..].chars().next().unwrap();
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
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

/// One argument: a single string literal or `$n`, optionally cast.
fn arg(toks: &[(Tok<'_>, usize, usize)]) -> Option<Arg> {
    let value = match toks.first()?.0 {
        Tok::Str(ref s) => Arg::Literal(s.clone()),
        Tok::Param(n) => Arg::Param(n),
        _ => return None,
    };
    match toks.len() {
        1 => Some(value),
        3 if toks[1].0 == Tok::Punct("::") && matches!(toks[2].0, Tok::Ident(_)) => Some(value),
        _ => None,
    }
}

fn literal(toks: &[(Tok<'_>, usize, usize)]) -> Option<String> {
    match arg(toks)? {
        Arg::Literal(s) => Some(s),
        Arg::Param(_) => None,
    }
}

/// Every rewritable call in `sql`, in source order. An error names the first
/// call that cannot be rewritten; SQL the lexer cannot follow is left alone.
pub(super) fn scan_with_strings(sql: &str, standard_strings: bool) -> Result<Vec<Call>, String> {
    let Ok(toks) = lex(sql, standard_strings) else {
        return Ok(Vec::new());
    };
    let mut calls = Vec::new();
    let mut i = 0;
    while i + 3 < toks.len() {
        let (embed, open) = match (&toks[i].0, &toks[i + 1].0, &toks[i + 2].0, &toks[i + 3].0) {
            (Tok::Ident(s), Tok::Punct("."), Tok::Ident(f), Tok::Punct("("))
                if s == "postvec" && (f == "search" || f == "embed") =>
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
        let close = loop {
            let t = toks.get(j).ok_or("unterminated call")?;
            match t.0 {
                Tok::Punct("(") | Tok::Punct("[") => depth += 1,
                Tok::Punct(")") | Tok::Punct("]") if depth > 0 => depth -= 1,
                Tok::Punct(")") => {
                    args.push(&toks[from..j]);
                    break j;
                }
                Tok::Punct(",") if depth == 0 => {
                    args.push(&toks[from..j]);
                    from = j + 1;
                }
                _ => {}
            }
            j += 1;
        };
        let name = if embed { "embed" } else { "search" };
        let unsupported =
            || format!("postvec.{name}() arguments are not supported by the proxy. {HINT}");
        let call = if embed {
            let [text, model] = args[..] else {
                return Err(unsupported());
            };
            Call {
                start: toks[i].1,
                end: toks[close].2,
                embed,
                relation: literal(model).ok_or_else(unsupported)?,
                column: String::new(),
                text: arg(text).ok_or_else(unsupported)?,
                rest: String::new(),
            }
        } else {
            let [rel, col, text, ..] = args[..] else {
                return Err(unsupported());
            };
            Call {
                start: toks[i].1,
                end: toks[close].2,
                embed,
                relation: literal(rel).ok_or_else(unsupported)?,
                column: literal(col).ok_or_else(unsupported)?,
                text: arg(text).ok_or_else(unsupported)?,
                rest: args
                    .get(3)
                    .and_then(|a| a.first())
                    .map(|first| format!(", {}", &sql[first.1..toks[close].1]))
                    .unwrap_or_default(),
            }
        };
        calls.push(call);
        i = close + 1;
    }
    Ok(calls)
}

/// Substitute each call with its vector form. `None` leaves a typed NULL,
/// used for the Parse of a statement whose text arrives at Bind.
pub(super) fn render(sql: &str, calls: &[Call], vectors: &[Option<Vec<f32>>]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    for (call, vector) in calls.iter().zip(vectors) {
        out.push_str(&sql[cursor..call.start]);
        let vector = match vector {
            Some(v) => format!("ARRAY{}::real[]", serialize_vector(v)),
            None => "NULL::real[]".into(),
        };
        if call.embed {
            match call.text {
                Arg::Param(n) => out.push_str(&format!(
                    "CASE WHEN ${n}::text IS NULL THEN NULL::real[] ELSE {vector} END"
                )),
                _ => out.push_str(&vector),
            }
        } else {
            let text = match &call.text {
                Arg::Literal(s) => quote(s),
                Arg::Param(n) => format!("${n}"),
            };
            out.push_str(&format!(
                "postvec.search_with_vector({}, {}, {vector}, {text}{})",
                quote(&call.relation),
                quote(&call.column),
                call.rest
            ));
        }
        cursor = call.end;
    }
    out.push_str(&sql[cursor..]);
    out
}

fn quote(s: &str) -> String {
    postvec_core::registry::quote_literal_estring(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(sql: &str) -> Result<Vec<Call>, String> {
        scan_with_strings(sql, true)
    }

    #[test]
    fn honors_legacy_string_escaping() {
        let sql = "SELECT postvec.embed('line\\nnext', 'm')";
        assert_eq!(
            scan_with_strings(sql, false).unwrap()[0].text,
            Arg::Literal("line\nnext".into())
        );
        assert_eq!(
            scan(sql).unwrap()[0].text,
            Arg::Literal("line\\nnext".into())
        );
    }

    #[test]
    fn rewrites_literals_and_params() {
        let sql = "SELECT * FROM postvec.search('public.docs', 'body', E'reset\\'s', limit_n => 5, filter => '{\"a\":1}') s, postvec.embed($1, 'm') /* postvec.search( */ -- x\nWHERE 'postvec.search(' <> $$postvec.embed($$";
        let calls = scan(sql).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].relation, "public.docs");
        assert_eq!(calls[0].text, Arg::Literal("reset's".into()));
        assert_eq!(calls[1].text, Arg::Param(1));
        assert_eq!(calls[1].relation, "m");
        let out = render(sql, &calls, &[Some(vec![1.0, 2.5]), None]);
        assert!(out.starts_with("SELECT * FROM postvec.search_with_vector(E'public.docs', E'body', ARRAY[1,2.5]::real[], E'reset\\'s', limit_n => 5, filter => '{\"a\":1}') s, CASE WHEN $1::text IS NULL THEN NULL::real[] ELSE NULL::real[] END /*"));
        assert!(out.ends_with("WHERE 'postvec.search(' <> $$postvec.embed($$"));
    }

    #[test]
    fn rejects_expressions_and_accepts_casts() {
        assert!(scan("SELECT postvec.search(t.name, 'body', 'x') FROM t").is_err());
        assert!(scan("SELECT postvec.search('d', 'body', lower('x'))").is_err());
        assert!(scan("SELECT postvec.search('d', 'body')").is_err());
        assert!(scan("SELECT postvec.embed(ARRAY['a'], 'm')").is_err());
        assert!(scan("SELECT postvec.search('d', 'body', 'x'").is_err());
        assert!(scan("SELECT E'\\x41', postvec.search('d', 'body', 'x')")
            .unwrap()
            .is_empty());
        let calls = scan("SELECT \"postvec\".SEARCH('d'::text, 'body', $2::text)").unwrap();
        assert_eq!(calls[0].text, Arg::Param(2));
        assert!(scan("SELECT 1").unwrap().is_empty());
        assert!(scan("SELECT postvec.status()").unwrap().is_empty());
    }
}
