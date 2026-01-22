//! Response-envelope budgets, lifted from postvec's queue engine
//! (postvec/src/jobs.rs) so the fixture refuses exactly the requests the
//! embedded loopback server refuses.

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
pub(crate) fn max_items_for_dim(dim: i32) -> usize {
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
pub(crate) const MAX_REQUEST_ITEMS: usize = 4096;
