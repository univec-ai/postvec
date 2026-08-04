// SPDX-License-Identifier: PostgreSQL
//! Deterministic recursive character splitter. No PostgreSQL, tokio, engine,
//! tokenizer or provider dependency: the contract is a function of its
//! inputs.
//!
//! Semantics are pinned by the tests below. Changing any of them changes
//! stored chunk identities, so treat this file as a wire format.
//!
//! - Sizes count Unicode scalar values (`str::chars()`), never UTF-8 bytes.
//!   Offsets are zero-based, end-exclusive character offsets.
//! - The source substring is preserved exactly. No normalization.
//! - Boundary preference is the fixed hierarchy in [`SEPARATORS`]. An
//!   oversized span that a level cannot split moves to the next level,
//!   ending at an arbitrary character boundary. Separators stay attached
//!   to the preceding piece, which keeps offsets unambiguous and avoids
//!   punctuation-only prefixes.
//! - Adjacent pieces merge greedily up to `chunk_size`. When a chunk is
//!   emitted, at most `chunk_overlap` of its trailing characters are
//!   carried into the next chunk (fewer when the next piece would not
//!   fit). Every emitted chunk holds at most `chunk_size` characters.
//! - Every chunk starts strictly after the previous chunk's start, even
//!   with `chunk_overlap == chunk_size - 1` and pathological separators.
//! - Chunks whose text is empty or entirely Unicode whitespace are
//!   dropped; survivors are numbered contiguously from 0. An empty or
//!   whitespace-only document yields zero chunks, successfully.
//! - At most [`MAX_CHUNKS_PER_DOCUMENT`] non-blank chunks. Exceeding the
//!   cap is an error and no partial result is returned.

/// Default chunk size in Unicode scalar values.
pub const DEFAULT_CHUNK_SIZE: i32 = 2000;
/// Default overlap in Unicode scalar values.
pub const DEFAULT_CHUNK_OVERLAP: i32 = 200;
/// Smallest `chunk_size` `enable()` accepts.
pub const MIN_CHUNK_SIZE: i32 = 64;
/// Largest `chunk_size` `enable()` accepts.
pub const MAX_CHUNK_SIZE: i32 = 100_000;
/// One document may produce at most this many non-blank chunks. Exceeding
/// it is a permanent refresh failure, not a partial write.
pub const MAX_CHUNKS_PER_DOCUMENT: usize = 10_000;

/// Input bound in UTF-8 bytes, checked before any per-character state is
/// allocated. The chunk cap alone does not bound peak memory: the splitter
/// materializes the document as `Vec<char>` (~4 bytes per ASCII character),
/// so a very large text value would amplify several-fold before the chunk
/// count could be observed. 32 MiB keeps the worst-case working set around
/// ~160 MiB and still exceeds any realistic single text document.
pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;

/// Output-amplification bound: attempted chunk spans (emitted or dropped as
/// blank) may total at most this many times the document's character count,
/// with a small floor so short documents never trip it. Overlap rematerializes
/// shared characters into every chunk that carries them, so neither the
/// input bound nor the chunk cap alone bounds the output:
/// `chunk_overlap = chunk_size - 1` over a separator-free ~440 KiB document
/// would otherwise materialize ~10,000 nearly-identical 100,000-character
/// strings inside the worker. 4x admits any overlap up to 75% of
/// `chunk_size` (the defaults use 10%) while capping the pathological
/// configurations. Charged per attempted span so it also bounds blank-scan
/// CPU, and enforced incrementally before any chunk string is allocated.
pub const MAX_OUTPUT_AMPLIFICATION: usize = 4;

/// Boundary hierarchy, strongest first. A span is split with the first level;
/// any piece still over `chunk_size` is split again with the *next* level,
/// down to the arbitrary character boundary fallback.
const SEPARATORS: [&[char]; 10] = [
    &['\r', '\n', '\r', '\n'],
    &['\n', '\n'],
    &['\r', '\n'],
    &['\n'],
    &['.', ' '],
    &['?', ' '],
    &['!', ' '],
    &[';', ' '],
    &[',', ' '],
    &[' '],
];

/// One emitted chunk. `char_start`/`char_end` are zero-based, end-exclusive
/// character offsets into the source text; `text` is exactly the source
/// substring those offsets denote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub seq: i32,
    pub char_start: i64,
    pub char_end: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitError {
    /// `chunk_size < 1`, or `chunk_overlap` outside `0..chunk_size`. The SQL
    /// surface validates the product bounds (64..=100_000) before anything is
    /// stored; this is the pure function's own defensive contract.
    Geometry(String),
    /// The document would produce more than [`MAX_CHUNKS_PER_DOCUMENT`]
    /// non-blank chunks. Atomic: no partial chunk list is returned.
    TooManyChunks { limit: usize },
    /// The document exceeds [`MAX_DOCUMENT_BYTES`]; refused before any
    /// splitting state is allocated.
    DocumentTooLarge { bytes: usize, limit: usize },
    /// The attempted chunk spans would total more than
    /// [`MAX_OUTPUT_AMPLIFICATION`] × the document's characters — an
    /// overlap-driven output (or blank-scan work) explosion. Refused before
    /// any chunk string is allocated; no partial result is returned.
    OutputBudgetExceeded { chars: usize, budget: usize },
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitError::Geometry(msg) => write!(f, "invalid chunking geometry: {msg}"),
            SplitError::TooManyChunks { limit } => write!(
                f,
                "the document splits into more than {limit} non-blank chunks (the per-document \
                 limit); re-create the entry with a larger chunk_size or shrink the document"
            ),
            SplitError::DocumentTooLarge { bytes, limit } => write!(
                f,
                "the document is {bytes} bytes, over the {limit}-byte per-document splitter \
                 limit; store oversized payloads outside the semantic column or split them \
                 upstream"
            ),
            SplitError::OutputBudgetExceeded { chars, budget } => write!(
                f,
                "the chunk set would scan and materialize {chars} characters, over the \
                 per-document output budget of {budget} ({MAX_OUTPUT_AMPLIFICATION}x the \
                 document); lower chunk_overlap, raise chunk_size, or split the document \
                 upstream"
            ),
        }
    }
}

/// Split `text` into recursive character chunks. Deterministic: the same
/// text and geometry produce byte-identical chunks and offsets everywhere.
pub fn split_recursive(
    text: &str,
    chunk_size: i32,
    chunk_overlap: i32,
) -> Result<Vec<Chunk>, SplitError> {
    if chunk_size < 1 {
        return Err(SplitError::Geometry(format!(
            "chunk_size must be at least 1 (got {chunk_size})"
        )));
    }
    if chunk_overlap < 0 || chunk_overlap >= chunk_size {
        return Err(SplitError::Geometry(format!(
            "chunk_overlap must be between 0 and chunk_size - 1 (got {chunk_overlap} for \
             chunk_size {chunk_size})"
        )));
    }
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(SplitError::DocumentTooLarge {
            bytes: text.len(),
            limit: MAX_DOCUMENT_BYTES,
        });
    }
    let size = chunk_size as usize;
    let overlap = chunk_overlap as usize;
    // Fast path: a document with no non-whitespace character yields zero
    // chunks by definition (blank chunks are dropped). Deciding that on the
    // `&str` skips the `Vec<char>` materialization and — under extreme
    // overlaps — the millions of overlapping spans the merge would generate
    // and drop one by one.
    if !text.chars().any(|c| !c.is_whitespace()) {
        return Ok(Vec::new());
    }
    let chars: Vec<char> = text.chars().collect();
    let mut merger = Merger {
        size,
        overlap,
        // The floor lets a document shorter than one chunk always emit its
        // single chunk; beyond that, output is a bounded multiple of input.
        output_budget: (chars.len() * MAX_OUTPUT_AMPLIFICATION).max(size),
        charged_chars: 0,
        cur: None,
        out: Vec::new(),
    };
    walk(&chars, 0, chars.len(), 0, &mut merger)?;
    merger.finish(&chars)
}

/// Greedy merge state over the contiguous piece stream. Pieces arrive in
/// order and cover the text exactly; chunks are contiguous spans whose starts
/// strictly increase. Blank chunks are dropped as they are emitted, so a
/// whitespace-heavy document neither trips the cap nor accumulates state —
/// `out` never holds more than [`MAX_CHUNKS_PER_DOCUMENT`] spans.
struct Merger {
    size: usize,
    overlap: usize,
    /// Cumulative character budget for **attempted** spans — the
    /// output-amplification bound doubling as the scan-work bound, enforced
    /// while chunks are still 16-byte spans, never after strings exist.
    output_budget: usize,
    charged_chars: usize,
    /// The chunk being assembled, as a char span.
    cur: Option<(usize, usize)>,
    /// Emitted non-blank chunk spans.
    out: Vec<(usize, usize)>,
}

impl Merger {
    /// Add one piece (`start..end`, at most `size` chars, contiguous with the
    /// previous piece).
    fn push(&mut self, chars: &[char], start: usize, end: usize) -> Result<(), SplitError> {
        debug_assert!(end > start, "pieces are non-empty");
        debug_assert!(end - start <= self.size, "pieces never exceed chunk_size");
        match self.cur {
            None => self.cur = Some((start, end)),
            Some((cs, ce)) => {
                debug_assert_eq!(ce, start, "pieces are contiguous");
                if (ce - cs) + (end - start) <= self.size {
                    self.cur = Some((cs, end));
                } else {
                    self.emit(chars, cs, ce)?;
                    // The next chunk starts with up to `overlap` trailing
                    // characters of the emitted one — fewer when the incoming
                    // piece would push the chunk past `size`, and always at
                    // least one character past the previous start (progress).
                    let mut ns = ce.saturating_sub(self.overlap).max(cs + 1);
                    ns = ns.max((ce + (end - start)).saturating_sub(self.size));
                    self.cur = Some((ns, end));
                }
            }
        }
        Ok(())
    }

    /// Record one chunk span, dropping blank ones and enforcing both caps —
    /// the count of **non-blank** chunks and the cumulative span budget —
    /// on spans only, before any string is materialized.
    fn emit(&mut self, chars: &[char], start: usize, end: usize) -> Result<(), SplitError> {
        // The budget is charged for every ATTEMPTED span, before the blank
        // scan below — it bounds scan work, not just materialized output.
        // Otherwise a whitespace-heavy document under an extreme overlap
        // could generate millions of spans that each scan up to chunk_size
        // characters and then drop as blank, consuming neither cap.
        self.charged_chars += end - start;
        if self.charged_chars > self.output_budget {
            return Err(SplitError::OutputBudgetExceeded {
                chars: self.charged_chars,
                budget: self.output_budget,
            });
        }
        if chars[start..end].iter().all(|c| c.is_whitespace()) {
            return Ok(());
        }
        if self.out.len() >= MAX_CHUNKS_PER_DOCUMENT {
            return Err(SplitError::TooManyChunks {
                limit: MAX_CHUNKS_PER_DOCUMENT,
            });
        }
        self.out.push((start, end));
        Ok(())
    }

    /// Close the final chunk and materialize the result.
    fn finish(mut self, chars: &[char]) -> Result<Vec<Chunk>, SplitError> {
        if let Some((cs, ce)) = self.cur.take() {
            self.emit(chars, cs, ce)?;
        }
        Ok(self
            .out
            .iter()
            .enumerate()
            .map(|(i, &(start, end))| Chunk {
                seq: i as i32,
                char_start: start as i64,
                char_end: end as i64,
                text: chars[start..end].iter().collect(),
            })
            .collect())
    }
}

/// Recursively feed the span `start..end` to the merger as pieces of at most
/// `size` characters, using separator level `level` and below.
fn walk(
    chars: &[char],
    start: usize,
    end: usize,
    level: usize,
    merger: &mut Merger,
) -> Result<(), SplitError> {
    if end - start <= merger.size {
        if end > start {
            merger.push(chars, start, end)?;
        }
        return Ok(());
    }
    if level >= SEPARATORS.len() {
        // Arbitrary character boundary fallback. Piece length `size -
        // overlap` makes the merge's carried overlap exact for separator-free
        // spans (the merge tops each chunk back up to `size`); `max(1)`
        // guarantees progress at `overlap == size - 1`.
        let step = (merger.size - merger.overlap).max(1);
        let mut p = start;
        while p < end {
            let q = (p + step).min(end);
            merger.push(chars, p, q)?;
            p = q;
        }
        return Ok(());
    }
    let sep = SEPARATORS[level];
    let mut found = false;
    let mut seg_start = start;
    let mut i = start;
    while i + sep.len() <= end {
        if &chars[i..i + sep.len()] == sep {
            found = true;
            let seg_end = i + sep.len(); // separator stays with the piece
            segment(chars, seg_start, seg_end, level, merger)?;
            seg_start = seg_end;
            i = seg_end;
        } else {
            i += 1;
        }
    }
    if !found {
        return walk(chars, start, end, level + 1, merger);
    }
    if seg_start < end {
        segment(chars, seg_start, end, level, merger)?;
    }
    Ok(())
}

/// One segment produced by a separator split: a piece when it fits, another
/// recursion level when it does not.
fn segment(
    chars: &[char],
    start: usize,
    end: usize,
    level: usize,
    merger: &mut Merger,
) -> Result<(), SplitError> {
    if end - start <= merger.size {
        merger.push(chars, start, end)
    } else {
        walk(chars, start, end, level + 1, merger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str, size: i32, overlap: i32) -> Vec<Chunk> {
        split_recursive(text, size, overlap).expect("split succeeds")
    }

    /// Every structural invariant, assertable on any successful result.
    fn assert_invariants(text: &str, size: i32, chunks: &[Chunk]) {
        let chars: Vec<char> = text.chars().collect();
        let mut prev_start: Option<i64> = None;
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.seq, i as i32, "seq is contiguous from 0");
            assert!(c.char_end > c.char_start, "chunks are non-empty");
            assert!(
                (c.char_end - c.char_start) as i32 <= size,
                "chunk {i} exceeds chunk_size: {} chars",
                c.char_end - c.char_start
            );
            // Offsets slice back to exactly the emitted text.
            let sliced: String = chars[c.char_start as usize..c.char_end as usize]
                .iter()
                .collect();
            assert_eq!(sliced, c.text, "offsets denote exactly the chunk text");
            assert!(
                !c.text.chars().all(|ch| ch.is_whitespace()),
                "blank chunks are dropped"
            );
            if let Some(p) = prev_start {
                assert!(
                    c.char_start > p,
                    "every chunk advances past the prior start"
                );
            }
            prev_start = Some(c.char_start);
        }
    }

    #[test]
    fn short_text_is_one_chunk() {
        let chunks = split("hello world", 64, 8);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            Chunk {
                seq: 0,
                char_start: 0,
                char_end: 11,
                text: "hello world".into()
            }
        );
    }

    #[test]
    fn exactly_size_is_one_chunk() {
        let text = "a".repeat(64);
        let chunks = split(&text, 64, 8);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].char_end, 64);
    }

    #[test]
    fn empty_and_whitespace_documents_yield_zero_chunks() {
        assert!(split("", 64, 0).is_empty());
        assert!(split("   \n\n\t  \r\n ", 64, 8).is_empty());
        // Unicode whitespace too (NBSP, ideographic space).
        assert!(split("\u{00a0}\u{3000}\u{2003}", 64, 8).is_empty());
    }

    // ---- golden cases: exact chunk output is a wire format ----

    #[test]
    fn golden_word_merge_no_overlap() {
        let chunks = split("aaa bbb ccc ddd", 10, 0);
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    seq: 0,
                    char_start: 0,
                    char_end: 8,
                    text: "aaa bbb ".into()
                },
                Chunk {
                    seq: 1,
                    char_start: 8,
                    char_end: 15,
                    text: "ccc ddd".into()
                },
            ]
        );
    }

    #[test]
    fn golden_word_merge_with_overlap() {
        let chunks = split("aaa bbb ccc ddd", 10, 3);
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    seq: 0,
                    char_start: 0,
                    char_end: 8,
                    text: "aaa bbb ".into()
                },
                Chunk {
                    seq: 1,
                    char_start: 5,
                    char_end: 15,
                    text: "bb ccc ddd".into()
                },
            ]
        );
    }

    #[test]
    fn golden_sentence_precedence_beats_space() {
        // ". " splits first; the two sentence pieces each fit, so the space
        // level is never consulted.
        let chunks = split("Hi. There? Ok", 12, 0);
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    seq: 0,
                    char_start: 0,
                    char_end: 4,
                    text: "Hi. ".into()
                },
                Chunk {
                    seq: 1,
                    char_start: 4,
                    char_end: 13,
                    text: "There? Ok".into()
                },
            ]
        );
    }

    #[test]
    fn golden_crlf_hierarchy() {
        // "A\r\n\r\nB" with size 4: the CRLF-paragraph piece is oversized, no
        // "\n\n" occurs inside "\r\n\r\n", so the "\r\n" level splits it.
        let chunks = split("A\r\n\r\nB", 4, 0);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "A\r\n");
        assert_eq!((chunks[0].char_start, chunks[0].char_end), (0, 3));
        // "\r\n" alone is blank and dropped; "\r\nB" survives.
        assert_eq!(chunks[1].text, "\r\nB");
        assert_eq!((chunks[1].char_start, chunks[1].char_end), (3, 6));
    }

    #[test]
    fn paragraphs_split_before_newlines() {
        let text = "first paragraph\n\nsecond paragraph\nsame paragraph";
        let chunks = split(text, 20, 0);
        assert_invariants(text, 20, &chunks);
        // The paragraph boundary was preferred: chunk 0 ends with "\n\n".
        assert_eq!(chunks[0].text, "first paragraph\n\n");
        // The remaining paragraph is over 20 chars and falls to the "\n" level.
        assert_eq!(chunks[1].text, "second paragraph\n");
    }

    #[test]
    fn question_exclamation_semicolon_comma_levels() {
        for (text, boundary) in [
            ("aaaa? bbbb cccc", "aaaa? "),
            ("aaaa! bbbb cccc", "aaaa! "),
            ("aaaa; bbbb cccc", "aaaa; "),
            ("aaaa, bbbb cccc", "aaaa, "),
        ] {
            let chunks = split(text, 10, 0);
            assert_eq!(chunks[0].text, boundary, "boundary for {text:?}");
            assert_invariants(text, 10, &chunks);
        }
    }

    #[test]
    fn separator_stays_with_preceding_piece() {
        let chunks = split("one. two. three. four.", 10, 0);
        assert_invariants("one. two. three. four.", 10, &chunks);
        for c in &chunks[..chunks.len() - 1] {
            assert!(
                c.text.ends_with(". ") || c.text.ends_with('.'),
                "no chunk starts with a dangling separator: {:?}",
                c.text
            );
        }
        assert!(chunks.iter().all(|c| !c.text.starts_with(' ')));
    }

    // ---- Unicode ----

    #[test]
    fn sizes_count_scalar_values_not_bytes() {
        // 10 crab emoji = 10 chars (40 UTF-8 bytes). size 4 → chunks of 4.
        let text = "🦀".repeat(10);
        let chunks = split(&text, 4, 0);
        assert_invariants(&text, 4, &chunks);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "🦀🦀🦀🦀");
        assert_eq!((chunks[1].char_start, chunks[1].char_end), (4, 8));
        assert_eq!(chunks[2].text, "🦀🦀");
    }

    #[test]
    fn combining_marks_and_accents_are_preserved_exactly() {
        // "e" + COMBINING ACUTE is two scalar values; a split between them is
        // legal at the fallback level and must round-trip exactly.
        let text = "e\u{301}".repeat(8); // 16 scalar values
        let chunks = split(&text, 5, 0);
        assert_invariants(&text, 5, &chunks);
        let reassembled: String = {
            // Chunks cover the text contiguously at overlap 0.
            chunks.iter().map(|c| c.text.as_str()).collect()
        };
        assert_eq!(reassembled, text, "no normalization, no lost characters");
    }

    #[test]
    fn cjk_and_rtl_round_trip() {
        let cjk = "漢字のテキストです。空白なし".repeat(3);
        let chunks = split(&cjk, 10, 2);
        assert_invariants(&cjk, 10, &chunks);

        let rtl = "שלום עולם טקסט בעברית ".repeat(4);
        let chunks = split(&rtl, 12, 3);
        assert_invariants(&rtl, 12, &chunks);
    }

    #[test]
    fn crlf_never_splits_between_cr_and_lf_at_separator_levels() {
        let text = "line one\r\nline two\r\nline three\r\n";
        let chunks = split(text, 12, 0);
        assert_invariants(text, 12, &chunks);
        for c in &chunks {
            assert!(
                !c.text.starts_with('\n'),
                "CRLF was split between CR and LF: {:?}",
                c.text
            );
        }
    }

    // ---- overlap and fallback ----

    #[test]
    fn zero_overlap_chunks_are_disjoint_and_cover() {
        let text = "x".repeat(100);
        let chunks = split(&text, 30, 0);
        assert_invariants(&text, 30, &chunks);
        for w in chunks.windows(2) {
            assert_eq!(w[0].char_end, w[1].char_start, "disjoint and contiguous");
        }
        assert_eq!(chunks.last().unwrap().char_end, 100);
    }

    #[test]
    fn fallback_overlap_is_exact() {
        // Separator-free text: the fallback pieces make the merge's carried
        // overlap exactly `chunk_overlap` for every chunk after the first.
        let text = "y".repeat(100);
        let chunks = split(&text, 30, 10);
        assert_invariants(&text, 30, &chunks);
        for w in chunks.windows(2) {
            assert_eq!(
                w[0].char_end - w[1].char_start,
                10,
                "carried overlap is exactly chunk_overlap"
            );
        }
        assert_eq!(chunks.last().unwrap().char_end, 100, "full coverage");
    }

    #[test]
    fn near_size_overlap_still_progresses() {
        // overlap = size - 1 over a separator-free span: 1-char stride — an
        // output amplification of ~chunk_size, which the budget now refuses
        // by design. The progress rule is what makes the refusal *terminate*
        // deterministically instead of looping; the highest in-budget
        // overlap (75%) still slides with a strict, constant stride.
        let text = "z".repeat(200);
        assert!(
            matches!(
                split_recursive(&text, 64, 63),
                Err(SplitError::OutputBudgetExceeded { .. })
            ),
            "maximum overlap on separator-free text is an amplification refusal"
        );

        let chunks = split(&text, 64, 48); // 75%: exactly the budget's edge
        assert_invariants(&text, 64, &chunks);
        assert!(chunks.len() > 2);
        for w in chunks.windows(2) {
            assert_eq!(
                w[1].char_start - w[0].char_start,
                16,
                "constant stride of size - overlap"
            );
        }
    }

    #[test]
    fn adversarial_separators_progress() {
        // Alternating separators with maximum overlap must terminate with
        // strictly increasing starts (the progress rule) — as a bounded
        // result or as a deterministic amplification refusal, never a hang.
        for text in [". ".repeat(300), " .  .  . x".repeat(50)] {
            match split_recursive(&text, 64, 63) {
                Ok(chunks) => assert_invariants(&text, 64, &chunks),
                Err(SplitError::OutputBudgetExceeded { .. }) => {}
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
        // The same adversarial separators inside the budget keep every
        // structural invariant.
        for text in [". ".repeat(300), " .  .  . x".repeat(50)] {
            let chunks = split(&text, 64, 16);
            assert_invariants(&text, 64, &chunks);
        }
    }

    #[test]
    fn overlap_never_exceeds_requested_maximum() {
        let text = "word ".repeat(200);
        let overlap = 20;
        let chunks = split(&text, 50, overlap);
        assert_invariants(&text, 50, &chunks);
        for w in chunks.windows(2) {
            let carried = w[0].char_end - w[1].char_start;
            assert!(
                carried <= overlap as i64,
                "carried {carried} exceeds requested overlap {overlap}"
            );
        }
    }

    // ---- blank handling ----

    #[test]
    fn whitespace_only_chunks_removed_without_sequence_gaps() {
        // A long run of spaces between words produces blank middle chunks;
        // they are dropped and seq stays contiguous.
        let text = format!("alpha{}omega", " ".repeat(100));
        let chunks = split(&text, 20, 0);
        assert_invariants(&text, 20, &chunks);
        assert!(chunks.iter().any(|c| c.text.contains("alpha")));
        assert!(chunks.iter().any(|c| c.text.contains("omega")));
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.seq, i as i32);
        }
    }

    // ---- the cap ----

    /// 40-char paragraphs at size 64 cannot merge in pairs, so each is its
    /// own chunk: `n` paragraphs → `n` chunks.
    fn n_paragraphs(n: usize) -> String {
        let mut s = String::with_capacity(n * 42);
        for _ in 0..n {
            s.push_str(&"p".repeat(40));
            s.push_str("\n\n");
        }
        s
    }

    #[test]
    fn exactly_the_chunk_cap_succeeds() {
        let text = n_paragraphs(MAX_CHUNKS_PER_DOCUMENT);
        let chunks = split(&text, 64, 0);
        assert_eq!(chunks.len(), MAX_CHUNKS_PER_DOCUMENT);
    }

    #[test]
    fn one_over_the_chunk_cap_fails_atomically() {
        let text = n_paragraphs(MAX_CHUNKS_PER_DOCUMENT + 1);
        match split_recursive(&text, 64, 0) {
            Err(SplitError::TooManyChunks { limit }) => {
                assert_eq!(limit, MAX_CHUNKS_PER_DOCUMENT)
            }
            other => panic!("expected TooManyChunks, got {other:?}"),
        }
    }

    #[test]
    fn separator_free_document_over_the_cap_fails() {
        let text = "q".repeat(MAX_CHUNKS_PER_DOCUMENT * 64 + 65);
        assert!(matches!(
            split_recursive(&text, 64, 0),
            Err(SplitError::TooManyChunks { .. })
        ));
    }

    #[test]
    fn whitespace_heavy_document_does_not_trip_the_cap() {
        // Only non-blank chunks count toward the limit: a document that is
        // almost entirely whitespace still succeeds.
        let text = format!("start{}end", " ".repeat(MAX_CHUNKS_PER_DOCUMENT * 64));
        let chunks = split(&text, 64, 0);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("start"));
        assert!(chunks[1].text.contains("end"));
    }

    /// An entirely-whitespace document under an extreme overlap. The
    /// pre-materialization fast path must decide this on the `&str` before
    /// any per-character state exists, or the merger would walk millions of
    /// overlapping spans, each scanned and dropped as blank without charging
    /// any cap. Zero chunks is the documented result for blank documents.
    #[test]
    fn all_whitespace_document_with_extreme_overlap_is_zero_chunks_instantly() {
        let text = " ".repeat(1_000_000);
        assert_eq!(split_recursive(&text, 100_000, 99_999), Ok(Vec::new()));
    }

    /// The mixed-content variant cannot take the blank-document fast path, so
    /// the budget itself must bound the work: every attempted span is charged
    /// BEFORE its blank scan, and the split aborts after ~budget characters
    /// of scanning instead of walking ~500,000 stride-1 blank spans.
    #[test]
    fn blank_span_scans_are_charged_against_the_budget() {
        let text = format!("x{}y", " ".repeat(500_000));
        match split_recursive(&text, 100_000, 99_999) {
            Err(SplitError::OutputBudgetExceeded { chars, budget }) => {
                assert_eq!(budget, text.chars().count() * MAX_OUTPUT_AMPLIFICATION);
                assert!(chars > budget);
                assert!(chars <= budget + 100_000, "aborts on the first overrun");
            }
            other => panic!("expected OutputBudgetExceeded, got {other:?}"),
        }
    }

    /// The adversarial output-amplification case from review: a ~440 KiB
    /// document of four-byte characters with `overlap = size - 1` would
    /// materialize ~10,000 nearly-identical 100,000-character strings
    /// (gigabytes). The cumulative output budget refuses it while the chunks
    /// are still 16-byte spans.
    #[test]
    fn overlap_amplification_is_refused_before_materialization() {
        let text = "\u{1F980}".repeat(109_999); // 4-byte crabs, ~440 KiB
        match split_recursive(&text, 100_000, 99_999) {
            Err(SplitError::OutputBudgetExceeded { chars, budget }) => {
                assert_eq!(budget, 109_999 * MAX_OUTPUT_AMPLIFICATION);
                assert!(chars > budget);
                // Fails after a handful of spans, not after 10,000 strings.
                assert!(chars <= budget + 100_000, "aborts on the first overrun");
            }
            other => panic!("expected OutputBudgetExceeded, got {other:?}"),
        }

        // A 90% overlap (10x amplification) is likewise refused ...
        let text = "z".repeat(1_000);
        assert!(matches!(
            split_recursive(&text, 100, 90),
            Err(SplitError::OutputBudgetExceeded { .. })
        ));
        // ... while overlaps up to 75% of chunk_size always fit the budget:
        // 50% doubles the output, 75% quadruples it.
        assert!(split_recursive(&text, 100, 50).is_ok());
        assert!(split_recursive(&text, 100, 75).is_ok());
        // The defaults (10% overlap) are nowhere near the bound.
        let text = "word ".repeat(20_000);
        assert!(split_recursive(&text, 2_000, 200).is_ok());
    }

    #[test]
    fn oversized_document_is_refused_by_bytes_before_splitting() {
        // Multi-byte characters: the bound is UTF-8 bytes, not characters.
        let text = "ü".repeat(MAX_DOCUMENT_BYTES / 2 + 1); // 2 bytes each
        match split_recursive(&text, 100_000, 0) {
            Err(SplitError::DocumentTooLarge { bytes, limit }) => {
                assert_eq!(bytes, text.len());
                assert_eq!(limit, MAX_DOCUMENT_BYTES);
            }
            other => panic!("expected DocumentTooLarge, got {other:?}"),
        }
        // Exactly at the limit is accepted (geometry keeps it under the
        // chunk cap).
        let text = "a".repeat(MAX_DOCUMENT_BYTES);
        assert!(split_recursive(&text, 100_000, 0).is_ok());
    }

    // ---- geometry ----

    #[test]
    fn geometry_refusals() {
        assert!(matches!(
            split_recursive("x", 0, 0),
            Err(SplitError::Geometry(_))
        ));
        assert!(matches!(
            split_recursive("x", 64, -1),
            Err(SplitError::Geometry(_))
        ));
        assert!(matches!(
            split_recursive("x", 64, 64),
            Err(SplitError::Geometry(_))
        ));
        assert!(matches!(
            split_recursive("x", 64, 100),
            Err(SplitError::Geometry(_))
        ));
    }

    // ---- determinism ----

    #[test]
    fn same_input_same_output() {
        let text = "Mixed content. With sentences! And\n\nparagraphs, plus 🦀 emoji and \
                    väldigt long words här; "
            .repeat(20);
        let a = split(&text, 120, 24);
        let b = split(&text, 120, 24);
        assert_eq!(a, b);
        assert_invariants(&text, 120, &a);
    }
}
