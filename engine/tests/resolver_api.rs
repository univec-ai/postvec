//! Public-API tests for `engine::resolver::ModelResolver`, complementing the
//! in-module unit tests. Focus: disabled-model skipping, deterministic
//! last-wins on duplicate targets, and rebuild clearing stale indexes.

use engine::resolver::ModelResolver;
use engine::ModelConfiguration;
use serde_json::json;

fn cfg(name: &str, enabled: bool, params: serde_json::Value) -> ModelConfiguration {
    serde_json::from_value(json!({
        "name": name,
        "enabled": enabled,
        "params": params,
    }))
    .unwrap()
}

fn embed(name: &str, target: Option<&str>, enabled: bool) -> ModelConfiguration {
    let params = match target {
        Some(t) => json!({"model_type": "embed", "target_model": t}),
        None => json!({"model_type": "embed"}),
    };
    cfg(name, enabled, params)
}

fn convert(name: &str, src: &str, tgt: &str, enabled: bool) -> ModelConfiguration {
    cfg(
        name,
        enabled,
        json!({"model_type": "convert", "source_model": src, "target_model": tgt}),
    )
}

#[test]
fn disabled_models_are_not_indexed() {
    let models = [embed("disabled-embed", Some("pub-a"), false)];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    assert!(r.resolve_embed("pub-a").is_none());
}

#[test]
fn embed_without_target_uses_internal_name() {
    let models = [embed("internal-name", None, true)];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    let got = r.resolve_embed("internal-name").unwrap();
    assert_eq!(got.internal_name, "internal-name");
}

#[test]
fn duplicate_target_last_alphabetical_wins() {
    // Two embed models claim the same public target. rebuild sorts by name and
    // inserts in order, so the alphabetically-last name wins deterministically.
    let models = [
        embed("aaa-model", Some("shared-target"), true),
        embed("zzz-model", Some("shared-target"), true),
    ];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    assert_eq!(
        r.resolve_embed("shared-target").unwrap().internal_name,
        "zzz-model"
    );
}

#[test]
fn convert_pair_is_directional() {
    let models = [convert("conv", "openai", "gemini", true)];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    assert!(r.resolve_convert("openai", "gemini").is_some());
    // Reverse direction is a distinct, unindexed pair.
    assert!(r.resolve_convert("gemini", "openai").is_none());
}

#[test]
fn rebuild_clears_previous_indexes() {
    let r = ModelResolver::new();
    r.rebuild([embed("m1", Some("target-1"), true)].iter());
    assert!(r.resolve_embed("target-1").is_some());

    // Rebuild with a different set — the old entry must be gone.
    r.rebuild([embed("m2", Some("target-2"), true)].iter());
    assert!(r.resolve_embed("target-1").is_none());
    assert!(r.resolve_embed("target-2").is_some());
}

#[test]
fn implicit_convert_without_model_type() {
    // No model_type, but both source_model and target_model present → treated
    // as a convert model (fallback branch).
    let models = [cfg(
        "implicit",
        true,
        json!({"source_model": "a", "target_model": "b"}),
    )];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    assert_eq!(
        r.resolve_convert("a", "b").unwrap().internal_name,
        "implicit"
    );
}

#[test]
fn mixed_set_indexes_both_kinds() {
    let models = [
        embed("e1", Some("emb-target"), true),
        convert("c1", "s", "t", true),
        embed("disabled", Some("nope"), false),
    ];
    let r = ModelResolver::new();
    r.rebuild(models.iter());
    assert!(r.resolve_embed("emb-target").is_some());
    assert!(r.resolve_convert("s", "t").is_some());
    assert!(r.resolve_embed("nope").is_none());
}
