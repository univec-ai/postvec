// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Response-envelope budgets, matching the embedded loopback server.
//! These guard foreign or buggy callers.

/// One call's response wire budget. A batch is `count x dim` doubles
/// (~12 wire bytes per float, more while decoding). [`max_items_for_dim`]
/// turns the budget into an item count.
const RESPONSE_WIRE_BUDGET_BYTES: u64 = 48 * 1024 * 1024;
const WIRE_BYTES_PER_FLOAT: u64 = 12;
/// Combined budget while the engine JSON tree and the prost tree coexist.
/// Ninety-six bytes per component is a conservative accounting unit.
const RESPONSE_TREE_BUDGET_BYTES: u64 = 96 * 1024 * 1024;
const TRANSIENT_TREE_BYTES_PER_FLOAT: u64 = 96;
const TRANSIENT_TREE_BYTES_PER_ITEM: u64 = 256;

/// Embeddings of `dim` that fit one response budget. Always at least 1.
pub fn max_items_for_dim(dim: i32) -> usize {
    let dim = dim.max(1) as u64;
    let wire = RESPONSE_WIRE_BUDGET_BYTES / dim.saturating_mul(WIRE_BYTES_PER_FLOAT);
    let trees = RESPONSE_TREE_BUDGET_BYTES
        / dim
            .saturating_mul(TRANSIENT_TREE_BYTES_PER_FLOAT)
            .saturating_add(TRANSIENT_TREE_BYTES_PER_ITEM);
    wire.min(trees).max(1).min(MAX_REQUEST_ITEMS as u64) as usize
}

/// Item ceiling, independent of dimension. Refused before the items are walked.
pub const MAX_REQUEST_ITEMS: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_dimensions_are_capped_by_the_absolute_item_ceiling() {
        assert_eq!(max_items_for_dim(1), MAX_REQUEST_ITEMS);
    }

    /// At real embedding widths the byte budget binds before the item ceiling.
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

    /// A single embedding always fits, even for a nonsense dimension.
    #[test]
    fn degenerate_dimensions_stay_safe() {
        for dim in [0, -1, i32::MIN, i32::MAX] {
            assert!(max_items_for_dim(dim) >= 1, "dim {dim}");
        }
    }
}
