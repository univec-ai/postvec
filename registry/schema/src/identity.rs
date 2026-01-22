//! What a model name promises across revisions.
//!
//! A name identifies **one immutable model identity and a monotonic mutable
//! head**. This module owns the identity half: the six fields that may never
//! change under a name, and the exact comparison of them.
//!
//! The comparison happens three times, deliberately:
//!
//! 1. the publisher checks the live index entry against the descriptor it is
//!    about to publish;
//! 2. the client's preflight checks its install receipt against the index
//!    entry, so an obviously wrong upgrade is refused before any download;
//! 3. the client checks its install receipt against the **staged descriptor**
//!    — the bytes it actually extracted — before anything is mutated. That
//!    third check is the one that matters: the engine consumes the
//!    descriptor, not the index, so a registry serving an archive that
//!    disagrees with its own catalogue must not be able to move an installed
//!    column's vector space.

/// The identity a revision must preserve, as either side of the check holds
/// it: the receipt fills it from disk, an index entry or a staged descriptor
/// from what it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub model_type: String,
    pub backend: String,
    pub source_model: Option<String>,
    pub target_model: Option<String>,
    pub source_dim: Option<u32>,
    pub target_dim: Option<u32>,
}

impl Identity {
    pub fn of_index(model: &crate::IndexModel) -> Identity {
        Identity {
            model_type: model.model_type.clone(),
            backend: model.backend.clone(),
            source_model: model.source_model.clone(),
            target_model: model.target_model.clone(),
            source_dim: model.source_dim,
            target_dim: model.target_dim,
        }
    }
}

/// Compare the two fields every writer has always recorded, so they can be
/// checked even against a receipt written before the identity block existed.
pub fn check_identity_core(
    installed_model_type: &str,
    installed_backend: &str,
    next: &Identity,
) -> Result<(), String> {
    if installed_model_type != next.model_type {
        return Err(format!(
            "model_type would change from {installed_model_type:?} to {:?}; that is a different \
             model, not a new revision — publish a successor name",
            next.model_type
        ));
    }
    if installed_backend != next.backend {
        return Err(format!(
            "backend would change from {installed_backend:?} to {:?}; the backend is the install \
             directory, and two copies of one logical model is a silent wrong-model bug — \
             publish a successor name",
            next.backend
        ));
    }
    Ok(())
}

/// Compare two identities **exactly**, naming the first field that differs.
///
/// Every field is compared including its absence: `None → Some` is a change
/// like any other, because a converter that starts declaring a target space
/// is not the model that did not declare one. Callers that genuinely do not
/// know the installed side (a receipt older than the identity block) must say
/// so by calling [`check_identity_core`] instead — "unknown" is never spelled
/// as `None` here.
pub fn check_identity(installed: &Identity, next: &Identity) -> Result<(), String> {
    check_identity_core(&installed.model_type, &installed.backend, next)?;
    for (label, before, after) in [
        ("source_model", &installed.source_model, &next.source_model),
        ("target_model", &installed.target_model, &next.target_model),
    ] {
        if before != after {
            return Err(format!(
                "{label} would change from {before:?} to {after:?}; that is the vector space \
                 itself — publish a successor name"
            ));
        }
    }
    for (label, before, after) in [
        ("source_dim", installed.source_dim, next.source_dim),
        ("target_dim", installed.target_dim, next.target_dim),
    ] {
        if before != after {
            return Err(format!(
                "{label} would change from {before:?} to {after:?}; every stored vector and \
                 every index is sized from it — publish a successor name"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Identity {
        Identity {
            model_type: "convert".into(),
            backend: "onnx-runtime".into(),
            source_model: Some("a".into()),
            target_model: Some("b".into()),
            source_dim: Some(768),
            target_dim: Some(1536),
        }
    }

    #[test]
    fn identity_names_the_field_that_would_change() {
        assert!(check_identity(&base(), &base()).is_ok());

        for (mutate, expected) in [
            (
                Box::new(|i: &mut Identity| i.model_type = "embed".into()) as Box<dyn Fn(&mut _)>,
                "model_type",
            ),
            (
                Box::new(|i: &mut Identity| i.backend = "candle".into()),
                "backend",
            ),
            (
                Box::new(|i: &mut Identity| i.target_model = Some("c".into())),
                "target_model",
            ),
            (
                Box::new(|i: &mut Identity| i.target_dim = Some(1024)),
                "target_dim",
            ),
        ] {
            let mut next = base();
            mutate(&mut next);
            let err = check_identity(&base(), &next).unwrap_err();
            assert!(err.contains(expected), "{err}");
        }
    }

    /// Absence is a value, not a wildcard.
    #[test]
    fn a_none_to_some_transition_is_a_change() {
        let mut old = base();
        old.source_model = None;
        old.source_dim = None;
        let err = check_identity(&old, &base()).unwrap_err();
        assert!(err.contains("source_model"), "{err}");
    }

    /// The core check works against a receipt too old to hold the block.
    #[test]
    fn the_core_check_needs_only_the_always_recorded_fields() {
        assert!(check_identity_core("convert", "onnx-runtime", &base()).is_ok());
        assert!(check_identity_core("embed", "onnx-runtime", &base()).is_err());
        assert!(check_identity_core("convert", "candle", &base()).is_err());
    }
}
