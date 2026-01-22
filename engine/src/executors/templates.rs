// File: engine/src/executors/templates.rs
//! ## Input templates
//!
//! Small helper used by `transformer-sequence-embedding` to wrap an input
//! string with a per-`input_type` template (e.g. `search_query`,
//! `search_document`). Backed by `minijinja`.
//!
//! Configured under `executor.params.templates`:
//!
//! ```json
//! "templates": {
//!   "search_query":    "assets/search_query.md",
//!   "search_document": "assets/search_document.md"
//! }
//! ```
//!
//! Paths are resolved relative to the model directory (the same convention
//! used by the tokenizer's `pretrained_vocab_file`).
//!
//! The template's only variable is `{{ text }}` — the raw input string.

use crate::error::EngineError;
use minijinja::{context, Environment};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A compiled set of input templates, keyed by `input_type`.
pub struct TemplateSet {
    env: Environment<'static>,
    names: Vec<String>,
}

impl TemplateSet {
    /// Load and compile templates from the `executor.params.templates` JSON
    /// value. Each entry's value is a path (relative or absolute) to a Jinja
    /// template file.
    ///
    /// Returns `Ok(None)` when `templates` is absent / null / empty so callers
    /// can keep the legacy "no templates configured" path cleanly.
    pub fn load(
        templates_value: Option<&Value>,
        model_dir: &Path,
    ) -> Result<Option<Self>, EngineError> {
        let Some(value) = templates_value else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let map = value.as_object().ok_or_else(|| {
            EngineError::Configuration(
                "executor.params.templates must be a JSON object mapping input_type → path".into(),
            )
        })?;
        if map.is_empty() {
            return Ok(None);
        }

        // Use BTreeMap for stable iteration order in logs / error messages.
        let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (name, rel) in map {
            let path_str = rel.as_str().ok_or_else(|| {
                EngineError::Configuration(format!(
                    "Template path for input_type '{}' must be a string, got {}",
                    name, rel
                ))
            })?;
            let candidate = Path::new(path_str);
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                model_dir.join(candidate)
            };
            by_name.insert(name.clone(), resolved);
        }

        let mut env = Environment::new();
        let mut names = Vec::with_capacity(by_name.len());
        for (name, path) in by_name {
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                EngineError::Configuration(format!(
                    "Failed to read template '{}' from '{}': {}",
                    name,
                    path.display(),
                    e
                ))
            })?;
            env.add_template_owned(name.clone(), contents)
                .map_err(|e| {
                    EngineError::Configuration(format!(
                        "Failed to compile template '{}' from '{}': {}",
                        name,
                        path.display(),
                        e
                    ))
                })?;
            names.push(name);
        }

        Ok(Some(Self { env, names }))
    }

    /// Returns the list of configured template names (sorted, for logs).
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// True if a template for this `input_type` is configured.
    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// Render the named template with `{{ text }}` bound for every input row.
    pub fn render(&self, name: &str, texts: &[String]) -> Result<Vec<String>, EngineError> {
        let tmpl = self.env.get_template(name).map_err(|e| {
            EngineError::Prediction(format!("Template '{}' not registered: {}", name, e))
        })?;
        texts
            .iter()
            .map(|t| {
                tmpl.render(context! { text => t }).map_err(|e| {
                    EngineError::Prediction(format!("Failed to render template '{}': {}", name, e))
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Create a temp model dir with the given (filename, contents) template files.
    fn temp_model_dir(files: &[(&str, &str)]) -> PathBuf {
        // Use a name unique to this process + a counter derived from the inputs to
        // avoid collisions between tests within the same binary.
        let dir = std::env::temp_dir().join(format!(
            "engine_templates_{}_{}",
            std::process::id(),
            files.iter().map(|(n, _)| n.len()).sum::<usize>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn load_returns_none_for_absent_or_empty() {
        let dir = std::env::temp_dir();
        assert!(TemplateSet::load(None, &dir).unwrap().is_none());
        assert!(TemplateSet::load(Some(&Value::Null), &dir)
            .unwrap()
            .is_none());
        assert!(TemplateSet::load(Some(&json!({})), &dir).unwrap().is_none());
    }

    // NOTE: `TemplateSet` has no `Debug` impl (it wraps a minijinja
    // `Environment`), so the error cases use `matches!` rather than
    // `unwrap_err()` (which would require `T: Debug`).
    #[test]
    fn load_rejects_non_object() {
        let dir = std::env::temp_dir();
        assert!(matches!(
            TemplateSet::load(Some(&json!("not-an-object")), &dir),
            Err(EngineError::Configuration(_))
        ));
    }

    #[test]
    fn load_rejects_non_string_path() {
        let dir = std::env::temp_dir();
        assert!(matches!(
            TemplateSet::load(Some(&json!({ "search_query": 123 })), &dir),
            Err(EngineError::Configuration(_))
        ));
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = std::env::temp_dir().join("engine_templates_missing");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            TemplateSet::load(Some(&json!({ "q": "does_not_exist.md" })), &dir),
            Err(EngineError::Configuration(_))
        ));
    }

    #[test]
    fn render_substitutes_text_per_row() {
        let dir = temp_model_dir(&[
            ("query.md", "query: {{ text }}"),
            ("doc.md", "passage: {{ text }}"),
        ]);
        let templates = json!({ "search_query": "query.md", "search_document": "doc.md" });
        let set = TemplateSet::load(Some(&templates), &dir).unwrap().unwrap();

        assert!(set.contains("search_query"));
        assert!(set.contains("search_document"));
        assert!(!set.contains("nope"));
        // names() is sorted (BTreeMap iteration).
        assert_eq!(
            set.names(),
            &["search_document".to_string(), "search_query".to_string()]
        );

        let rendered = set
            .render("search_query", &["hello".to_string(), "world".to_string()])
            .unwrap();
        assert_eq!(rendered, vec!["query: hello", "query: world"]);

        let docs = set
            .render("search_document", &["body".to_string()])
            .unwrap();
        assert_eq!(docs, vec!["passage: body"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_unknown_template_errors() {
        let dir = temp_model_dir(&[("only.md", "{{ text }}")]);
        let set = TemplateSet::load(Some(&json!({ "only": "only.md" })), &dir)
            .unwrap()
            .unwrap();
        let err = set.render("missing", &["x".to_string()]).unwrap_err();
        assert!(matches!(err, EngineError::Prediction(_)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
