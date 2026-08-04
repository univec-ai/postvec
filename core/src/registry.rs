// SPDX-License-Identifier: PostgreSQL
/// One enabled (table, column) as stored in `postvec.registry`.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: i64,
    pub table_schema: String,
    pub table_name: String,
    pub source_column: String,
    pub vector_column: String,
    /// PK columns in index order; length 1 for a plain PK, >1 for composite.
    pub pk_columns: Vec<String>,
    /// `format_type` of each PK column; cast targets for watermark literals.
    pub pk_types: Vec<String>,
    pub model: String,
    pub dim: i32,
    /// FTS text-search configuration name (regconfig rendered as text).
    pub fts_config: String,
    /// Distance metric: `cosine` | `l2` | `ip`. Drives the pgvector operator
    /// used by `search()` and the opclass used when building the index.
    pub distance: String,
    /// `none` | `queue` | `cursor` | `done`. Cursor is the large-table path.
    pub backfill_mode: String,
    pub backfill_watermark: Option<String>,
    pub state: String,
    /// `statement` | `row` | `none` — `none` is an adopted observed entry
    /// (no DML enqueue triggers; only the TRUNCATE identity sentinel).
    pub trigger_mode: String,
    /// Whether teardown may drop `vector_column`: true for enable()-created
    /// (and migration-finalized) columns, false for adopted ones.
    pub owns_vector_column: bool,
    /// Document-embedding template, exactly as validated at
    /// enable/adopt/set_format time. `None` embeds the raw source column.
    pub format: Option<String>,
    /// `manual` | `immediate` | `auto`. Only `auto` reconciles later.
    pub index_mode: String,
    /// The parked automatic-build failure, if any.
    pub index_error: Option<String>,
    /// `none` (one row, one vector on the source table) or `recursive`
    /// (one row, many chunk vectors in the managed destination).
    pub chunking: String,
    /// Splitter geometry, non-NULL exactly for recursive entries.
    pub chunk_size: Option<i32>,
    pub chunk_overlap: Option<i32>,
    /// Managed destination table/view, non-NULL exactly for recursive
    /// entries. The view is `<destination_table>_view`.
    pub destination_schema: Option<String>,
    pub destination_table: Option<String>,
    pub destination_view: Option<String>,
    /// Destination ownership token, mirrored in exact comments on the table
    /// and view. Destructive teardown requires the match.
    pub destination_token: Option<String>,
}

impl RegistryEntry {
    pub fn qualified_table(&self) -> String {
        format!(
            "{}.{}",
            quote_ident(&self.table_schema),
            quote_ident(&self.table_name)
        )
    }
    pub fn is_recursive(&self) -> bool {
        self.chunking == "recursive"
    }
    pub fn qualified_source_table(&self) -> String {
        self.qualified_table()
    }
    pub fn qualified_vector_table(&self) -> String {
        match (&self.destination_schema, &self.destination_table) {
            (Some(schema), Some(table)) => {
                format!("{}.{}", quote_ident(schema), quote_ident(table))
            }
            _ => self.qualified_table(),
        }
    }
    pub fn qualified_destination_view(&self) -> Option<String> {
        match (&self.destination_schema, &self.destination_view) {
            (Some(schema), Some(view)) => {
                Some(format!("{}.{}", quote_ident(schema), quote_ident(view)))
            }
            _ => None,
        }
    }
    pub fn source_pk_join(&self, source_alias: &str, dest_alias: &str) -> String {
        format!(
            "{}.{} = {dest_alias}.postvec_source_pk",
            source_alias,
            quote_ident(&self.pk_columns[0]),
        )
    }
    pub fn is_composite_pk(&self) -> bool {
        self.pk_columns.len() > 1
    }
    pub fn pk_text_expr(&self, alias: &str) -> String {
        let prefix = if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        };
        if self.is_composite_pk() {
            let cols: Vec<String> = self
                .pk_columns
                .iter()
                .map(|c| format!("{prefix}{}", quote_ident(c)))
                .collect();
            format!("ROW({})::text", cols.join(", "))
        } else {
            format!("{prefix}{}::text", quote_ident(&self.pk_columns[0]))
        }
    }
    fn alias_prefix(alias: &str) -> String {
        if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        }
    }
    pub fn pk_any_clause(&self, alias: &str, param: &str) -> String {
        if self.is_composite_pk() {
            format!("{} = ANY({param})", self.pk_text_expr(alias))
        } else {
            format!(
                "{}{} = ANY({param}::{}[])",
                Self::alias_prefix(alias),
                quote_ident(&self.pk_columns[0]),
                self.pk_types[0],
            )
        }
    }
    pub fn pk_staging_join_clause(
        &self,
        alias: &str,
        staging_alias: &str,
        staging_col: &str,
    ) -> String {
        if self.is_composite_pk() {
            format!(
                "{} = {staging_alias}.{}",
                self.pk_text_expr(alias),
                quote_ident(staging_col)
            )
        } else {
            format!(
                "{}{} = {staging_alias}.{}::{}",
                Self::alias_prefix(alias),
                quote_ident(&self.pk_columns[0]),
                quote_ident(staging_col),
                self.pk_types[0],
            )
        }
    }
    pub fn pk_order_expr(&self, alias: &str) -> String {
        if self.is_composite_pk() {
            format!("{} COLLATE \"C\"", self.pk_text_expr(alias))
        } else {
            let prefix = if alias.is_empty() {
                String::new()
            } else {
                format!("{alias}.")
            };
            format!("{prefix}{}", quote_ident(&self.pk_columns[0]))
        }
    }
    pub fn pk_watermark_clause(&self, alias: &str, watermark: &str) -> String {
        if self.is_composite_pk() {
            format!(
                " AND {} > {} COLLATE \"C\"",
                self.pk_order_expr(alias),
                quote_literal(watermark)
            )
        } else {
            format!(
                " AND {} > {}::{}",
                self.pk_order_expr(alias),
                quote_literal(watermark),
                self.pk_types[0]
            )
        }
    }
    pub fn destination_comments(&self) -> Option<(String, String)> {
        let token = self.destination_token.as_deref()?;
        let table = format!(
            "postvec: managed chunk destination for {}.{}.{} (registry entry {}); ownership \
             token {token}. Automatic teardown requires this exact comment.",
            self.table_schema, self.table_name, self.source_column, self.id,
        );
        let view = format!(
            "postvec: managed chunk join view for {}.{}.{} (registry entry {}); ownership \
             token {token}. Automatic teardown requires this exact comment.",
            self.table_schema, self.table_name, self.source_column, self.id,
        );
        Some((table, view))
    }
    pub fn referenced_columns(&self) -> Result<Vec<String>, FormatError> {
        match self.format.as_deref() {
            None => Ok(vec![self.source_column.clone()]),
            Some(t) => {
                let refs = format_referenced_columns(&parse_format(t)?);
                if self.is_recursive() {
                    let mut out = vec![self.source_column.clone()];
                    for c in refs {
                        if c != "chunk" && !out.contains(&c) {
                            out.push(c);
                        }
                    }
                    Ok(out)
                } else {
                    Ok(refs)
                }
            }
        }
    }
}
/// Always-quote an identifier (equivalent to `quote_ident`, but unconditional):
/// wrap in double quotes and double any embedded quote. Safe to apply to
/// identifiers that would not otherwise need quoting.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// pgvector distance operator for a registry entry's metric.
/// Ascending order of any of these puts the nearest neighbour first.
pub fn distance_op(distance: &str) -> &'static str {
    match distance {
        "l2" => "<->",
        "ip" => "<#>",
        _ => "<=>", // cosine
    }
}

/// pgvector index opclass matching [`distance_op`].
pub fn distance_opclass(distance: &str) -> &'static str {
    match distance {
        "l2" => "vector_l2_ops",
        "ip" => "vector_ip_ops",
        _ => "vector_cosine_ops", // cosine
    }
}

/// The halfvec opclass for a metric — what the recommended expression index
/// uses above pgvector's 2000-dim HNSW limit.
pub fn halfvec_opclass(distance: &str) -> &'static str {
    match distance {
        "l2" => "halfvec_l2_ops",
        "ip" => "halfvec_ip_ops",
        _ => "halfvec_cosine_ops", // cosine
    }
}

/// The opclass an index must use to actually serve `search()` for an entry:
/// `search()` renders its semantic leg through halfvec above pgvector's
/// 2000-dim HNSW limit, plain vector otherwise, so a usable-for-this-entry
/// index is dimension- as well as distance-dependent.
pub fn expected_ann_opclass(distance: &str, dim: i32) -> &'static str {
    if dim > 2000 {
        halfvec_opclass(distance)
    } else {
        distance_opclass(distance)
    }
}

/// `EXISTS (...)` probe: does any **ANN search** index on `{rel}` depend on
/// column `{col}`? Covers direct column indexes (`hnsw (col vector_*_ops)`)
/// via `indkey`, and expression indexes such as the high-dimensional halfvec
/// recommendation (`hnsw ((col::halfvec(N)) halfvec_*_ops)`) via `pg_depend`
/// — Postgres records a per-column dependency for every column an index
/// expression references (the same edges `DROP COLUMN` cascades along), which
/// is exact: no string matching against the expression, so a sibling column
/// like `<col>_new` can never false-positive. The index's access method must
/// be `hnsw` or `ivfflat` (pgvector's ANN AMs) — a btree/gin diagnostic index
/// that merely references the column is not a vector-search index and must
/// not satisfy `status().has_vector_index` or migration finalization.
///
/// `rel` and `col` are SQL *expressions* interpolated verbatim (bind
/// parameters or catalog column references) — the single fragment shared by
/// [`vector_index_exists`] and `status()`'s set-based query.
pub fn vector_index_probe_sql(rel: &str, col: &str) -> String {
    format!(
        "EXISTS (
             SELECT 1
               FROM pg_index i
               JOIN pg_class ic ON ic.oid = i.indexrelid
               JOIN pg_am am ON am.oid = ic.relam
              WHERE i.indrelid = {rel}
                AND am.amname IN ('hnsw', 'ivfflat')
                -- a failed CREATE INDEX CONCURRENTLY leaves an invalid index
                -- PostgreSQL will not use; it must not satisfy status(),
                -- migration finalization, or the adopt-time advisory
                AND i.indisvalid AND i.indisready AND i.indislive
                AND (
                    EXISTS (
                        SELECT 1
                          FROM pg_attribute a
                         WHERE a.attrelid = i.indrelid
                           AND a.attnum = ANY(i.indkey)
                           AND a.attname = {col}
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM pg_depend d
                          JOIN pg_attribute a
                            ON a.attrelid = i.indrelid
                           AND a.attnum = d.refobjsubid
                         WHERE d.classid = 'pg_class'::regclass
                           AND d.objid = i.indexrelid
                           AND d.refclassid = 'pg_class'::regclass
                           AND d.refobjid = i.indrelid
                           AND a.attname = {col}
                    )
                )
        )"
    )
}

/// Quote a string as a SQL literal (`quote_literal`): wrap in single quotes and
/// double any embedded single quote.
///
/// NOT sufficient for text that later runs under an unknown
/// `standard_conforming_strings` setting — stored user input that will be
/// interpolated into worker-executed SQL must go through
/// [`quote_literal_estring`] instead.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote a string as a setting-independent `E'...'` literal, escaping both
/// backslashes and apostrophes. This is the only literal form that renders
/// identically under `standard_conforming_strings = on` and `off`, which is
/// mandatory for stored user input (template literals) that the superuser
/// worker later interpolates into dynamic SQL.
pub fn quote_literal_estring(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    out.push_str("E'");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

// ---- Formatting / context templates -----------------------------------

/// Template shape limits (query-shape bounds, not configuration).
pub const FORMAT_MAX_BYTES: usize = 16 * 1024;
pub const FORMAT_MAX_REFS: usize = 64;

/// One parsed template segment. Adjacent literals are coalesced by
/// [`parse_format`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatSeg {
    Literal(String),
    Column(String),
}

/// A template parse error, with the byte position it was detected at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    pub pos: usize,
    pub msg: String,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.msg, self.pos)
    }
}

/// Parse a format template. The grammar is deliberately small:
///
/// - `$name` for an ASCII simple identifier (`[A-Za-z_][A-Za-z0-9_]*`);
/// - `${exact column name}` for every other identifier (`}}` inside the
///   braces decodes to one literal `}` in the column name);
/// - `$$` for a literal dollar sign;
/// - every other byte is literal. There is **no** backslash-escape
///   processing: `\n` is two literal characters — newlines enter a template
///   through SQL string syntax (`E'\n'` or a literal newline).
pub fn parse_format(template: &str) -> Result<Vec<FormatSeg>, FormatError> {
    if template.is_empty() {
        return Err(FormatError {
            pos: 0,
            msg: "the template is empty (use NULL to clear a template)".into(),
        });
    }
    if template.len() > FORMAT_MAX_BYTES {
        return Err(FormatError {
            pos: FORMAT_MAX_BYTES,
            msg: format!(
                "the template is {} bytes (max {FORMAT_MAX_BYTES})",
                template.len()
            ),
        });
    }
    let mut segs: Vec<FormatSeg> = Vec::new();
    let mut lit = String::new();
    let mut chars = template.char_indices().peekable();
    let push_column = |segs: &mut Vec<FormatSeg>, lit: &mut String, name: String| {
        if !lit.is_empty() {
            segs.push(FormatSeg::Literal(std::mem::take(lit)));
        }
        segs.push(FormatSeg::Column(name));
    };
    while let Some((pos, ch)) = chars.next() {
        if ch != '$' {
            lit.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some((_, '$')) => {
                chars.next();
                lit.push('$');
            }
            Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some((_, c)) = chars.peek().copied() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                push_column(&mut segs, &mut lit, name);
            }
            Some((brace_pos, '{')) => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                while let Some((_, c)) = chars.next() {
                    if c == '}' {
                        // `}}` decodes to one literal `}` in the name;
                        // a single `}` closes the reference.
                        if matches!(chars.peek(), Some((_, '}'))) {
                            chars.next();
                            name.push('}');
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        name.push(c);
                    }
                }
                if !closed {
                    return Err(FormatError {
                        pos: brace_pos,
                        msg: "unclosed ${...} column reference".into(),
                    });
                }
                if name.is_empty() {
                    return Err(FormatError {
                        pos: brace_pos,
                        msg: "empty ${} column reference".into(),
                    });
                }
                push_column(&mut segs, &mut lit, name);
            }
            _ => {
                return Err(FormatError {
                    pos,
                    msg: "dangling '$' (use '$$' for a literal dollar sign, $name or \
                          ${name} for a column reference)"
                        .into(),
                });
            }
        }
    }
    if !lit.is_empty() {
        segs.push(FormatSeg::Literal(lit));
    }
    let distinct = format_referenced_columns(&segs);
    if distinct.len() > FORMAT_MAX_REFS {
        return Err(FormatError {
            pos: 0,
            msg: format!(
                "{} distinct columns referenced (max {FORMAT_MAX_REFS})",
                distinct.len()
            ),
        });
    }
    Ok(segs)
}

/// Distinct referenced columns, deduplicated in first-occurrence order.
pub fn format_referenced_columns(segs: &[FormatSeg]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in segs {
        if let FormatSeg::Column(c) = seg {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
    }
    out
}

/// The one render expression for all document-embedding reads (queue source
/// reads and the reembed migration leg). With no template it returns exactly
/// today's raw `source::text` expression; with a template it renders
///
/// ```sql
/// CASE WHEN alias.source IS NOT NULL THEN
///     E'literal' || COALESCE(alias.col::text, '') || ...
/// END
/// ```
///
/// The CASE is the lifecycle anchor: context columns cannot keep a vector
/// alive after the source becomes NULL (the expression goes NULL, the job
/// takes the no-inference NULL path, the vector converges to NULL). Literal
/// segments always render through [`quote_literal_estring`] — they are stored
/// user input that the superuser worker interpolates into dynamic SQL, so the
/// apostrophe-only [`quote_literal`] would be an insufficient boundary under
/// `standard_conforming_strings = off`.
pub fn format_expr(entry: &RegistryEntry, alias: &str) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let Some(template) = entry.format.as_deref() else {
        return format!("{src}::text");
    };
    let segs = parse_format(template).unwrap_or_else(|e| {
        // Stored templates are validated before storage; an invalid one means
        // the registry row was edited by hand. Fail loudly rather than embed
        // the wrong text (a plain panic, which pgrx reports as an ERROR in a
        // backend, keeps this function linkable from non-postgres unit tests).
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let parts: Vec<String> = segs
        .iter()
        .map(|seg| match seg {
            FormatSeg::Literal(s) => quote_literal_estring(s),
            FormatSeg::Column(c) => {
                format!("COALESCE({prefix}{}::text, '')", quote_ident(c))
            }
        })
        .collect();
    format!(
        "CASE WHEN {src} IS NOT NULL THEN {} END",
        parts.join(" || ")
    )
}

/// Chunk-mode document-embedding render expression: the recursive
/// counterpart of [`format_expr`], with a source alias for context columns
/// and a chunk alias for the reserved `$chunk` pseudo-column.
///
/// With no template it renders exactly `chunk_alias.chunk_text` (the chunk is
/// embedded verbatim). With a template it renders the same CASE-anchored
/// concatenation as column mode, except that `Column("chunk")` resolves to
/// the destination's `chunk_text` (NOT NULL, so no COALESCE) and every other
/// reference resolves against the source alias. The CASE anchor keeps the
/// lifecycle rule: a NULL source renders SQL NULL whatever the context holds
/// — the claim path treats that as an obsolete chunk, never as embeddable
/// text.
pub fn chunk_format_expr(entry: &RegistryEntry, source_alias: &str, chunk_alias: &str) -> String {
    let chunk = format!("{chunk_alias}.chunk_text");
    let Some(template) = entry.format.as_deref() else {
        return chunk;
    };
    let prefix = if source_alias.is_empty() {
        String::new()
    } else {
        format!("{source_alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let parts: Vec<String> = segs
        .iter()
        .map(|seg| match seg {
            FormatSeg::Literal(s) => quote_literal_estring(s),
            FormatSeg::Column(c) if c == "chunk" => chunk.clone(),
            FormatSeg::Column(c) => {
                format!("COALESCE({prefix}{}::text, '')", quote_ident(c))
            }
        })
        .collect();
    format!(
        "CASE WHEN {src} IS NOT NULL THEN {} END",
        parts.join(" || ")
    )
}

/// The EXACT byte length [`format_expr`] would render, computed **without
/// building the concatenation**: literal byte lengths are summed as a
/// constant and each column reference contributes
/// `octet_length(COALESCE(col::text, ''))`. Concatenation length equals the
/// sum of its parts, so the value matches `octet_length(format_expr(...))`
/// byte for byte — while `octet_length` on a toasted text column reads the
/// stored size without detoasting. This is what lets the claim/migration
/// reads measure every candidate row cheaply and render ONLY the rows that
/// pass the byte ceilings (the render itself stays in a lazily-evaluated
/// projection over the admitted set).
pub fn format_len_expr(entry: &RegistryEntry, alias: &str) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let Some(template) = entry.format.as_deref() else {
        return format!("octet_length({src}::text)::bigint");
    };
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let mut literal_bytes: i64 = 0;
    let mut terms: Vec<String> = Vec::new();
    for seg in &segs {
        match seg {
            FormatSeg::Literal(s) => literal_bytes += s.len() as i64,
            FormatSeg::Column(c) => terms.push(format!(
                "COALESCE(octet_length({prefix}{}::text)::bigint, 0)",
                quote_ident(c)
            )),
        }
    }
    let mut sum = format!("{literal_bytes}::bigint");
    for t in terms {
        sum.push_str(" + ");
        sum.push_str(&t);
    }
    format!("CASE WHEN {src} IS NOT NULL THEN {sum} END")
}

/// The chunk-mode twin of [`format_len_expr`], mirroring
/// [`chunk_format_expr`]: `$chunk` contributes `octet_length(chunk_text)`
/// (NOT NULL by schema), everything else resolves against the source alias.
pub fn chunk_format_len_expr(
    entry: &RegistryEntry,
    source_alias: &str,
    chunk_alias: &str,
) -> String {
    let chunk_len = format!("octet_length({chunk_alias}.chunk_text)::bigint");
    let Some(template) = entry.format.as_deref() else {
        return chunk_len;
    };
    let prefix = if source_alias.is_empty() {
        String::new()
    } else {
        format!("{source_alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let mut literal_bytes: i64 = 0;
    let mut terms: Vec<String> = Vec::new();
    for seg in &segs {
        match seg {
            FormatSeg::Literal(s) => literal_bytes += s.len() as i64,
            FormatSeg::Column(c) if c == "chunk" => terms.push(chunk_len.clone()),
            FormatSeg::Column(c) => terms.push(format!(
                "COALESCE(octet_length({prefix}{}::text)::bigint, 0)",
                quote_ident(c)
            )),
        }
    }
    let mut sum = format!("{literal_bytes}::bigint");
    for t in terms {
        sum.push_str(" + ");
        sum.push_str(&t);
    }
    format!("CASE WHEN {src} IS NOT NULL THEN {sum} END")
}

/// Serialize an embedding to pgvector's text input format: `[f1,f2,...]`.
/// Cast the result server-side with `$n::vector`.
pub fn serialize_vector(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 8 + 2);
    out.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // Rust's shortest round-tripping float formatting; pgvector parses it.
        out.push_str(&f.to_string());
    }
    out.push(']');
    out
}

/// Parse pgvector's text output format (`[f1,f2,...]`, as produced by
/// `vec::text`) back into floats — the migration driver reads stored vectors
/// this way before sending them to `ConvertEmbeddings`.
pub fn parse_vector(text: &str) -> Result<Vec<f32>, String> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("not a pgvector text literal: {text:?}"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|f| {
            f.trim()
                .parse::<f32>()
                .map_err(|e| format!("bad float {f:?} in vector: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pk_columns: &[&str], pk_types: &[&str]) -> RegistryEntry {
        RegistryEntry {
            id: 1,
            table_schema: "public".into(),
            table_name: "docs".into(),
            source_column: "body".into(),
            vector_column: "body_semantic".into(),
            pk_columns: pk_columns.iter().map(|s| s.to_string()).collect(),
            pk_types: pk_types.iter().map(|s| s.to_string()).collect(),
            model: "m".into(),
            dim: 3,
            fts_config: "pg_catalog.english".into(),
            distance: "cosine".into(),
            backfill_mode: "none".into(),
            backfill_watermark: None,
            state: "active".into(),
            trigger_mode: "statement".into(),
            owns_vector_column: true,
            format: None,
            index_mode: "manual".into(),
            index_error: None,
            chunking: "none".into(),
            chunk_size: None,
            chunk_overlap: None,
            destination_schema: None,
            destination_table: None,
            destination_view: None,
            destination_token: None,
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("body"), "\"body\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn vector_serialization() {
        assert_eq!(serialize_vector(&[]), "[]");
        assert_eq!(serialize_vector(&[0.5, -1.0, 2.0]), "[0.5,-1,2]");
    }

    #[test]
    fn vector_parse_roundtrip() {
        assert_eq!(parse_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(parse_vector("[0.5,-1,2]").unwrap(), vec![0.5f32, -1.0, 2.0]);
        assert_eq!(
            parse_vector(&serialize_vector(&[1.25, -0.001])).unwrap(),
            vec![1.25f32, -0.001]
        );
        assert!(parse_vector("0.5,1").is_err());
        assert!(parse_vector("[a,b]").is_err());
    }

    #[test]
    fn single_pk_expressions() {
        let e = entry(&["id"], &["bigint"]);
        assert!(!e.is_composite_pk());
        assert_eq!(e.pk_text_expr(""), "\"id\"::text");
        assert_eq!(e.pk_text_expr("t"), "t.\"id\"::text");
        assert_eq!(e.pk_any_clause("t", "$1"), "t.\"id\" = ANY($1::bigint[])");
        assert_eq!(
            e.pk_staging_join_clause("t", "d", "pk"),
            "t.\"id\" = d.\"pk\"::bigint"
        );
        assert_eq!(e.pk_order_expr("t"), "t.\"id\"");
        assert_eq!(
            e.pk_watermark_clause("t", "42"),
            " AND t.\"id\" > '42'::bigint"
        );
    }

    // ---- Template parser and render expression ----

    fn lit(s: &str) -> FormatSeg {
        FormatSeg::Literal(s.into())
    }
    fn col(s: &str) -> FormatSeg {
        FormatSeg::Column(s.into())
    }

    #[test]
    fn format_parser_matrix() {
        assert_eq!(parse_format("$body").unwrap(), vec![col("body")]);
        assert_eq!(
            parse_format("a $body b").unwrap(),
            vec![lit("a "), col("body"), lit(" b")]
        );
        assert_eq!(
            parse_format("$title — $author\n\n$body").unwrap(),
            vec![
                col("title"),
                lit(" — "),
                col("author"),
                lit("\n\n"),
                col("body")
            ]
        );
        // Braced names take arbitrary characters; `}}` decodes to one `}`.
        assert_eq!(parse_format("${we ird}").unwrap(), vec![col("we ird")]);
        assert_eq!(parse_format("${a}}b}").unwrap(), vec![col("a}b")]);
        // `$$` is a literal dollar; adjacent literals coalesce.
        assert_eq!(parse_format("$$5 a$$b").unwrap(), vec![lit("$5 a$b")]);
        assert_eq!(parse_format("$a$b").unwrap(), vec![col("a"), col("b")]);
        // Underscore/digit name rules.
        assert_eq!(parse_format("$_x9 ").unwrap(), vec![col("_x9"), lit(" ")]);
        // No backslash processing: \n is two literal characters.
        assert_eq!(parse_format(r"a\nb").unwrap(), vec![lit(r"a\nb")]);
    }

    #[test]
    fn format_parser_errors_carry_positions() {
        assert_eq!(parse_format("").unwrap_err().pos, 0);
        assert_eq!(parse_format("$").unwrap_err().pos, 0);
        assert_eq!(parse_format("abc$").unwrap_err().pos, 3);
        assert_eq!(
            parse_format("$1").unwrap_err().pos,
            0,
            "digit cannot start a name"
        );
        assert_eq!(parse_format("ab${}").unwrap_err().pos, 3);
        assert_eq!(parse_format("ab${xy").unwrap_err().pos, 3);
        assert_eq!(
            parse_format("${a}}b").unwrap_err().pos,
            1,
            "}} consumed as a literal brace leaves the reference unclosed"
        );

        let oversized = "x".repeat(FORMAT_MAX_BYTES + 1);
        assert!(parse_format(&oversized).unwrap_err().msg.contains("bytes"));
        let many: String = (0..65).map(|i| format!("$c{i} ")).collect();
        assert!(parse_format(&many).unwrap_err().msg.contains("distinct"));
    }

    #[test]
    fn format_referenced_columns_dedup_in_first_occurrence_order() {
        let segs = parse_format("$b $a $b $c $a").unwrap();
        assert_eq!(format_referenced_columns(&segs), vec!["b", "a", "c"]);
    }

    #[test]
    fn estring_literal_doubles_backslashes_and_quotes() {
        assert_eq!(quote_literal_estring("plain"), "E'plain'");
        assert_eq!(quote_literal_estring("it's"), r"E'it\'s'");
        assert_eq!(quote_literal_estring(r"a\b"), r"E'a\\b'");
        assert_eq!(quote_literal_estring(r"\'"), r"E'\\\''");
    }

    #[test]
    fn format_expr_golden_sql() {
        let mut e = entry(&["id"], &["bigint"]);
        // No template: exactly today's raw source expression.
        assert_eq!(format_expr(&e, "t"), "t.\"body\"::text");
        assert_eq!(format_expr(&e, ""), "\"body\"::text");

        e.format = Some("$title — $body".into());
        assert_eq!(
            format_expr(&e, "t"),
            "CASE WHEN t.\"body\" IS NOT NULL THEN \
             COALESCE(t.\"title\"::text, '') || E' — ' || COALESCE(t.\"body\"::text, '') END"
        );

        // Hostile literal content renders through the setting-independent
        // E-string helper, never the apostrophe-only quote_literal().
        e.format = Some(r"it's \ ok $body".into());
        assert_eq!(
            format_expr(&e, ""),
            r#"CASE WHEN "body" IS NOT NULL THEN E'it\'s \\ ok ' || COALESCE("body"::text, '') END"#
        );
    }

    #[test]
    fn composite_pk_expressions() {
        let e = entry(&["a", "b"], &["integer", "text"]);
        assert!(e.is_composite_pk());
        assert_eq!(e.pk_text_expr("n"), "ROW(n.\"a\", n.\"b\")::text");
        assert_eq!(
            e.pk_any_clause("t", "$1"),
            "ROW(t.\"a\", t.\"b\")::text = ANY($1)"
        );
        assert_eq!(
            e.pk_staging_join_clause("t", "d", "pk"),
            "ROW(t.\"a\", t.\"b\")::text = d.\"pk\""
        );
        assert_eq!(e.pk_order_expr(""), "ROW(\"a\", \"b\")::text COLLATE \"C\"");
        assert_eq!(
            e.pk_watermark_clause("", "(1,x)"),
            " AND ROW(\"a\", \"b\")::text COLLATE \"C\" > '(1,x)' COLLATE \"C\""
        );
    }
}
