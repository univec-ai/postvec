//! Rendering: human output for people, versioned JSON for automation.
//!
//! Two invariants:
//!
//! - **JSON is exactly one object on stdout.** Progress from mutating commands
//!   goes to stderr, so `postvec setup --format json | jq` never chokes on a
//!   status line.
//! - **Colour is never the only signal.** Every status is spelled out, so the
//!   output is readable in a pipe, a log, and a CI transcript.

use crate::checks::{CheckResult, CheckStatus, Report};
use crate::cli::OutputFormat;
use crate::error::CliError;
use crate::plan::Plan;
use crate::proc;
use std::io::Write;

/// ANSI styling, applied only on a real terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn resolve(no_color: bool) -> Self {
        // NO_COLOR is honoured whatever its value, per the convention, and
        // beats CLICOLOR_FORCE / FORCE_COLOR.
        let forced = !no_color
            && (std::env::var_os("CLICOLOR_FORCE").is_some()
                || std::env::var_os("FORCE_COLOR").is_some());
        let disabled = no_color
            || std::env::var_os("NO_COLOR").is_some()
            || (!proc::is_stdout_tty() && !forced);
        Self { enabled: !disabled }
    }

    pub fn is_enabled(self) -> bool {
        self.enabled
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn status(&self, status: CheckStatus) -> String {
        let code = match status {
            CheckStatus::Pass => "32",
            CheckStatus::Warn => "33",
            CheckStatus::Fail => "31",
            CheckStatus::Skip => "90",
        };
        self.paint(code, status.label())
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("90", text)
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn success(&self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn fail(&self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn tone(&self, tone: Tone, text: &str) -> String {
        match tone {
            Tone::Plain => text.to_string(),
            Tone::Dim => self.dim(text),
            Tone::Bold => self.bold(text),
            Tone::Success => self.success(text),
            Tone::Warn => self.warn(text),
            Tone::Fail => self.fail(text),
            Tone::Accent => self.accent(text),
        }
    }
}

/// Column alignment for [`render_table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Semantic colour for a table cell. Colour is never the only signal: the
/// text is complete without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Bold,
    Success,
    Warn,
    Fail,
    Accent,
}

/// One column of a human table. Widths are fitted to the terminal: a column
/// may shrink as far as `min`, highest `shrink` first, and never grows past
/// `max`.
#[derive(Debug, Clone)]
pub struct Column {
    pub header: &'static str,
    pub align: Align,
    pub min: usize,
    pub max: usize,
    pub shrink: u8,
}

/// One table cell. `text` is what width is measured against; [`Tone`] is
/// applied after any truncation so escape codes cannot break the layout.
#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: Tone::Plain,
        }
    }

    pub fn with_tone(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// Fit and render a table. `width` is the whole line budget, including the
/// two-space gaps between columns. Overflow past every column's `min` is
/// left as-is — better a wrapped line than a table that dropped a field.
pub fn render_table(style: &Style, columns: &[Column], rows: &[Vec<Cell>], width: usize) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let widths = fit_widths(columns, rows, width);
    let mut out = String::new();
    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, col)| pad(&truncate(col.header, widths[i]), widths[i], col.align))
        .collect();
    out.push_str(&style.dim(&header.join("  ")));
    out.push('\n');
    for row in rows {
        let mut cells = Vec::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            let cell = row.get(i).cloned().unwrap_or_else(|| Cell::new(""));
            let clipped = truncate(&cell.text, widths[i]);
            let painted = style.tone(cell.tone, &clipped);
            cells.push(pad(&painted, widths[i], col.align));
        }
        out.push_str(&cells.join("  "));
        out.push('\n');
    }
    out
}

fn fit_widths(columns: &[Column], rows: &[Vec<Cell>], width: usize) -> Vec<usize> {
    let gaps = columns.len().saturating_sub(1) * 2;
    let mut widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let content = rows
                .iter()
                .filter_map(|row| row.get(i))
                .map(|cell| display_width(&cell.text))
                .max()
                .unwrap_or(0);
            display_width(col.header)
                .max(content)
                .clamp(col.min, col.max)
        })
        .collect();
    let mut total = widths.iter().sum::<usize>() + gaps;
    if total <= width {
        return widths;
    }
    let mut order: Vec<usize> = (0..columns.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(columns[i].shrink));
    // Highest `shrink` first, one cell at a time, stop as soon as we
    // fit — so a long NAME absorbs the overflow before LICENSE is
    // reduced to "LI…".
    while total > width {
        match order.iter().copied().find(|&i| widths[i] > columns[i].min) {
            Some(i) => {
                widths[i] -= 1;
                total -= 1;
            }
            None => break,
        }
    }
    widths
}

/// Visible character count. ANSI is not expected in the *source* text we
/// measure — styling is applied after truncation.
pub fn display_width(text: &str) -> usize {
    text.chars().count()
}

/// Truncate to `max` visible characters, using a single ellipsis when
/// anything is dropped. `max == 0` yields an empty string.
pub fn truncate(text: &str, max: usize) -> String {
    let width = display_width(text);
    if width <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let visible = display_width(&strip_ansi(text));
    if visible >= width {
        return text.to_string();
    }
    let pad = " ".repeat(width - visible);
    match align {
        Align::Left => format!("{text}{pad}"),
        Align::Right => format!("{pad}{text}"),
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Where a command's output goes.
pub struct Output {
    pub format: OutputFormat,
    pub style: Style,
    /// Override for tests so golden output does not depend on the tty.
    width: Option<usize>,
}

impl Output {
    pub fn new(format: OutputFormat, no_color: bool) -> Self {
        Self {
            format,
            style: Style::resolve(no_color),
            width: None,
        }
    }

    /// Visible columns for human tables and wrapping.
    pub fn columns(&self) -> usize {
        self.width.unwrap_or_else(proc::stdout_width)
    }

    /// Render a table fitted to this output's width and style.
    pub fn render_table(&self, columns: &[Column], rows: &[Vec<Cell>]) -> String {
        render_table(&self.style, columns, rows, self.columns())
    }

    pub fn is_json(&self) -> bool {
        self.format == OutputFormat::Json
    }

    /// Human-facing progress. Always stderr, so stdout stays parseable.
    pub fn progress(&self, message: &str) {
        if !self.is_json() {
            eprintln!("{message}");
        }
    }

    pub fn note(&self, message: &str) {
        if !self.is_json() {
            eprintln!("{}", self.style.dim(message));
        }
    }

    /// Render a plan before asking for confirmation.
    pub fn show_plan(&self, plan: &Plan) {
        if self.is_json() {
            // The plan is part of the command's JSON result, not a separate
            // document, so nothing is printed here.
            return;
        }
        eprintln!("{}", self.style.bold(&plan.headline()));
        for line in plan.describe() {
            eprintln!("  {line}");
        }
        if plan.restart_required {
            eprintln!(
                "  {}",
                self.style.bold(&format!(
                    "a restart of {} is required; connections will be interrupted",
                    plan.restart_label
                        .clone()
                        .unwrap_or_else(|| "the cluster".to_string())
                ))
            );
        }
        if plan.destructive {
            eprintln!(
                "  {}",
                self.style
                    .bold("this plan destroys data that cannot be recovered")
            );
        }
        eprintln!();
    }

    /// Render a doctor report.
    pub fn show_report(&self, report: &Report) -> Result<(), CliError> {
        match self.format {
            OutputFormat::Json => {
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer_pretty(&mut stdout, report)?;
                writeln!(stdout)?;
                Ok(())
            }
            OutputFormat::Human => {
                let mut stdout = std::io::stdout().lock();
                write!(stdout, "{}", self.render_report(report))?;
                Ok(())
            }
        }
    }

    /// Emit a read-only command's document: in JSON mode, exactly one object
    /// on stdout; in human mode, the caller's pre-rendered lines. Model
    /// listing/inspection commands use this — they are neither a check
    /// [`Report`] nor a mutating [`CommandResult`].
    pub fn show_document(
        &self,
        value: &impl serde::Serialize,
        human: &str,
    ) -> Result<(), CliError> {
        let mut stdout = std::io::stdout().lock();
        match self.format {
            OutputFormat::Json => {
                serde_json::to_writer_pretty(&mut stdout, value)?;
                writeln!(stdout)?;
            }
            OutputFormat::Human => {
                write!(stdout, "{human}")?;
            }
        }
        Ok(())
    }

    /// The human report, as a string (so it can be golden-tested).
    pub fn render_report(&self, report: &Report) -> String {
        let mut out = String::new();
        out.push_str(&format!("postvec doctor {}\n", report.cli_version));
        let mut header = format!("Cluster: {}", report.cluster.id);
        if let Some(version) = &report.cluster.postgres_version {
            header.push_str("  ");
            header.push_str(&self.style.dim(&format!("PostgreSQL: {version}")));
        }
        if let Some(mode) = &report.cluster.mode {
            header.push_str("  ");
            header.push_str(&self.style.dim(&format!("Mode: {mode}")));
        }
        out.push_str(&header);
        out.push('\n');
        if report.concurrent_change_possible {
            out.push_str(&format!(
                "{}\n",
                self.style
                    .dim("(no host lock held; a concurrent setup/uninstall cannot be excluded)")
            ));
        }
        out.push('\n');

        // Capped: a single long endpoint scope would otherwise push every
        // summary far to the right. Longer scopes overflow their own row only.
        const MAX_SCOPE_WIDTH: usize = 20;
        let scope_width = report
            .checks
            .iter()
            .map(|check| display_scope(&check.scope).len())
            .max()
            .unwrap_or(0)
            .min(MAX_SCOPE_WIDTH);
        let id_width = report
            .checks
            .iter()
            .map(|check| check.id.len())
            .max()
            .unwrap_or(0);
        for check in &report.checks {
            // Trailing whitespace is trimmed so a cluster-scope row (which has
            // an empty scope column) does not leave a ragged tail in diffs and
            // logs.
            let line = format!(
                "{:<4}  {:<id_width$}  {:<scope_width$}  {}",
                self.style.status(check.status),
                check.id,
                display_scope(&check.scope),
                check.summary,
                id_width = id_width,
                scope_width = scope_width,
            );
            out.push_str(line.trim_end());
            out.push('\n');
            if let Some(remediation) = &check.remediation {
                // Wrapped, never truncated: the fix is the useful half.
                out.push_str(&self.render_remediation(remediation, "      "));
            }
        }
        out.push('\n');
        out.push_str(&format!(
            "Summary: {} pass, {} warn, {} fail, {} skipped ({} ms)\n",
            self.style.success(&report.summary.pass.to_string()),
            self.style.warn(&report.summary.warn.to_string()),
            self.style.fail(&report.summary.fail.to_string()),
            report.summary.skip,
            report.duration_ms,
        ));
        out
    }

    /// A remediation block: the label once, continuations aligned under it.
    fn render_remediation(&self, remediation: &str, indent: &str) -> String {
        let label = "fix:";
        let text_width = self
            .columns()
            .saturating_sub(indent.len() + label.len() + 1)
            .max(40);
        let mut out = String::new();
        for (index, line) in wrap(remediation, text_width).into_iter().enumerate() {
            if index == 0 {
                out.push_str(&format!("{indent}{} {line}\n", self.style.dim(label)));
            } else {
                out.push_str(&format!(
                    "{indent}{:width$} {line}\n",
                    "",
                    width = label.len()
                ));
            }
        }
        out
    }

    /// Render a command result (setup/uninstall) once it has finished.
    pub fn show_result(&self, result: &CommandResult) -> Result<(), CliError> {
        match self.format {
            OutputFormat::Json => {
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer_pretty(&mut stdout, result)?;
                writeln!(stdout)?;
                Ok(())
            }
            OutputFormat::Human => {
                for line in &result.messages {
                    println!("{line}");
                }
                if !result.checks.is_empty() {
                    println!();
                    for check in &result.checks {
                        println!(
                            "{:<4}  {}  {}",
                            self.style.status(check.status),
                            check.id,
                            check.summary
                        );
                        if let Some(remediation) = &check.remediation {
                            print!("{}", self.render_remediation(remediation, "      "));
                        }
                    }
                }
                if let Some(next) = &result.next_step {
                    println!();
                    println!("{}", self.style.bold(next));
                }
                Ok(())
            }
        }
    }

    /// Report a failure. In JSON mode this is a versioned error envelope, so a
    /// caller never has to parse prose.
    pub fn show_error(&self, command: &'static str, error: &CliError) {
        match self.format {
            OutputFormat::Json => {
                let envelope = serde_json::json!({
                    "schema_version": crate::checks::SCHEMA_VERSION,
                    "command": command,
                    "cli_version": crate::CLI_VERSION,
                    "error": {
                        "kind": error.kind(),
                        "message": error.to_string(),
                        "remediation": error.remediation(),
                    },
                    "exit_code": error.exit().code(),
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&envelope)
                        .unwrap_or_else(|_| "{\"error\":\"unserializable\"}".to_string())
                );
            }
            OutputFormat::Human => {
                eprintln!("{}: {error}", self.style.fail("postvec"));
                if let Some(remediation) = error.remediation() {
                    eprint!("{}", self.render_remediation(remediation, "  "));
                }
            }
        }
    }
}

/// The result of a mutating command.
#[derive(Debug, serde::Serialize)]
pub struct CommandResult {
    pub schema_version: u32,
    pub command: &'static str,
    pub cli_version: String,
    pub cluster: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub plan: Plan,
    /// What was actually done, in order.
    pub applied: Vec<String>,
    /// Human-facing summary lines.
    pub messages: Vec<String>,
    /// Post-apply verification.
    pub checks: Vec<CheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    pub exit_code: i32,
}

/// `cluster` is implied by the header, so drop the prefix noise from per-scope
/// columns and leave the interesting part.
fn display_scope(scope: &str) -> &str {
    if scope == "cluster" {
        ""
    } else {
        scope.split_once(':').map(|(_, rest)| rest).unwrap_or(scope)
    }
}

/// Wrap at word boundaries. Long unbreakable tokens (paths, URLs) are left
/// intact rather than split, because a broken path is unusable.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{ReportCluster, Summary, SCHEMA_VERSION};

    fn report() -> Report {
        let checks = vec![
            CheckResult::pass("cluster.preload", "cluster", "postvec is active"),
            CheckResult::pass(
                "extension.version",
                "database:univec",
                "SQL 0.1.0, library 0.1.0, diagnostics API 1",
            ),
            CheckResult::fail(
                "embedded.loaded-models",
                "embedded",
                "1 expected model is not loaded: convert-a-to-b",
            )
            .with_fix("inspect journalctl -u postgresql@18-main.service --since -10m"),
            CheckResult::warn(
                "registry.indexes",
                "database:univec",
                "public.docs.body_semantic has no ANN index",
            ),
        ];
        Report {
            schema_version: SCHEMA_VERSION,
            command: "doctor",
            cli_version: "0.1.0".into(),
            cluster: ReportCluster {
                id: "18/main".into(),
                postgres_major: Some(18),
                postgres_version: Some("18.4".into()),
                mode: Some("embedded".into()),
            },
            started_at: "2026-07-30T12:00:00Z".into(),
            duration_ms: 842,
            summary: Summary::of(&checks),
            checks,
            concurrent_change_possible: false,
        }
    }

    fn plain() -> Output {
        Output {
            format: OutputFormat::Human,
            style: Style { enabled: false },
            width: Some(80),
        }
    }

    #[test]
    fn human_report_is_stable_and_aligned() {
        let rendered = plain().render_report(&report());
        let expected = "\
postvec doctor 0.1.0
Cluster: 18/main  PostgreSQL: 18.4  Mode: embedded

PASS  cluster.preload                   postvec is active
PASS  extension.version       univec    SQL 0.1.0, library 0.1.0, diagnostics API 1
FAIL  embedded.loaded-models  embedded  1 expected model is not loaded: convert-a-to-b
      fix: inspect journalctl -u postgresql@18-main.service --since -10m
WARN  registry.indexes        univec    public.docs.body_semantic has no ANN index

Summary: 2 pass, 1 warn, 1 fail, 0 skipped (842 ms)
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn status_words_are_present_without_colour() {
        let rendered = plain().render_report(&report());
        assert!(!rendered.contains('\x1b'), "no escapes when styling is off");
        for label in ["PASS", "WARN", "FAIL"] {
            assert!(rendered.contains(label));
        }
    }

    #[test]
    fn colour_is_never_the_only_signal() {
        let styled = Output {
            format: OutputFormat::Human,
            style: Style { enabled: true },
            width: Some(80),
        };
        let rendered = styled.render_report(&report());
        assert!(rendered.contains('\x1b'));
        // The words survive alongside the escapes.
        assert!(rendered.contains("PASS\x1b[0m"));
        assert!(rendered.contains("FAIL\x1b[0m"));
    }

    #[test]
    fn json_report_has_the_documented_shape() {
        let value = serde_json::to_value(report()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], "doctor");
        assert_eq!(value["cluster"]["postgres_major"], 18);
        assert_eq!(value["summary"]["fail"], 1);
        assert_eq!(value["checks"].as_array().unwrap().len(), 4);
        let check = &value["checks"][0];
        assert_eq!(check["id"], "cluster.preload");
        assert_eq!(check["status"], "PASS");
        // Absent optional fields are omitted rather than serialized as null.
        assert!(check.get("evidence").is_none());
        assert!(check.get("remediation").is_none());
        assert!(check.get("required").is_none());
    }

    #[test]
    fn concurrent_change_is_flagged_in_both_formats() {
        let mut report = report();
        report.concurrent_change_possible = true;
        assert!(plain().render_report(&report).contains("no host lock held"));
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["concurrent_change_possible"], true);
    }

    #[test]
    fn remediation_wraps_instead_of_truncating() {
        let long = "a".repeat(40) + " " + &"b".repeat(40) + " tail";
        let lines = wrap(&long, 50);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].len() <= 50);
        assert!(lines.join(" ").contains("tail"));
        assert_eq!(
            lines.iter().map(|l| l.len()).sum::<usize>() + lines.len() - 1,
            long.len(),
            "wrapping loses nothing"
        );
    }

    #[test]
    fn long_unbreakable_tokens_are_not_split() {
        let path = "/very/long/path/".repeat(10);
        let lines = wrap(&path, 20);
        assert_eq!(lines.len(), 1, "a path must stay usable");
        assert_eq!(lines[0], path);
    }

    #[test]
    fn scope_column_drops_the_prefix() {
        assert_eq!(display_scope("cluster"), "");
        assert_eq!(display_scope("database:univec"), "univec");
        assert_eq!(display_scope("endpoint:192.0.2.2:33333"), "192.0.2.2:33333");
        assert_eq!(display_scope("embedded"), "embedded");
    }

    #[test]
    fn json_error_envelope_is_versioned_and_carries_the_fix() {
        let error = CliError::precondition("pgvector is missing").with_fix("install it");
        let envelope = serde_json::json!({
            "schema_version": crate::checks::SCHEMA_VERSION,
            "command": "setup",
            "cli_version": "0.1.0",
            "error": {
                "kind": error.kind(),
                "message": error.to_string(),
                "remediation": error.remediation(),
            },
            "exit_code": error.exit().code(),
        });
        assert_eq!(envelope["error"]["kind"], "precondition");
        assert_eq!(envelope["error"]["remediation"], "install it");
        assert_eq!(envelope["exit_code"], 1);
    }

    #[test]
    fn styling_is_disabled_when_no_color_is_requested() {
        assert!(!Style::resolve(true).enabled);
    }

    #[test]
    fn truncate_keeps_short_text_and_marks_the_cut() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 1), "…");
        assert_eq!(truncate("ab", 0), "");
    }

    #[test]
    fn table_fits_a_narrow_terminal_by_truncating_flexible_columns() {
        let columns = [
            Column {
                header: "NAME",
                align: Align::Left,
                min: 8,
                max: 40,
                shrink: 2,
            },
            Column {
                header: "SIZE",
                align: Align::Right,
                min: 4,
                max: 10,
                shrink: 0,
            },
            Column {
                header: "STATE",
                align: Align::Left,
                min: 4,
                max: 12,
                shrink: 1,
            },
        ];
        let rows = vec![vec![
            Cell::new("sentence-transformers-all-minilm-l6-v2"),
            Cell::new("91.2 MB"),
            Cell::new("installed"),
        ]];
        let rendered = render_table(&Style { enabled: false }, &columns, &rows, 40);
        for line in rendered.lines() {
            assert!(
                display_width(line) <= 40,
                "{line:?} is {} columns",
                display_width(line)
            );
        }
        assert!(rendered.contains('…'), "{rendered}");
        assert!(rendered.contains("91.2 MB"), "{rendered}");
        assert!(rendered.contains("NAME"), "{rendered}");
    }

    #[test]
    fn table_colour_is_applied_after_truncation() {
        let columns = [Column {
            header: "NAME",
            align: Align::Left,
            min: 4,
            max: 8,
            shrink: 1,
        }];
        let rows = vec![vec![Cell::with_tone("installed", Tone::Success)]];
        let rendered = render_table(&Style { enabled: true }, &columns, &rows, 20);
        assert!(rendered.contains('\x1b'), "{rendered}");
        assert!(
            rendered.contains("install…"),
            "truncation must happen before colour is applied:\n{rendered}"
        );
    }
}
