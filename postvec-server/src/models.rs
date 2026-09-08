// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! On-disk model store: `<root>/models/<backend>/<name>/ninference.hub.json`.
//! Nothing here fetches. A model that is not on disk is a refusal.
//!
//! A malformed descriptor is a warning; the rest of the root still loads.
//! A duplicated enabled name is excluded with a warning. Naming it in
//! `--models` is fatal. A disabled descriptor never loads on any path.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Longest accepted model (directory) name, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// Depth ceiling for a dependency chain; far above any real bridge.
const MAX_DEPENDENCY_DEPTH: usize = 32;

pub const DESCRIPTOR_FILENAME: &str = "ninference.hub.json";
pub const MODELS_DIR: &str = "models";

/// A deliberately minimal descriptor view: only the admission facts. The
/// engine re-parses the full schema in `load_model` and stays authoritative
/// for everything else, so widening this struct would create a second
/// definition of the same file.
#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// One model directory found under the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskModel {
    pub name: String,
    pub enabled: bool,
    pub backend: String,
    pub path: PathBuf,
    /// The same name exists under more than one backend.
    pub ambiguous: bool,
}

/// Everything a scan found, plus everything it could not make sense of.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub models: Vec<DiskModel>,
    pub warnings: Vec<String>,
}

impl Inventory {
    /// Enabled, unambiguous names — the set this node will try to load.
    pub fn loadable(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.enabled && !m.ambiguous)
            .map(|m| m.name.clone())
            .collect()
    }

    /// Every enabled name, ambiguity included. `status` reports on these so a
    /// duplicate shows up as a fact rather than as a silent absence.
    pub fn enabled(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.name.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&DiskModel> {
        self.models.iter().find(|m| m.name == name)
    }
}

/// Model name → its descriptor path(s). More than one path means the name
/// exists under more than one backend.
pub type DescriptorIndex = BTreeMap<String, Vec<PathBuf>>;

/// Names this node's engine owns: currently loaded, plus every on-disk
/// descriptor.
///
/// Wider than `get_active_models()`. A configured model that failed to load
/// is missing from the engine map. Reserving only loaded names would let a
/// provider file take that name and send source text off-box.
///
/// A failed scan is an error, not a smaller set. The caller keeps the
/// previous gateway snapshot on reload, or serves no providers at boot.
pub fn reserved_local_names(
    root: &Path,
    engine: &engine::InferenceEngine,
) -> Result<BTreeMap<String, Option<u32>>, String> {
    let mut names: BTreeMap<String, Option<u32>> = descriptor_index(root)?
        .into_keys()
        .map(|n| (n, None))
        .collect();
    for name in engine.get_active_models() {
        let dim = engine
            .get_model(&name)
            .ok()
            .and_then(|m| m.configuration().params.get("target_dim")?.as_u64())
            .map(|d| d as u32);
        names.insert(name, dim);
    }
    Ok(names)
}

/// Walk `<root>/models/*/*/ninference.hub.json`.
///
/// Dot-directories are never backends or models. The CLI stages downloads
/// in `models/.staging`; a half-written descriptor there must not appear
/// in a scan.
pub fn descriptor_index(root: &Path) -> Result<DescriptorIndex, String> {
    let models_dir = root.join(MODELS_DIR);
    let backends = std::fs::read_dir(&models_dir).map_err(|e| {
        format!(
            "cannot scan {}: {e}\n\
             (this is the engine root's models directory; set --root or \
             POSTVEC_SERVER_ROOT to the tree `postvec model pull` writes into)",
            models_dir.display()
        )
    })?;
    let mut index: DescriptorIndex = BTreeMap::new();
    for backend in backends {
        let backend = backend.map_err(|e| format!("scanning {}: {e}", models_dir.display()))?;
        let backend_path = backend.path();
        if !backend_path.is_dir() || starts_with_dot(&backend.file_name().to_string_lossy()) {
            continue;
        }
        let entries = std::fs::read_dir(&backend_path)
            .map_err(|e| format!("cannot scan {}: {e}", backend_path.display()))?;
        for model in entries {
            let model = model.map_err(|e| format!("scanning {}: {e}", backend_path.display()))?;
            let model_path = model.path();
            let Some(name) = model.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !model_path.is_dir() || starts_with_dot(&name) {
                continue;
            }
            let descriptor = model_path.join(DESCRIPTOR_FILENAME);
            if descriptor.is_file() {
                index.entry(name).or_default().push(descriptor);
            }
        }
    }
    Ok(index)
}

fn starts_with_dot(name: &str) -> bool {
    name.starts_with('.')
}

pub fn read_descriptor(path: &Path) -> Result<Descriptor, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let descriptor: Descriptor =
        serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    if descriptor.name.trim().is_empty() {
        return Err(format!("{}: field \"name\" is empty", path.display()));
    }
    Ok(descriptor)
}

fn backend_of(descriptor_path: &Path) -> String {
    descriptor_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Read every descriptor the index found. Unreadable ones become warnings.
pub fn inventory_from_index(index: &DescriptorIndex) -> Inventory {
    let mut out = Inventory::default();
    for (name, paths) in index {
        let ambiguous = paths.len() > 1;
        for path in paths {
            match read_descriptor(path) {
                Ok(descriptor) => {
                    if descriptor.name != *name {
                        out.warnings.push(format!(
                            "{} names model {:?} but sits in a directory called {name:?}; the \
                             engine refuses that mismatch, so it will not load",
                            path.display(),
                            descriptor.name
                        ));
                        continue;
                    }
                    out.models.push(DiskModel {
                        name: name.clone(),
                        enabled: descriptor.enabled,
                        backend: backend_of(path),
                        path: path.clone(),
                        ambiguous,
                    });
                }
                Err(e) => out.warnings.push(format!("skipping model {name:?}: {e}")),
            }
        }
    }
    for model in out.models.iter().filter(|m| m.ambiguous && m.enabled) {
        out.warnings.push(format!(
            "enabled model {:?} exists under more than one backend (including {:?}); it is \
             excluded because the engine would resolve it by directory order — remove the \
             duplicate directory",
            model.name, model.backend
        ));
    }
    out.models
        .sort_by(|a, b| a.name.cmp(&b.name).then(a.backend.cmp(&b.backend)));
    out.warnings.sort();
    out.warnings.dedup();
    out
}

pub fn inventory(root: &Path) -> Result<Inventory, String> {
    Ok(inventory_from_index(&descriptor_index(root)?))
}

/// Validate a request-supplied model name *before* it goes near the
/// filesystem: names are joined into `<root>/models/<backend>/<name>/`, so
/// anything path-like is refused outright rather than becoming a traversal.
pub fn validate_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model names must be non-empty".to_string());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("model name exceeds {MAX_NAME_BYTES} bytes"));
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return Err(format!("model name {name:?} must start with [A-Za-z0-9]"));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "model name {name:?} contains characters outside [A-Za-z0-9._-]"
        ));
    }
    Ok(())
}

/// The result of walking a dependency closure.
#[derive(Debug, Default, Clone)]
pub struct Closure {
    /// Every model the load would make resident, the roots included.
    pub planned: BTreeSet<String>,
    /// Members whose descriptor says `enabled: false`.
    pub disabled: BTreeSet<String>,
}

/// Walk the dependency closure of `roots`.
///
/// Disabled members are *recorded* rather than raised, because the two
/// callers want opposite answers from the same walk: the startup preflight
/// wants a resource count, and `/admin/load` wants a refusal that names the
/// deactivated model.
pub fn closure_of(
    index: &DescriptorIndex,
    roots: &[String],
    ceiling: usize,
) -> Result<Closure, String> {
    let mut closure = Closure::default();
    let mut visiting = BTreeSet::new();
    for root in roots {
        visit(index, root, 0, true, ceiling, &mut visiting, &mut closure)?;
    }
    Ok(closure)
}

#[allow(clippy::too_many_arguments)]
fn visit(
    index: &DescriptorIndex,
    name: &str,
    depth: usize,
    is_root: bool,
    ceiling: usize,
    visiting: &mut BTreeSet<String>,
    closure: &mut Closure,
) -> Result<(), String> {
    if closure.planned.contains(name) {
        return Ok(());
    }
    if depth > MAX_DEPENDENCY_DEPTH {
        return Err(format!(
            "dependency chain under {name:?} exceeds depth {MAX_DEPENDENCY_DEPTH}"
        ));
    }
    if !visiting.insert(name.to_string()) {
        return Err(format!("dependency cycle reaches model {name:?}"));
    }
    if visiting.len() + closure.planned.len() > ceiling {
        return Err(format!(
            "model roots plus dependencies exceed the resident ceiling of {ceiling} (first \
             overflow at {name:?}); shorten --models or raise --max-resident-models"
        ));
    }
    let paths = index
        .get(name)
        .ok_or_else(|| format!("model {name:?} has no descriptor under {MODELS_DIR}/<backend>/"))?;
    if paths.len() != 1 {
        return Err(format!(
            "model {name:?} exists under multiple backends; remove the duplicate directory"
        ));
    }
    let descriptor = read_descriptor(&paths[0])?;
    if descriptor.name != name {
        return Err(format!(
            "{} names model {:?}, but its directory is {name:?}",
            paths[0].display(),
            descriptor.name
        ));
    }
    if is_root && !descriptor.enabled {
        return Err(format!(
            "model {name:?} is deactivated in {}; run `postvec model activate {name}`",
            paths[0].display()
        ));
    }
    if !descriptor.enabled {
        closure.disabled.insert(name.to_string());
    }
    for dependency in &descriptor.dependencies {
        visit(
            index,
            dependency,
            depth + 1,
            false,
            ceiling,
            visiting,
            closure,
        )?;
    }
    visiting.remove(name);
    closure.planned.insert(name.to_string());
    Ok(())
}

/// The `/admin/load` closure: refused outright when any member is
/// deactivated.
///
/// `InferenceEngine::load_model` refuses a disabled configuration anyway, so
/// the load would fail mid-closure. Catching it here names the deactivated
/// model and the command that fixes it, and it matches what a restart would
/// produce — a load that would not survive one is not a load worth doing.
pub fn admin_load_closure(index: &DescriptorIndex, name: &str) -> Result<BTreeSet<String>, String> {
    let closure = closure_of(index, std::slice::from_ref(&name.to_string()), usize::MAX)?;
    if let Some(first) = closure.disabled.iter().next() {
        let names: Vec<&str> = closure.disabled.iter().map(String::as_str).collect();
        return Err(format!(
            "model {name:?} depends on deactivated model(s) [{}]; the engine will not load a \
             deactivated descriptor, so this load would not survive a restart — run `postvec \
             model activate {first}`",
            names.join(", ")
        ));
    }
    Ok(closure.planned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Root(tempfile::TempDir);

    impl Root {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn model(&self, backend: &str, name: &str, enabled: bool, deps: &[&str]) -> &Self {
            self.raw(
                backend,
                name,
                &json!({
                    "name": name,
                    "enabled": enabled,
                    "dependencies": deps,
                })
                .to_string(),
            )
        }

        fn raw(&self, backend: &str, name: &str, body: &str) -> &Self {
            let dir = self.path().join(MODELS_DIR).join(backend).join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(DESCRIPTOR_FILENAME), body).unwrap();
            self
        }
    }

    #[test]
    fn a_missing_models_directory_names_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let err = descriptor_index(dir.path()).unwrap_err();
        assert!(err.contains("POSTVEC_SERVER_ROOT"), "{err}");
    }

    #[test]
    fn the_scan_finds_enabled_models_and_skips_disabled_ones() {
        let root = Root::new();
        root.model("onnx-runtime", "embed-a", true, &[]).model(
            "onnx-runtime",
            "embed-b",
            false,
            &[],
        );
        let inv = inventory(root.path()).unwrap();
        assert_eq!(inv.loadable(), ["embed-a"]);
        assert_eq!(inv.enabled(), ["embed-a"]);
        assert_eq!(inv.models.len(), 2, "both are reported, only one loads");
        assert!(inv.warnings.is_empty(), "{:?}", inv.warnings);
    }

    /// One bad directory must not keep the rest of the root offline.
    #[test]
    fn a_malformed_descriptor_warns_and_the_rest_still_load() {
        let root = Root::new();
        root.model("onnx-runtime", "good", true, &[]).raw(
            "onnx-runtime",
            "broken",
            "{ this is not json",
        );
        let inv = inventory(root.path()).unwrap();
        assert_eq!(inv.loadable(), ["good"]);
        assert_eq!(inv.warnings.len(), 1, "{:?}", inv.warnings);
        assert!(inv.warnings[0].contains("broken"), "{:?}", inv.warnings);
    }

    #[test]
    fn a_name_directory_mismatch_is_reported_and_excluded() {
        let root = Root::new();
        root.raw(
            "onnx-runtime",
            "on-disk-name",
            &json!({"name": "other-name", "enabled": true}).to_string(),
        );
        let inv = inventory(root.path()).unwrap();
        assert!(inv.loadable().is_empty());
        assert!(
            inv.warnings[0].contains("directory called"),
            "{:?}",
            inv.warnings
        );
    }

    /// Two backends, one name: the engine picks by read order, so the scan
    /// refuses to pick at all.
    #[test]
    fn an_enabled_duplicate_is_excluded_with_a_warning() {
        let root = Root::new();
        root.model("onnx-runtime", "dup", true, &[])
            .model("candle", "dup", true, &[]);
        let inv = inventory(root.path()).unwrap();
        assert!(inv.loadable().is_empty(), "ambiguous names never load");
        assert_eq!(inv.enabled(), ["dup", "dup"], "both are still reported");
        assert!(
            inv.warnings
                .iter()
                .any(|w| w.contains("more than one backend")),
            "{:?}",
            inv.warnings
        );
    }

    #[test]
    fn dot_directories_are_invisible() {
        let root = Root::new();
        root.model("onnx-runtime", "real", true, &[])
            .model(".staging", "half-written", true, &[])
            .model("onnx-runtime", ".tmp", true, &[]);
        assert_eq!(inventory(root.path()).unwrap().loadable(), ["real"]);
    }

    #[test]
    fn closures_include_transitive_dependencies() {
        let root = Root::new();
        root.model("onnx-runtime", "bridge", true, &["converter"])
            .model("onnx-runtime", "converter", true, &["base"])
            .model("onnx-runtime", "base", true, &[]);
        let index = descriptor_index(root.path()).unwrap();
        let closure = closure_of(&index, &["bridge".to_string()], 16).unwrap();
        assert_eq!(
            closure.planned.iter().cloned().collect::<Vec<_>>(),
            ["base", "bridge", "converter"]
        );
        assert!(closure.disabled.is_empty());
    }

    #[test]
    fn closures_refuse_cycles_and_depth() {
        let root = Root::new();
        root.model("onnx-runtime", "a", true, &["b"])
            .model("onnx-runtime", "b", true, &["a"]);
        let index = descriptor_index(root.path()).unwrap();
        let err = closure_of(&index, &["a".to_string()], 16).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn closures_respect_the_resident_ceiling() {
        let root = Root::new();
        root.model("onnx-runtime", "a", true, &["b"])
            .model("onnx-runtime", "b", true, &["c"])
            .model("onnx-runtime", "c", true, &[]);
        let index = descriptor_index(root.path()).unwrap();
        let err = closure_of(&index, &["a".to_string()], 2).unwrap_err();
        assert!(err.contains("resident ceiling"), "{err}");
        assert!(closure_of(&index, &["a".to_string()], 3).is_ok());
    }

    #[test]
    fn an_explicitly_named_disabled_root_is_refused() {
        let root = Root::new();
        root.model("onnx-runtime", "off", false, &[]);
        let index = descriptor_index(root.path()).unwrap();
        let err = closure_of(&index, &["off".to_string()], 16).unwrap_err();
        assert!(err.contains("postvec model activate off"), "{err}");
    }

    /// A disabled dependency is recorded by the walk and refused on the admin path.
    #[test]
    fn admin_load_refuses_a_deactivated_dependency() {
        let root = Root::new();
        root.model("onnx-runtime", "bridge", true, &["converter"])
            .model("onnx-runtime", "converter", false, &[]);
        let index = descriptor_index(root.path()).unwrap();

        let closure = closure_of(&index, &["bridge".to_string()], 16).unwrap();
        assert_eq!(
            closure.disabled.iter().cloned().collect::<Vec<_>>(),
            ["converter"]
        );

        let err = admin_load_closure(&index, "bridge").unwrap_err();
        assert!(err.contains("postvec model activate converter"), "{err}");
    }

    #[test]
    fn a_missing_dependency_is_named() {
        let root = Root::new();
        root.model("onnx-runtime", "bridge", true, &["gone"]);
        let index = descriptor_index(root.path()).unwrap();
        let err = closure_of(&index, &["bridge".to_string()], 16).unwrap_err();
        assert!(err.contains("\"gone\""), "{err}");
    }

    #[test]
    fn model_names_are_validated_against_traversal() {
        assert!(validate_model_name("baai-bge-m3").is_ok());
        assert!(validate_model_name("model.v1_2").is_ok());
        assert!(validate_model_name("nvidia.NV-Embed-v2").is_ok());
        for bad in [
            "",
            "../escape",
            "/absolute",
            "-leading-dash",
            ".hidden",
            "has space",
            "sub/dir",
        ] {
            assert!(validate_model_name(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(validate_model_name(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
    }
}
