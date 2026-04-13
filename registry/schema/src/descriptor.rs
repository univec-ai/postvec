//! One canonical reading of `ninference.hub.json` for registry purposes,
//! shared by the client installer and the publisher.
//!
//! The registry-relevant subset of a descriptor lives here, once, so the
//! publisher and installer cannot disagree about it. The rules:
//!
//! - **No guessed load-bearing defaults.** A published descriptor must state
//!   its `backend` and `params.model_type` explicitly; the publisher refuses
//!   anything else, and the installer refuses an archive whose descriptor
//!   disagrees with the index entry that advertised it.
//! - **Every runtime file reference is a required asset.** `file_path` plus
//!   every tokenizer file reference the engine understands
//!   (`pretrained_vocab_file`, `vocab_file_path`, `merges_file_path`) — from
//!   every object stored under a `tokenizer` key anywhere in the descriptor,
//!   because the engine's executors also consume nested shapes
//!   (`executor.params.transformer.tokenizer`, encoder/decoder tokenizers).
//!   The publisher ships them or fails; the installer requires them present
//!   or fails.
//! - **Checked numeric conversions.** A dimension that does not fit `u32` is
//!   an error, never a silent truncation.
//!
//! This module deliberately has no dependency on the `engine` crate: it must
//! stay easy to audit, and the publisher separately proves "the engine can
//! parse this" with the real engine schema.

use crate::IndexModel;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// The registry-relevant subset of a model descriptor.
#[derive(Debug, Clone)]
pub struct Descriptor {
    pub name: String,
    pub enabled: bool,
    pub backend: Option<String>,
    pub dependencies: Vec<String>,
    pub model_type: Option<String>,
    pub source_model: Option<String>,
    pub target_model: Option<String>,
    pub source_dim: Option<u32>,
    pub target_dim: Option<u32>,
    pub sequence_len: Option<u32>,
    pub eval: Option<Value>,
    /// Relative paths the engine dereferences at load time, deduplicated:
    /// the weight file and every tokenizer file reference.
    pub required_assets: Vec<RequiredAsset>,
    /// Every `tokenizer_type` the descriptor declares (under either
    /// tokenizer location). The publisher gates registry admission on these:
    /// a shape without a load-level fixture is not publishable.
    pub tokenizer_types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RequiredAsset {
    /// What the reference is, for messages ("file_path", "tokenizer
    /// vocab_file_path", …).
    pub what: String,
    /// The referenced path, relative to the model directory.
    pub path: String,
}

#[derive(Debug, Deserialize)]
struct RawDescriptor {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    params: serde_json::Map<String, Value>,
    // `executor` is deliberately NOT modelled here: tokenizer harvesting
    // walks the whole raw JSON (see `walk_tokenizers`), so no fixed executor
    // shape is load-bearing.
}

/// The tokenizer file-reference keys the engine's tokenizer configuration
/// understands (engine/src/tokenizers/config.rs). `pretrained_name` is a hub
/// identifier, not a file, and is deliberately absent.
const TOKENIZER_FILE_KEYS: &[&str] = &[
    "pretrained_vocab_file",
    "vocab_file_path",
    "merges_file_path",
];

impl Descriptor {
    /// Parse a descriptor body. Errors are plain strings so the publisher
    /// (anyhow) and the installer (CliError) can each wrap them.
    pub fn parse(content: &str) -> Result<Descriptor, String> {
        let raw: RawDescriptor =
            serde_json::from_str(content).map_err(|e| format!("descriptor does not parse: {e}"))?;
        if raw.name.trim().is_empty() {
            return Err("descriptor field \"name\" is empty".to_string());
        }

        let mut assets: Vec<RequiredAsset> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        if let Some(file_path) = &raw.file_path {
            if !file_path.trim().is_empty() && seen.insert(file_path.clone()) {
                assets.push(RequiredAsset {
                    what: "file_path".to_string(),
                    path: file_path.clone(),
                });
            }
        }
        // Tokenizer configurations are harvested by walking the entire
        // descriptor for objects held under a key named exactly `tokenizer`.
        // The engine's executors consume nested shapes too
        // (`executor.params.transformer.tokenizer`, encoder/decoder
        // tokenizers in generation executors), and an admission gate that
        // only looked at two fixed paths could be bypassed by any of them.
        // A similarly-named key (`detokenizer`, `tokenizer_config`) is not
        // a tokenizer configuration and is deliberately not harvested.
        let root_value: Value =
            serde_json::from_str(content).map_err(|e| format!("descriptor does not parse: {e}"))?;
        let mut tokenizer_types: Vec<String> = Vec::new();
        walk_tokenizers(&root_value, &mut |tokenizer| {
            if let Some(kind) = tokenizer.get("tokenizer_type").and_then(|v| v.as_str()) {
                if !tokenizer_types.iter().any(|t| t == kind) {
                    tokenizer_types.push(kind.to_string());
                }
            }
            let Some(node) = tokenizer.get("params") else {
                return;
            };
            for key in TOKENIZER_FILE_KEYS {
                if let Some(path) = node.get(*key).and_then(|v| v.as_str()) {
                    if !path.trim().is_empty() && seen.insert(path.to_string()) {
                        assets.push(RequiredAsset {
                            what: format!("tokenizer {key}"),
                            path: path.to_string(),
                        });
                    }
                }
            }
        });

        Ok(Descriptor {
            name: raw.name,
            enabled: raw.enabled,
            backend: raw.backend.filter(|b| !b.trim().is_empty()),
            dependencies: raw.dependencies,
            model_type: param_str(&raw.params, "model_type"),
            source_model: param_str(&raw.params, "source_model"),
            target_model: param_str(&raw.params, "target_model"),
            source_dim: param_u32(&raw.params, "source_dim")?,
            target_dim: param_u32(&raw.params, "target_dim")?,
            sequence_len: param_u32(&raw.params, "sequence_len")?,
            eval: raw.params.get("eval").cloned(),
            required_assets: assets,
            tokenizer_types,
        })
    }

    /// The identity this descriptor declares — what the **engine** will
    /// believe once the directory is installed, which is the only identity
    /// that can move a column's vector space.
    ///
    /// `backend` and `model_type` are `Option` on the descriptor but never
    /// absent in a published one (`validate_against_index` refuses that
    /// before this is called), so the fallbacks here are unreachable in
    /// practice and deliberately empty rather than guessed.
    pub fn identity(&self) -> crate::identity::Identity {
        crate::identity::Identity {
            model_type: self.model_type.clone().unwrap_or_default(),
            backend: self.backend.clone().unwrap_or_default(),
            source_model: self.source_model.clone(),
            target_model: self.target_model.clone(),
            source_dim: self.source_dim,
            target_dim: self.target_dim,
        }
    }

    /// The install-time agreement gate. The **immutable identity** fields —
    /// `name`, `backend`, `model_type`, `source_model`, `target_model`,
    /// `source_dim`, `target_dim` — are hard failures: the engine consumes
    /// the descriptor, so an archive that disagrees with the catalogue that
    /// advertised it is serving a different model than the one the client
    /// asked for, whatever the index says.
    /// Everything else the index duplicates is advertising, and the shipped
    /// descriptor is authoritative once installed, so a disagreement there is
    /// reported as a warning. Returns the warnings.
    pub fn validate_against_index(&self, model: &IndexModel) -> Result<Vec<String>, String> {
        if self.name != model.name {
            return Err(format!(
                "descriptor names itself {:?}; the engine resolves by directory name and would refuse the load",
                self.name
            ));
        }
        if !self.enabled {
            return Err("descriptor is disabled; the engine would never load it".to_string());
        }
        match &self.backend {
            None => {
                return Err(
                    "descriptor states no backend; published descriptors must be explicit"
                        .to_string(),
                )
            }
            Some(backend) if backend != &model.backend => {
                return Err(format!(
                    "descriptor backend {:?} does not match the index's {:?}",
                    backend, model.backend
                ));
            }
            Some(_) => {}
        }
        match &self.model_type {
            None => return Err(
                "descriptor states no params.model_type; published descriptors must be explicit"
                    .to_string(),
            ),
            Some(model_type) if model_type != &model.model_type => {
                return Err(format!(
                    "descriptor model_type {:?} does not match the index's {:?}",
                    model_type, model.model_type
                ));
            }
            Some(_) => {}
        }

        // The vector-space half of the identity contract: a hard error, not
        // advertising. A registry that keeps its index entry stable while
        // serving an archive whose descriptor names another space or shape
        // would otherwise pass preflight and then be believed by the engine.
        for (label, descriptor_value, index_value) in [
            ("source_model", &self.source_model, &model.source_model),
            ("target_model", &self.target_model, &model.target_model),
        ] {
            if descriptor_value != index_value {
                return Err(format!(
                    "descriptor {label} {descriptor_value:?} does not match the index's                      {index_value:?}; the archive declares a different vector space than the                      catalogue advertised"
                ));
            }
        }
        for (label, descriptor_value, index_value) in [
            ("source_dim", self.source_dim, model.source_dim),
            ("target_dim", self.target_dim, model.target_dim),
        ] {
            if descriptor_value != index_value {
                return Err(format!(
                    "descriptor {label} {descriptor_value:?} does not match the index's                      {index_value:?}; the archive declares a different shape than the catalogue                      advertised"
                ));
            }
        }

        let mut warnings: Vec<String> = Vec::new();
        let mine: BTreeSet<&str> = self.dependencies.iter().map(|d| d.as_str()).collect();
        let advertised: BTreeSet<&str> = model.dependencies.iter().map(|d| d.as_str()).collect();
        if mine != advertised {
            warnings.push(format!(
                "{}: descriptor dependencies {:?} differ from the index's {:?}; the \
                 descriptor is authoritative at load time",
                model.name, self.dependencies, model.dependencies
            ));
        }
        // `sequence_len` is advertising: it changes which inputs get
        // truncated, not which space the output lives in.
        if self.sequence_len != model.sequence_len {
            warnings.push(format!(
                "{}: descriptor sequence_len {:?} differs from the index's {:?}",
                model.name, self.sequence_len, model.sequence_len
            ));
        }
        if let Err(problem) = self.check_postvec_requires(model) {
            warnings.push(format!("{}: {problem}", model.name));
        }
        if let Err(problem) = self.check_model_type_shape() {
            warnings.push(format!("{}: {problem}", model.name));
        }
        Ok(warnings)
    }

    /// The `postvec_requires` product rule, derived from the descriptor
    /// alone: a converter needs its source space's embed model and the
    /// `embed-bridge` executor so its target stays writable for new text;
    /// everything else needs nothing. Shared by the publisher (which
    /// writes the index from it) and the installer (which verifies the
    /// index against it).
    pub fn derived_postvec_requires(&self) -> Vec<String> {
        match self.model_type.as_deref() {
            Some("convert") => {
                let mut requires = Vec::new();
                if let Some(source) = &self.source_model {
                    requires.push(source.clone());
                }
                requires.push("embed-bridge".to_string());
                requires
            }
            _ => Vec::new(),
        }
    }

    fn check_postvec_requires(&self, model: &IndexModel) -> Result<(), String> {
        let derived: BTreeSet<String> = self.derived_postvec_requires().into_iter().collect();
        let advertised: BTreeSet<String> = model.postvec_requires.iter().cloned().collect();
        if derived != advertised {
            return Err(format!(
                "the index advertises postvec_requires {:?} but the descriptor derives {:?}; \
                 the installed companion closure would differ from the required one",
                model.postvec_requires,
                self.derived_postvec_requires()
            ));
        }
        Ok(())
    }

    /// Per-model-type structural rules: the fields a type needs to be usable
    /// must be present. The **publisher** enforces this hard, so a
    /// mispublished entry fails at publication; at install time it is only a
    /// warning. The load check already proved usability before the
    /// entry was published.
    pub fn check_model_type_shape(&self) -> Result<(), String> {
        match self.model_type.as_deref() {
            Some("convert") => {
                for (label, present) in [
                    ("source_model", self.source_model.is_some()),
                    ("target_model", self.target_model.is_some()),
                    ("source_dim", self.source_dim.is_some()),
                    ("target_dim", self.target_dim.is_some()),
                ] {
                    if !present {
                        return Err(format!(
                            "a convert descriptor must state params.{label}; without it the \
                             converter's spaces cannot be identified or dimensioned"
                        ));
                    }
                }
            }
            Some("embed") if self.target_dim.is_none() => {
                return Err(
                    "an embed descriptor must state params.target_dim; postvec sizes the \
                     shadow vector column from it"
                        .to_string(),
                );
            }
            // embed-bridge / convert-bridge executors carry no dimensions of
            // their own; nothing further to require here.
            _ => {}
        }
        Ok(())
    }
}

/// Depth-first walk over the whole descriptor: every JSON object stored
/// under a key named exactly `tokenizer` is handed to `harvest`, wherever it
/// nests. Arrays are traversed; scalar values are not tokenizers.
fn walk_tokenizers(value: &Value, harvest: &mut impl FnMut(&Value)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "tokenizer" && child.is_object() {
                    harvest(child);
                }
                walk_tokenizers(child, harvest);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_tokenizers(item, harvest);
            }
        }
        _ => {}
    }
}

fn param_str(params: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

fn param_u32(params: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u32>, String> {
    match params.get(key) {
        None => Ok(None),
        Some(value) => {
            let n = value
                .as_u64()
                .ok_or_else(|| format!("params.{key} is not a non-negative integer: {value}"))?;
            let n = u32::try_from(n).map_err(|_| format!("params.{key} {n} does not fit u32"))?;
            Ok(Some(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchiveInfo, IndexModel};

    fn index_model(name: &str) -> IndexModel {
        IndexModel {
            name: name.into(),
            access: "public".into(),
            model_type: "embed".into(),
            backend: "onnx-runtime".into(),
            quantization: None,
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
            sequence_len: None,
            license: Some("mit".into()),
            license_version: None,
            license_url: None,
            license_acceptance: None,
            source: None,
            dependencies: vec![],
            postvec_requires: vec![],
            min_postvec_version: None,
            published_at: None,
            summary: None,
            eval: None,
            withdrawn: false,
            revision: None,
            required_entitlements: vec![],
            archive: ArchiveInfo {
                digest: format!("sha256:{}", "ab".repeat(32)),
                size: 10,
                installed_size: 5,
                sources: vec!["https://example.invalid/a".into()],
            },
        }
    }

    fn descriptor_json(extra: &str) -> String {
        format!(
            r#"{{"name":"m","backend":"onnx-runtime","enabled":true,
                 "file_path":"onnx/model.onnx",
                 "params":{{"model_type":"embed","target_dim":384{extra}}}}}"#
        )
    }

    #[test]
    fn a_complete_descriptor_parses_and_agrees_with_its_index_entry() {
        let descriptor = Descriptor::parse(&descriptor_json("")).unwrap();
        assert_eq!(descriptor.name, "m");
        assert_eq!(descriptor.required_assets.len(), 1);
        assert_eq!(descriptor.required_assets[0].path, "onnx/model.onnx");
        descriptor
            .validate_against_index(&index_model("m"))
            .unwrap();
    }

    #[test]
    fn tokenizer_file_references_become_required_assets() {
        let body = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
            "file_path":"onnx/model.onnx",
            "executor":{"params":{"tokenizer":{"tokenizer_type":"huggingface-bpe",
                "params":{"vocab_file_path":"tok/vocab.json",
                          "merges_file_path":"tok/merges.txt",
                          "pretrained_vocab_file":""}}}},
            "params":{"model_type":"embed"}}"#;
        let descriptor = Descriptor::parse(body).unwrap();
        let paths: Vec<&str> = descriptor
            .required_assets
            .iter()
            .map(|a| a.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["onnx/model.onnx", "tok/vocab.json", "tok/merges.txt"]
        );
    }

    /// The engine also consumes nested tokenizer configurations
    /// (`executor.params.transformer.tokenizer` and encoder/decoder
    /// shapes), so harvesting must find every `tokenizer`-keyed object,
    /// wherever it nests, for both the admission gate and the
    /// required-asset set.
    #[test]
    fn nested_tokenizer_configurations_are_harvested() {
        let body = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
            "file_path":"onnx/model.onnx",
            "executor":{"params":{
                "transformer":{"tokenizer":{"tokenizer_type":"huggingface-bpe",
                    "params":{"vocab_file_path":"tok/vocab.json",
                              "merges_file_path":"tok/merges.txt"}}},
                "encoder":{"tokenizer":{"tokenizer_type":"huggingface-wordpiece",
                    "params":{"vocab_file_path":"enc/vocab.txt"}}},
                "decoder":{"tokenizer":{"tokenizer_type":"huggingface-unigram",
                    "params":{"vocab_file_path":"dec/unigram.json"}}}}},
            "params":{"model_type":"embed","target_dim":384}}"#;
        let descriptor = Descriptor::parse(body).unwrap();

        let mut types = descriptor.tokenizer_types.clone();
        types.sort();
        assert_eq!(
            types,
            [
                "huggingface-bpe",
                "huggingface-unigram",
                "huggingface-wordpiece"
            ],
            "every nested tokenizer_type must be visible to the admission gate"
        );
        let paths: std::collections::BTreeSet<&str> = descriptor
            .required_assets
            .iter()
            .map(|a| a.path.as_str())
            .collect();
        for expected in [
            "tok/vocab.json",
            "tok/merges.txt",
            "enc/vocab.txt",
            "dec/unigram.json",
        ] {
            assert!(
                paths.contains(expected),
                "missing required asset {expected}"
            );
        }
    }

    /// Negative control: similarly named keys are not tokenizer
    /// configurations and must not be harvested.
    #[test]
    fn similarly_named_keys_are_not_tokenizers() {
        let body = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
            "executor":{"params":{
                "detokenizer":{"tokenizer_type":"huggingface-unigram",
                    "params":{"vocab_file_path":"x/y.json"}},
                "tokenizer_config":{"tokenizer_type":"huggingface-unigram"},
                "notes":{"tokenizer":"a plain string, not a configuration"}}},
            "params":{"model_type":"embed","target_dim":384}}"#;
        let descriptor = Descriptor::parse(body).unwrap();
        assert!(
            descriptor.tokenizer_types.is_empty(),
            "{:?}",
            descriptor.tokenizer_types
        );
        assert!(descriptor.required_assets.is_empty());
    }

    #[test]
    fn params_level_tokenizer_references_are_also_collected() {
        let body = r#"{"name":"m","enabled":true,
            "params":{"model_type":"embed",
                "tokenizer":{"params":{"pretrained_vocab_file":"tokenizer.json"}}}}"#;
        let descriptor = Descriptor::parse(body).unwrap();
        assert_eq!(descriptor.required_assets.len(), 1);
        assert_eq!(descriptor.required_assets[0].path, "tokenizer.json");
    }

    /// A dependency-list disagreement is advertising drift, not an
    /// install error. The shipped descriptor is authoritative at load time.
    #[test]
    fn dependency_disagreement_is_a_warning() {
        let body = r#"{"name":"m","backend":"onnx-runtime","enabled":true,
            "dependencies":["other"],
            "params":{"model_type":"embed","target_dim":384}}"#;
        let descriptor = Descriptor::parse(body).unwrap();
        let warnings = descriptor
            .validate_against_index(&index_model("m"))
            .unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("dependencies")),
            "{warnings:?}"
        );
    }

    #[test]
    fn missing_backend_or_model_type_is_refused() {
        let no_backend =
            Descriptor::parse(r#"{"name":"m","enabled":true,"params":{"model_type":"embed"}}"#)
                .unwrap();
        assert!(no_backend
            .validate_against_index(&index_model("m"))
            .unwrap_err()
            .contains("backend"));

        let no_type =
            Descriptor::parse(r#"{"name":"m","backend":"onnx-runtime","enabled":true}"#).unwrap();
        assert!(no_type
            .validate_against_index(&index_model("m"))
            .unwrap_err()
            .contains("model_type"));
    }

    /// Every immutable-identity field is a refusal, not advertising
    /// the engine consumes the descriptor, so an
    /// archive declaring another space or shape than the catalogue
    /// advertised is a different model than the one that was asked for.
    #[test]
    fn identity_disagreement_with_the_index_is_refused() {
        let wrong_type = Descriptor::parse(
            r#"{"name":"m","backend":"onnx-runtime","enabled":true,"params":{"model_type":"convert"}}"#,
        )
        .unwrap();
        assert!(wrong_type
            .validate_against_index(&index_model("m"))
            .unwrap_err()
            .contains("model_type"));

        let wrong_dim = Descriptor::parse(&descriptor_json("").replace("384", "512")).unwrap();
        let err = wrong_dim
            .validate_against_index(&index_model("m"))
            .unwrap_err();
        assert!(err.contains("target_dim"), "{err}");
        assert!(err.contains("different shape"), "{err}");

        let mut index = index_model("m");
        index.source_model = Some("ghost-source".into());
        let err = Descriptor::parse(&descriptor_json(""))
            .unwrap()
            .validate_against_index(&index)
            .unwrap_err();
        assert!(err.contains("source_model"), "{err}");
        assert!(err.contains("different vector space"), "{err}");
    }

    #[test]
    fn disabled_descriptors_are_refused() {
        let disabled = Descriptor::parse(
            r#"{"name":"m","backend":"onnx-runtime","enabled":false,"params":{"model_type":"embed"}}"#,
        )
        .unwrap();
        assert!(disabled
            .validate_against_index(&index_model("m"))
            .unwrap_err()
            .contains("disabled"));
    }

    /// Presence on one side only is still drift in the advertising
    /// fields: reported, never a refusal. Identity fields are covered by
    /// `identity_disagreement_with_the_index_is_refused` instead.
    #[test]
    fn one_sided_metadata_omission_warns() {
        let descriptor = Descriptor::parse(&descriptor_json("")).unwrap();

        // Index invents a sequence_len.
        let mut index = index_model("m");
        index.sequence_len = Some(512);
        let warnings = descriptor.validate_against_index(&index).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("sequence_len")),
            "{warnings:?}"
        );
    }

    fn convert_descriptor() -> Descriptor {
        Descriptor::parse(
            r#"{"name":"c","backend":"onnx-runtime","enabled":true,
                "file_path":"onnx/model.onnx",
                "params":{"model_type":"convert","source_model":"embed-a",
                          "target_model":"space-b","source_dim":384,"target_dim":1536}}"#,
        )
        .unwrap()
    }

    fn convert_index_model() -> IndexModel {
        let mut model = index_model("c");
        model.model_type = "convert".into();
        model.source_model = Some("embed-a".into());
        model.target_model = Some("space-b".into());
        model.source_dim = Some(384);
        model.target_dim = Some(1536);
        model.postvec_requires = vec!["embed-a".into(), "embed-bridge".into()];
        model
    }

    /// The installer independently derives the companion closure and
    /// requires the index to advertise exactly that set.
    #[test]
    fn postvec_requires_must_match_the_derived_closure() {
        let descriptor = convert_descriptor();
        assert_eq!(
            descriptor.derived_postvec_requires(),
            ["embed-a", "embed-bridge"]
        );
        descriptor
            .validate_against_index(&convert_index_model())
            .unwrap();

        // An index that omits the companions is drift, reported as a
        // warning. Pull's closure came from the validated index either way.
        let mut bare = convert_index_model();
        bare.postvec_requires.clear();
        let warnings = descriptor.validate_against_index(&bare).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("postvec_requires")),
            "{warnings:?}"
        );

        // An index that invents companions for a non-converter, likewise.
        let embed = Descriptor::parse(&descriptor_json("")).unwrap();
        let mut index = index_model("m");
        index.postvec_requires = vec!["embed-bridge".into()];
        let warnings = embed.validate_against_index(&index).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("postvec_requires")),
            "{warnings:?}"
        );
    }

    /// Per-model-type structural rules stay a hard gate on the publisher
    /// path (`check_model_type_shape`); at install time they only warn.
    #[test]
    fn model_type_shapes_are_enforced_at_publication() {
        // A converter without source_dim cannot dimension its source space.
        let incomplete = Descriptor::parse(
            r#"{"name":"c","backend":"onnx-runtime","enabled":true,
                "params":{"model_type":"convert","source_model":"embed-a",
                          "target_model":"space-b","target_dim":1536}}"#,
        )
        .unwrap();
        let err = incomplete.check_model_type_shape().unwrap_err();
        assert!(err.contains("source_dim"), "{err}");
        let mut index = convert_index_model();
        index.source_dim = None;
        let warnings = incomplete.validate_against_index(&index).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("source_dim")),
            "{warnings:?}"
        );

        // An embed model without target_dim cannot size a shadow column.
        let dimless = Descriptor::parse(
            r#"{"name":"m","backend":"onnx-runtime","enabled":true,
                "params":{"model_type":"embed"}}"#,
        )
        .unwrap();
        let err = dimless.check_model_type_shape().unwrap_err();
        assert!(err.contains("target_dim"), "{err}");
    }

    #[test]
    fn oversized_dimensions_are_a_checked_error_not_a_truncation() {
        let err =
            Descriptor::parse(r#"{"name":"m","enabled":true,"params":{"target_dim":4294967296}}"#)
                .unwrap_err();
        assert!(err.contains("does not fit u32"), "{err}");
        let err = Descriptor::parse(r#"{"name":"m","enabled":true,"params":{"target_dim":-1}}"#)
            .unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
    }
}
