//! Response-envelope budgets.
//!
//! Lifted unchanged from postvec's embedded loopback server (which lifted
//! them from its own queue engine, postvec/src/jobs.rs), so a remote node
//! refuses exactly the requests an embedded one refuses. A client that
//! sub-batches for embedded mode needs no second calibration for remote
//! mode — and postvec's own client never comes close to these; they guard
//! foreign or buggy callers.

/// The gRPC response for a batch is `count × dim` doubles in a protobuf
/// `Value` tree (~12 wire bytes per float, several times that while
/// decoding). This budget bounds one call's expected response wire size;
/// [`max_items_for_dim`] converts it to an item count.
const RESPONSE_WIRE_BUDGET_BYTES: u64 = 48 * 1024 * 1024;
const WIRE_BYTES_PER_FLOAT: u64 = 12;
/// A response exists first as the engine's `serde_json::Value` tree and then,
/// while that tree is still alive, as tonic/prost values. Ninety-six bytes per
/// component is a deliberately conservative combined accounting unit.
const RESPONSE_TREE_BUDGET_BYTES: u64 = 96 * 1024 * 1024;
const TRANSIENT_TREE_BYTES_PER_FLOAT: u64 = 96;
const TRANSIENT_TREE_BYTES_PER_ITEM: u64 = 256;

/// How many embeddings of `dim` dimensions fit one response budget. Always
/// at least 1 (a single embedding always fits any real configuration).
pub fn max_items_for_dim(dim: i32) -> usize {
    let dim = dim.max(1) as u64;
    let wire = RESPONSE_WIRE_BUDGET_BYTES / dim.saturating_mul(WIRE_BYTES_PER_FLOAT);
    let trees = RESPONSE_TREE_BUDGET_BYTES
        / dim
            .saturating_mul(TRANSIENT_TREE_BYTES_PER_FLOAT)
            .saturating_add(TRANSIENT_TREE_BYTES_PER_ITEM);
    wire.min(trees).max(1).min(MAX_REQUEST_ITEMS as u64) as usize
}

/// Absolute per-request item ceiling, independent of dimension. 4096 items
/// per gRPC call is far above any real embedding batch; the server refuses
/// above it before touching a request's items.
pub const MAX_REQUEST_ITEMS: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_dimensions_are_capped_by_the_absolute_item_ceiling() {
        assert_eq!(max_items_for_dim(1), MAX_REQUEST_ITEMS);
    }

    /// The budget, not the item ceiling, is what binds at real embedding
    /// widths — MiniLM's 384 already lands below 4096.
    #[test]
    fn the_ceiling_falls_monotonically_with_dimension() {
        let widths = [384, 768, 1024, 3072, 8192];
        let mut previous = MAX_REQUEST_ITEMS + 1;
        for dim in widths {
            let items = max_items_for_dim(dim);
            assert!(items >= 1, "dim {dim} produced {items}");
            assert!(items <= MAX_REQUEST_ITEMS, "dim {dim} produced {items}");
            assert!(
                items < previous,
                "dim {dim} produced {items}, not below the previous {previous}"
            );
            previous = items;
        }
    }

    /// A single embedding always fits, whatever the model claims, and a
    /// nonsensical dimension never produces a zero or a panic.
    #[test]
    fn degenerate_dimensions_stay_safe() {
        for dim in [0, -1, i32::MIN, i32::MAX] {
            assert!(max_items_for_dim(dim) >= 1, "dim {dim}");
        }
    }
}
