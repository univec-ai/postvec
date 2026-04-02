//! Public model names to internal names. Rebuilt on load and unload.

use crate::models::ModelConfiguration;
use dashmap::DashMap;

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub internal_name: String,
}

pub struct ModelResolver {
    /// Embed: `target_model` -> model.
    embed_index: DashMap<String, ResolvedModel>,
    /// Convert: `(source_model, target_model)` -> model.
    convert_index: DashMap<(String, String), ResolvedModel>,
}

impl Default for ModelResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelResolver {
    pub fn new() -> Self {
        Self {
            embed_index: DashMap::new(),
            convert_index: DashMap::new(),
        }
    }

    /// Rebuild indexes. Disabled models are skipped. Duplicate targets: last
    /// name alphabetically wins.
    pub fn rebuild<'a, I>(&self, models: I)
    where
        I: Iterator<Item = &'a ModelConfiguration>,
    {
        self.embed_index.clear();
        self.convert_index.clear();

        let mut model_configs: Vec<&ModelConfiguration> = models.collect();

        model_configs.sort_by(|a, b| a.name.cmp(&b.name));

        for config in model_configs {
            let internal_name = &config.name;

            if !config.enabled {
                continue;
            }

            let model_type = config.params.get("model_type").and_then(|v| v.as_str());

            let source_model = config.params.get("source_model").and_then(|v| v.as_str());

            let target_model = config.params.get("target_model").and_then(|v| v.as_str());

            let resolved = ResolvedModel {
                internal_name: internal_name.clone(),
            };

            match (model_type, source_model, target_model) {
                (Some("convert"), Some(src), Some(tgt)) => {
                    log::debug!(
                        "Resolver: indexing convert model '{}' <- ({}, {})",
                        internal_name,
                        src,
                        tgt
                    );
                    self.convert_index
                        .insert((src.to_string(), tgt.to_string()), resolved);
                }
                (Some("embed"), _, _) => {
                    let tgt = target_model.unwrap_or(internal_name.as_str());
                    log::debug!(
                        "Resolver: indexing embed model '{}' <- '{}'",
                        internal_name,
                        tgt
                    );
                    self.embed_index.insert(tgt.to_string(), resolved);
                }
                _ => {
                    // No model_type: source+target is treated as convert.
                    if let (Some(src), Some(tgt)) = (source_model, target_model) {
                        self.convert_index
                            .insert((src.to_string(), tgt.to_string()), resolved.clone());
                    } else if let Some(tgt) = target_model {
                        if let Some("embed") = model_type {
                            self.embed_index.insert(tgt.to_string(), resolved);
                        }
                    }
                }
            }
        }

        log::info!(
            "ModelResolver rebuilt: {} embed, {} convert models indexed",
            self.embed_index.len(),
            self.convert_index.len(),
        );
    }

    pub fn resolve_embed(&self, target_model: &str) -> Option<ResolvedModel> {
        self.embed_index.get(target_model).map(|r| r.clone())
    }

    pub fn resolve_convert(&self, source_model: &str, target_model: &str) -> Option<ResolvedModel> {
        let key = (source_model.to_string(), target_model.to_string());
        self.convert_index.get(&key).map(|r| r.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_config(name: &str, params: &[(&str, &str)]) -> ModelConfiguration {
        let mut param_map = HashMap::new();
        for (k, v) in params {
            param_map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
        }
        ModelConfiguration {
            name: name.to_string(),
            enabled: true,
            params: param_map,
            ..Default::default()
        }
    }

    #[test]
    fn test_resolver_indexing() {
        let models = [
            make_config(
                "embed-model-1",
                &[
                    ("model_type", "embed"),
                    ("target_model", "public-embed-model"),
                ],
            ),
            make_config(
                "convert-model-1",
                &[
                    ("model_type", "convert"),
                    ("source_model", "src-fmt"),
                    ("target_model", "tgt-fmt"),
                ],
            ),
            // No model_type, both source and target present.
            make_config(
                "convert-model-2",
                &[("source_model", "src-fmt-2"), ("target_model", "tgt-fmt-2")],
            ),
        ];

        let resolver = ModelResolver::new();
        resolver.rebuild(models.iter());

        let resolved_embed = resolver.resolve_embed("public-embed-model");
        assert!(resolved_embed.is_some());
        assert_eq!(resolved_embed.unwrap().internal_name, "embed-model-1");

        let resolved_convert = resolver.resolve_convert("src-fmt", "tgt-fmt");
        assert!(resolved_convert.is_some());
        assert_eq!(resolved_convert.unwrap().internal_name, "convert-model-1");

        let resolved_convert_2 = resolver.resolve_convert("src-fmt-2", "tgt-fmt-2");
        assert!(resolved_convert_2.is_some());
        assert_eq!(resolved_convert_2.unwrap().internal_name, "convert-model-2");

        assert!(resolver.resolve_embed("missing").is_none());
        assert!(resolver.resolve_convert("foo", "bar").is_none());
    }

    #[test]
    fn test_embed_model_without_target_model_uses_name() {
        // No target_model: index under the internal name.
        let models = [make_config(
            "Alibaba-NLP.gte-large-en-v1.5",
            &[("model_type", "embed")],
        )];

        let resolver = ModelResolver::new();
        resolver.rebuild(models.iter());

        let resolved = resolver.resolve_embed("Alibaba-NLP.gte-large-en-v1.5");
        assert!(resolved.is_some());
        assert_eq!(
            resolved.unwrap().internal_name,
            "Alibaba-NLP.gte-large-en-v1.5"
        );
    }

    #[test]
    fn test_embed_model_with_target_model_takes_priority() {
        // Explicit target_model wins over the internal name.
        let models = [make_config(
            "Alibaba-NLP.gte-base-en-v1.5",
            &[("model_type", "embed"), ("target_model", "custom-alias")],
        )];

        let resolver = ModelResolver::new();
        resolver.rebuild(models.iter());

        let by_alias = resolver.resolve_embed("custom-alias");
        assert!(by_alias.is_some());
        assert_eq!(
            by_alias.unwrap().internal_name,
            "Alibaba-NLP.gte-base-en-v1.5"
        );

        let by_name = resolver.resolve_embed("Alibaba-NLP.gte-base-en-v1.5");
        assert!(by_name.is_none());
    }
}
