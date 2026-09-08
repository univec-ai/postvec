//! Effective route priorities. One function the gateway, CLI and doctor share.

use crate::config::ModelKind;

/// One route as the caller sees it, before priorities are assigned.
pub struct RouteInput<'a> {
    pub name: &'a str,
    pub space: &'a str,
    pub kind: ModelKind,
    pub local: bool,
    pub priority: Option<u32>,
    pub added: Option<&'a str>,
    pub file_stem: &'a str,
}

/// A served embed route with its computed priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub name: String,
    pub space: String,
    pub priority: u32,
    pub explicit: bool,
}

/// Local engine models are fixed at 100. Explicit `priority` is used as-is.
/// Remaining provider embed routes in a space get 200 plus their rank among
/// themselves: oldest `added` first (missing last), then file stem, then name.
/// Converters are ignored — `resolve_convert` keeps its own order.
pub fn effective_priorities(routes: &[RouteInput<'_>]) -> Vec<Effective> {
    let mut out = Vec::new();
    let mut unprioritised: Vec<&RouteInput<'_>> = Vec::new();
    for route in routes.iter().filter(|r| r.kind == ModelKind::Embed) {
        if route.local {
            out.push(Effective {
                name: route.name.to_string(),
                space: route.space.to_string(),
                priority: 100,
                explicit: false,
            });
        } else if let Some(priority) = route.priority {
            out.push(Effective {
                name: route.name.to_string(),
                space: route.space.to_string(),
                priority,
                explicit: true,
            });
        } else {
            unprioritised.push(route);
        }
    }
    unprioritised.sort_by(|a, b| {
        added_ord(a.added)
            .cmp(&added_ord(b.added))
            .then_with(|| a.file_stem.cmp(b.file_stem))
            .then_with(|| a.name.cmp(b.name))
    });
    let mut rank_in_space: std::collections::BTreeMap<&str, u32> = Default::default();
    for route in unprioritised {
        let rank = rank_in_space.entry(route.space).or_insert(0);
        out.push(Effective {
            name: route.name.to_string(),
            space: route.space.to_string(),
            priority: 200 + *rank,
            explicit: false,
        });
        *rank += 1;
    }
    out
}

fn added_ord(added: Option<&str>) -> (bool, i64) {
    match added.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
        Some(dt) => (false, dt.timestamp_millis()),
        None => (true, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embed<'a>(
        name: &'a str,
        space: &'a str,
        local: bool,
        priority: Option<u32>,
        added: Option<&'a str>,
        file_stem: &'a str,
    ) -> RouteInput<'a> {
        RouteInput {
            name,
            space,
            kind: ModelKind::Embed,
            local,
            priority,
            added,
            file_stem,
        }
    }

    fn by_name(out: &[Effective]) -> std::collections::BTreeMap<&str, u32> {
        out.iter().map(|e| (e.name.as_str(), e.priority)).collect()
    }

    #[test]
    fn default_order_follows_added() {
        let routes = [
            embed("newer", "s", false, None, Some("2026-09-08T12:00:00Z"), "b"),
            embed("older", "s", false, None, Some("2026-09-01T00:00:00Z"), "a"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["older"], 200);
        assert_eq!(map["newer"], 201);
        assert!(!out.iter().find(|e| e.name == "older").unwrap().explicit);
    }

    #[test]
    fn missing_added_sorts_last() {
        let routes = [
            embed(
                "stamped",
                "s",
                false,
                None,
                Some("2026-01-01T00:00:00Z"),
                "a",
            ),
            embed("unstamped", "s", false, None, None, "a"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["stamped"], 200);
        assert_eq!(map["unstamped"], 201);
    }

    #[test]
    fn explicit_beats_derived() {
        let routes = [
            embed(
                "derived",
                "s",
                false,
                None,
                Some("2026-01-01T00:00:00Z"),
                "a",
            ),
            embed("set", "s", false, Some(1), None, "b"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["set"], 1);
        assert!(out.iter().find(|e| e.name == "set").unwrap().explicit);
        assert_eq!(map["derived"], 200);
        assert!(!out.iter().find(|e| e.name == "derived").unwrap().explicit);
    }

    #[test]
    fn local_is_fixed_at_100() {
        let routes = [
            embed("local", "s", true, Some(1), None, ""),
            embed("hosted", "s", false, None, None, "p"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["local"], 100);
        assert!(!out.iter().find(|e| e.name == "local").unwrap().explicit);
        assert_eq!(map["hosted"], 200);
    }

    #[test]
    fn two_spaces_do_not_interfere() {
        let routes = [
            embed("a1", "a", false, None, Some("2026-01-01T00:00:00Z"), "x"),
            embed("a2", "a", false, None, Some("2026-02-01T00:00:00Z"), "x"),
            embed("b1", "b", false, None, Some("2026-03-01T00:00:00Z"), "y"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["a1"], 200);
        assert_eq!(map["a2"], 201);
        assert_eq!(map["b1"], 200);
    }

    #[test]
    fn converters_are_ignored() {
        let routes = [
            RouteInput {
                name: "conv",
                space: "s",
                kind: ModelKind::Convert,
                local: false,
                priority: Some(1),
                added: None,
                file_stem: "u",
            },
            embed("e", "s", false, None, None, "p"),
        ];
        let out = effective_priorities(&routes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "e");
        assert_eq!(out[0].priority, 200);
    }

    #[test]
    fn unstamped_ties_break_on_file_stem_then_name() {
        let routes = [
            embed("n2", "s", false, None, None, "b"),
            embed("n1", "s", false, None, None, "a"),
            embed("n0", "s", false, None, None, "a"),
        ];
        let out = effective_priorities(&routes);
        let map = by_name(&out);
        assert_eq!(map["n0"], 200);
        assert_eq!(map["n1"], 201);
        assert_eq!(map["n2"], 202);
    }
}
