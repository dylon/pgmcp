//! The **address bridge** between the P5 control plane and the P0-P2 data plane.
//!
//! The control plane ([`crate::tape::working_set`], [`crate::tape::engine`])
//! speaks in opaque [`PageAddr`]`(String)` handles; the data-plane crate
//! ([`context_tape`]) speaks in typed [`context_tape::PageAddress`] values. This
//! module is the single, lossless translation seam between them.
//!
//! ## The invariant: `PageAddr`'s string IS `PageAddress::to_path()`
//!
//! The P5 design fixed the `PageAddr` string to be a data-plane *path* (see the
//! [`crate::tape::data_plane`] module docs). The data-plane crate already
//! renders / parses exactly that path grammar:
//!
//! | `PageAddress` variant            | path string                              |
//! |----------------------------------|------------------------------------------|
//! | `Chunk { chunk_id }`             | `corpus/chunk/{chunk_id}`                 |
//! | `FileRegion { file_id, lo, hi }` | `corpus/file/{file_id}/region/{lo}..{hi}` |
//! | `File { file_id }`               | `corpus/file/{file_id}`                   |
//! | `Observation { obs_id }`         | `memory/obs/{obs_id}`                    |
//! | `Scratch { tree, slot }`         | `scratch/{tree}/{hex(slot)}`             |
//!
//! So the bridge is just [`context_tape::PageAddress::to_path`] /
//! [`context_tape::PageAddress::parse_path`] wrapped in the `PageAddr` newtype —
//! a total round-trip for every legal address (property-tested below).
//!
//! ## The `node_id` axis
//!
//! For corpus pages the data plane also relates 1:1 to a pgmcp unified-graph
//! `node_id = "<type>:<pk>"` ([`context_tape::PageAddress::node_id`]).
//! [`address_to_node_id`] is the forward map; [`node_id_to_address`] is the
//! inverse, reusing the canonical
//! [`crate::db::queries::resolve_graph_node_id`] resolver so a
//! human key (a file path, an entity slug) and a numeric pk both resolve to the
//! same address — the corpus reader never has to re-implement id resolution.

use sqlx::PgPool;

use crate::tape::working_set::PageAddr;

/// Parse a control-plane [`PageAddr`] into a typed
/// [`context_tape::PageAddress`]. Returns `None` if the string is not a legal
/// data-plane path (a malformed address — the caller treats it as a benign
/// `NotFound`, never a crash).
#[inline]
pub fn pageaddr_to_address(addr: &PageAddr) -> Option<context_tape::PageAddress> {
    context_tape::PageAddress::parse_path(addr.as_str())
}

/// Render a typed [`context_tape::PageAddress`] back into a control-plane
/// [`PageAddr`]. Total — every `PageAddress` has a canonical path.
#[inline]
pub fn address_to_pageaddr(address: &context_tape::PageAddress) -> PageAddr {
    PageAddr(address.to_path())
}

/// The unified-graph `node_id` for a corpus [`context_tape::PageAddress`], or
/// `None` for a span (`FileRegion`) / tree-local (`Scratch`) address that has no
/// single graph node. Pure (no DB) — it is just the address's own
/// [`context_tape::PageAddress::node_id`].
#[inline]
pub fn address_to_node_id(address: &context_tape::PageAddress) -> Option<String> {
    address.node_id()
}

/// Resolve a unified-graph `node_id` (`"<type>:<pk>"`, where `<pk>` may be a
/// numeric id *or* a human key like a file path / entity slug) into the typed
/// corpus [`context_tape::PageAddress`] it denotes.
///
/// The `<pk>` half is run through
/// [`crate::db::queries::resolve_graph_node_id`] so a human key is
/// translated to its numeric id (the resolver short-circuits when `<pk>` already
/// parses as an `i64`). Only the corpus node types that have a `PageAddress`
/// representation are mapped:
///
/// | node_id prefix  | `PageAddress`            |
/// |-----------------|--------------------------|
/// | `chunk:<id>`    | `Chunk { chunk_id }`     |
/// | `file:<id|path>`| `File { file_id }`       |
/// | `observation:<id>` | `Observation { obs_id }` |
///
/// Returns:
/// - `Ok(Some(addr))` — resolved to a corpus address;
/// - `Ok(None)` — well-formed but not a corpus page kind (e.g. `project:…`,
///   `topic:…`, `work_item:…`) or the key did not resolve to a row;
/// - `Err(_)` — a genuine DB fault (ADR-021 `error!`-grade at the call site).
pub async fn node_id_to_address(
    pool: &PgPool,
    node_id: &str,
) -> Result<Option<context_tape::PageAddress>, sqlx::Error> {
    let Some((node_type, key)) = node_id.split_once(':') else {
        return Ok(None);
    };
    // Only corpus kinds with a PageAddress have any business being resolved here;
    // skip the DB round-trip for everything else.
    if !matches!(node_type, "chunk" | "file" | "observation") {
        return Ok(None);
    }
    let Some(resolved) = crate::db::queries::resolve_graph_node_id(pool, node_type, key).await?
    else {
        return Ok(None);
    };
    // `resolve_graph_node_id` returns the canonical `"<type>:<numeric_pk>"`; pull
    // the numeric pk back out and build the typed address.
    let Some((_, pk_str)) = resolved.split_once(':') else {
        return Ok(None);
    };
    let Ok(pk) = pk_str.parse::<i64>() else {
        return Ok(None);
    };
    let address = match node_type {
        "chunk" => context_tape::PageAddress::Chunk { chunk_id: pk },
        "file" => context_tape::PageAddress::File { file_id: pk },
        "observation" => context_tape::PageAddress::Observation { obs_id: pk },
        _ => return Ok(None),
    };
    Ok(Some(address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use context_tape::PageAddress;
    use proptest::prelude::*;

    fn sample_tree() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x0123456789abcdef0fedcba987654321)
    }

    #[test]
    fn known_addresses_round_trip_through_pageaddr() {
        for address in [
            PageAddress::Chunk { chunk_id: 42 },
            PageAddress::FileRegion {
                file_id: 5,
                start_chunk: 2,
                end_chunk: 9,
            },
            PageAddress::File { file_id: 5 },
            PageAddress::Observation { obs_id: 100 },
            PageAddress::Scratch {
                tree: sample_tree(),
                slot: Box::new([0xde, 0xad]),
            },
        ] {
            let pa = address_to_pageaddr(&address);
            let back = pageaddr_to_address(&pa).expect("round-trips back to a PageAddress");
            assert_eq!(
                back, address,
                "address {address:?} survived PageAddr round-trip"
            );
        }
    }

    #[test]
    fn pageaddr_string_is_exactly_to_path() {
        // The load-bearing invariant: the opaque control-plane string IS the
        // data-plane path. Anything else and the bridge is lossy.
        let address = PageAddress::Chunk { chunk_id: 7 };
        assert_eq!(address_to_pageaddr(&address).0, "corpus/chunk/7");
        let region = PageAddress::FileRegion {
            file_id: 3,
            start_chunk: 1,
            end_chunk: 4,
        };
        // The region separator is ".." (NOT "-") so a negative start/end bound
        // parses unambiguously — see `context_tape::PageAddress::to_path`.
        assert_eq!(address_to_pageaddr(&region).0, "corpus/file/3/region/1..4");
        let obs = PageAddress::Observation { obs_id: 88 };
        assert_eq!(address_to_pageaddr(&obs).0, "memory/obs/88");
    }

    #[test]
    fn malformed_pageaddr_is_none_not_panic() {
        assert!(pageaddr_to_address(&PageAddr("nonsense".into())).is_none());
        assert!(pageaddr_to_address(&PageAddr("corpus/chunk/notanumber".into())).is_none());
        assert!(pageaddr_to_address(&PageAddr(String::new())).is_none());
    }

    #[test]
    fn node_id_only_for_single_node_corpus_addresses() {
        assert_eq!(
            address_to_node_id(&PageAddress::Chunk { chunk_id: 7 }).as_deref(),
            Some("chunk:7")
        );
        assert_eq!(
            address_to_node_id(&PageAddress::File { file_id: 9 }).as_deref(),
            Some("file:9")
        );
        assert_eq!(
            address_to_node_id(&PageAddress::Observation { obs_id: 3 }).as_deref(),
            Some("observation:3")
        );
        // A span and a scratch page have no single node_id.
        assert!(
            address_to_node_id(&PageAddress::FileRegion {
                file_id: 1,
                start_chunk: 0,
                end_chunk: 2
            })
            .is_none()
        );
        assert!(
            address_to_node_id(&PageAddress::Scratch {
                tree: sample_tree(),
                slot: Box::new([1])
            })
            .is_none()
        );
    }

    proptest! {
        /// Every corpus address over the **realizable domain** round-trips PageAddr
        /// → PageAddress losslessly.
        ///
        /// The domain is the values these fields actually take from the corpus:
        /// `BIGSERIAL` ids (`chunk_id` / `file_id` / `obs_id`) are ≥ 1, and a
        /// `file_chunks.chunk_index` is a non-negative `INTEGER` (chunks are
        /// 0-indexed). We deliberately do NOT fuzz the *signed* full range here:
        /// the data-plane crate's `PageAddress::to_path` / `parse_path` is only a
        /// total round-trip over non-negative chunk indices, because the region
        /// path `corpus/file/{f}/region/{lo}-{hi}` is parsed with
        /// `split_once('-')`, which is ambiguous for a NEGATIVE `lo`/`hi`
        /// (`region/-1-0` splits as `("", "1-0")`). That is a latent
        /// non-totality in `context_tape::address::parse_path` for signed inputs;
        /// it cannot arise from real corpus rows (no negative chunk index, no
        /// non-positive id), so the bridge is correct over the realizable domain.
        /// (The positional `to_key`/`from_key` axis IS total over all `i64`/`i32`
        /// via its sign-flip encoding — only the human-path axis has this limit.)
        #[test]
        fn corpus_addresses_round_trip(
            chunk_id in 1i64..=i64::MAX,
            file_id in 1i64..=i64::MAX,
            start in 0i32..=i32::MAX,
            end in 0i32..=i32::MAX,
            obs_id in 1i64..=i64::MAX,
        ) {
            for address in [
                PageAddress::Chunk { chunk_id },
                PageAddress::FileRegion { file_id, start_chunk: start, end_chunk: end },
                PageAddress::File { file_id },
                PageAddress::Observation { obs_id },
            ] {
                let pa = address_to_pageaddr(&address);
                prop_assert_eq!(pageaddr_to_address(&pa), Some(address));
            }
        }
    }
}
