//! The built-in model catalog: well-known provider model ids with their
//! dimensions and limits, plus the public-name convention.
//!
//! Used only by the `postvec provider` CLI to prefill descriptors (and by
//! docs generation) — the serving hosts never consult it: the providers.d
//! file on disk is the complete serving truth. Unknown ids fall back to the
//! CLI's verification probe, so this list stays deliberately small; the file
//! format makes an exhaustive registry unnecessary.

/// One well-known provider model.
#[derive(Debug, Clone, Copy)]
pub struct CatalogModel {
    /// Canonical connector type: `openai | google | cohere | aws | mistral`.
    pub provider: &'static str,
    /// The identifier the provider's API expects.
    pub id: &'static str,
    /// The curated public name (the docs' spelling).
    pub name: &'static str,
    pub dim: u32,
    pub max_tokens: u32,
    pub max_batch: usize,
}

pub const CATALOG: &[CatalogModel] = &[
    // OpenAI
    CatalogModel {
        provider: "openai",
        id: "text-embedding-3-small",
        name: "openai-text-embedding-3-small",
        dim: 1536,
        max_tokens: 8191,
        max_batch: 512,
    },
    CatalogModel {
        provider: "openai",
        id: "text-embedding-3-large",
        name: "openai-text-embedding-3-large",
        dim: 3072,
        max_tokens: 8191,
        max_batch: 512,
    },
    CatalogModel {
        provider: "openai",
        id: "text-embedding-ada-002",
        name: "openai-text-embedding-ada-002",
        dim: 1536,
        max_tokens: 8191,
        max_batch: 512,
    },
    // Gemini
    CatalogModel {
        provider: "google",
        id: "gemini-embedding-001",
        name: "gemini-embedding-001",
        dim: 3072,
        max_tokens: 2048,
        max_batch: 96,
    },
    // Cohere
    CatalogModel {
        provider: "cohere",
        id: "embed-v4.0",
        name: "cohere-embed-v4-0",
        dim: 1536,
        max_tokens: 8192,
        max_batch: 96,
    },
    CatalogModel {
        provider: "cohere",
        id: "embed-english-v3.0",
        name: "cohere-embed-english-v3-0",
        dim: 1024,
        max_tokens: 512,
        max_batch: 96,
    },
    CatalogModel {
        provider: "cohere",
        id: "embed-multilingual-v3.0",
        name: "cohere-embed-multilingual-v3-0",
        dim: 1024,
        max_tokens: 512,
        max_batch: 96,
    },
    // AWS Bedrock. Titan invokes one text per request (`max_batch` 1 is the
    // API shape, not a policy).
    CatalogModel {
        provider: "aws",
        id: "amazon.titan-embed-text-v2:0",
        name: "aws-titan-embed-text-v2-0",
        dim: 1024,
        max_tokens: 8192,
        max_batch: 1,
    },
    CatalogModel {
        provider: "aws",
        id: "cohere.embed-english-v3",
        name: "aws-cohere-embed-english-v3",
        dim: 1024,
        max_tokens: 512,
        max_batch: 1,
    },
    // Mistral
    CatalogModel {
        provider: "mistral",
        id: "mistral-embed",
        name: "mistral-mistral-embed",
        dim: 1024,
        max_tokens: 8192,
        max_batch: 96,
    },
];

/// Canonical connector type for a CLI-typed provider (folds the aliases).
pub fn canonical_provider(typed: &str) -> String {
    match typed.to_lowercase().as_str() {
        "gemini" => "google".to_string(),
        "amazon" => "aws".to_string(),
        other => other.to_string(),
    }
}

/// Look up a well-known model by canonical provider type and provider id.
pub fn lookup(provider: &str, id: &str) -> Option<&'static CatalogModel> {
    let provider = canonical_provider(provider);
    CATALOG
        .iter()
        .find(|model| model.provider == provider && model.id == id)
}

/// The public-name convention: curated catalog name where one exists, else
/// the mechanical rule — lowercase `{typed-provider}-{provider_model_id}`
/// with `/`, `:`, `.` and spaces mapped to `-` (the same derivation the
/// established provider spaces use). `typed` keeps the CLI's spelling
/// (`gemini`, not `google`) because that is the prefix the documented names
/// carry.
pub fn public_name(typed: &str, id: &str) -> String {
    if let Some(model) = lookup(typed, id) {
        return model.name.to_string();
    }
    format!("{typed}-{id}")
        .to_lowercase()
        .replace(['/', ':', '.', ' '], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_names_match_the_documented_convention() {
        for model in CATALOG {
            assert!(model.dim > 0, "{}", model.name);
            assert!(model.max_batch > 0, "{}", model.name);
            // Public names use the flat hyphenated charset the hosts accept.
            assert!(
                model
                    .name
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')),
                "{}",
                model.name
            );
        }
        // The doc-blessed spellings.
        assert_eq!(
            public_name("openai", "text-embedding-3-small"),
            "openai-text-embedding-3-small"
        );
        assert_eq!(public_name("cohere", "embed-v4.0"), "cohere-embed-v4-0");
        assert_eq!(
            public_name("aws", "amazon.titan-embed-text-v2:0"),
            "aws-titan-embed-text-v2-0"
        );
        assert_eq!(
            public_name("mistral", "mistral-embed"),
            "mistral-mistral-embed"
        );
        assert_eq!(
            public_name("gemini", "gemini-embedding-001"),
            "gemini-embedding-001"
        );
    }

    #[test]
    fn unknown_ids_fall_back_to_the_mechanical_rule() {
        assert_eq!(
            public_name("openrouter", "openai/text-embedding-3-large"),
            "openrouter-openai-text-embedding-3-large"
        );
        assert_eq!(
            public_name("openai", "Future Model.v2:1"),
            "openai-future-model-v2-1"
        );
        assert!(lookup("openrouter", "anything").is_none());
    }

    #[test]
    fn aliases_fold_to_canonical_types() {
        assert_eq!(canonical_provider("gemini"), "google");
        assert_eq!(canonical_provider("GEMINI"), "google");
        assert_eq!(canonical_provider("amazon"), "aws");
        assert_eq!(canonical_provider("openai"), "openai");
        assert!(lookup("gemini", "gemini-embedding-001").is_some());
    }
}
