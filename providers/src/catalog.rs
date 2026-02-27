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
    // embed-v4.0 takes 128k tokens per input, not the 8192 of v3 — the
    // figure a reader will quote back from the docs table, and the one the
    // descriptor advertises as `sequence_len`.
    CatalogModel {
        provider: "cohere",
        id: "embed-v4.0",
        name: "cohere-embed-v4-0",
        dim: 1536,
        max_tokens: 128_000,
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
    // AWS Bedrock. The `aws` connector speaks the Amazon Titan embedding
    // schema only (`titan.rs`): `inputText` in, `embedding` out. Models on
    // Bedrock with a different body shape — Cohere's, for one — need their
    // own codec, so they are deliberately absent here rather than offered
    // as a descriptor that can never serve. Titan invokes one text per
    // request, so `max_batch` 1 is the API shape, not a policy.
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
        id: "amazon.titan-embed-text-v1",
        name: "aws-titan-embed-text-v1",
        dim: 1536,
        max_tokens: 8192,
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

/// The prefix the documented public names carry for a connector type.
///
/// Derived from the *canonical* type, never from what the operator typed:
/// `gemini` and `google` name one connector, and if the prefix followed the
/// spelling they would produce two different SQL model names for the same
/// model — the alias would stop being an alias at exactly the point it
/// matters. `google` is spelled `gemini` here because that is the prefix the
/// docs and the catalog use.
pub fn name_prefix(typed: &str) -> String {
    match canonical_provider(typed).as_str() {
        "google" => "gemini".to_string(),
        other => other.to_string(),
    }
}

/// The public-name convention: curated catalog name where one exists, else
/// the mechanical rule — lowercase `{prefix}-{provider_model_id}` reduced to
/// the charset the hosts accept for a model name (`[a-z0-9._-]`, starting
/// `[a-z0-9]`).
///
/// The reduction is deliberately total rather than a fixed substitution
/// list: a name the hosts refuse is a providers.d file that fails to load
/// *as a whole*, so a model id with an unexpected character in it would take
/// a whole connector down at the next reload instead of being renamed.
pub fn public_name(typed: &str, id: &str) -> String {
    if let Some(model) = lookup(typed, id) {
        return model.name.to_string();
    }
    sanitize_public_name(&format!("{}-{}", name_prefix(typed), id))
}

/// Lowercase, map every character outside `[a-z0-9._-]` to `-`, collapse
/// runs of `-`, then trim the separators off both ends. What survives is
/// either empty or starts with `[a-z0-9]`, because those are the only other
/// characters the loop can emit.
fn sanitize_public_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.to_lowercase().chars() {
        let mapped = match ch {
            'a'..='z' | '0'..='9' | '.' | '_' => ch,
            _ => '-',
        };
        if mapped == '-' && out.ends_with('-') {
            continue;
        }
        out.push(mapped);
    }
    out.trim_matches(|c| c == '-' || c == '.' || c == '_')
        .to_string()
}

/// The rule the hosts enforce on a served model name
/// (`config::validate_model_name`). `public_name` already produces a
/// conforming name; this is what tells the CLI that an id reduced to
/// *nothing* usable.
pub fn validate_public_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the derived public name is empty".to_string());
    }
    if name.len() > 128 {
        return Err(format!(
            "the derived public name {name:?} exceeds 128 bytes"
        ));
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!("public name {name:?} must start with [a-z0-9]"));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "public name {name:?} contains characters outside [a-z0-9._-]"
        ));
    }
    Ok(())
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

    /// The `aws` connector implements the Amazon Titan request and response
    /// schema and nothing else (`titan.rs`). A catalog row for any other
    /// Bedrock model would prefill a descriptor the host can never serve —
    /// silently so under `provider add --no-verify`. Add the codec first,
    /// then the row.
    #[test]
    fn the_aws_catalog_holds_only_models_the_titan_codec_serves() {
        for model in CATALOG.iter().filter(|m| m.provider == "aws") {
            assert!(
                model.id.starts_with("amazon.titan-embed"),
                "{} is not a Titan embedding model",
                model.id
            );
        }
    }

    #[test]
    fn unknown_ids_fall_back_to_the_mechanical_rule() {
        assert_eq!(
            public_name("openrouter", "openai/text-embedding-3-large"),
            "openrouter-openai-text-embedding-3-large"
        );
        assert_eq!(
            public_name("openai", "Future Model.v2:1"),
            "openai-future-model.v2-1"
        );
        assert!(lookup("openrouter", "anything").is_none());
    }

    /// An alias must not change the name a model gets in SQL. Before this,
    /// `provider add google` and `provider add gemini` produced two
    /// different public names for one model.
    #[test]
    fn aliases_derive_one_public_name() {
        assert_eq!(
            public_name("google", "text-embedding-004"),
            public_name("gemini", "text-embedding-004")
        );
        assert_eq!(
            public_name("google", "text-embedding-004"),
            "gemini-text-embedding-004"
        );
        assert_eq!(
            public_name("amazon", "some.future-model"),
            public_name("aws", "some.future-model")
        );
    }

    /// A derived name the hosts would refuse takes the whole connector file
    /// down at load, so the derivation reduces to the accepted charset
    /// instead of hoping ids stay tidy.
    #[test]
    fn derived_names_always_satisfy_the_host_rule() {
        for id in [
            "weird@id+v2",
            "  spaced  out  ",
            "///leading",
            "Ünïcode-model",
            "UPPER::CASE",
        ] {
            let name = public_name("openai", id);
            assert!(
                validate_public_name(&name).is_ok(),
                "{id:?} -> {name:?}: {:?}",
                validate_public_name(&name)
            );
        }
        for model in CATALOG {
            assert!(validate_public_name(model.name).is_ok(), "{}", model.name);
        }
        // An id that reduces to nothing usable is reported, not written.
        assert!(validate_public_name(&sanitize_public_name("---")).is_err());
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
