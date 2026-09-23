//! Known-answer vectors for [`super`], plus the property tests the spec's
//! Tests table lists.
//!
//! **Vector source:** `github.com/transparency-dev/merkle`, commit
//! `fbbcd741c3d1c69d8498487baa8edc9e5824847c` (the current default branch on
//! 2026-09-23; rfc-editor.org was unreachable from where this was written).
//! `LEAVES`, `LEAF_HASHES` and `ROOTS` are copied byte for byte from
//! `testonly/constants.go`'s `LeafInputs`, `NodeHashes()[0]` and
//! `RootHashes` — the tree RFC 9162 §2.1 builds from those inputs is
//! identical to RFC 6962's, since 9162 restates 6962's hash unchanged.
//! `CONSISTENCY_*` and `INCLUSION_*` were not hand-derived: they are the
//! output of that repository's own reference implementation
//! (`testonly.Tree`, which calls `proof.Consistency` / `proof.Inclusion`),
//! run locally against the same `LeafInputs()` with:
//!
//! ```go
//! tree := testonly.New(rfc6962.DefaultHasher)
//! tree.AppendData(testonly.LeafInputs()...)
//! tree.ConsistencyProof(6, 8) // etc.
//! ```

use super::*;

/// `testonly.LeafInputs()`.
const LEAVES: &[&[u8]] = &[
    &[],
    &[0x00],
    &[0x10],
    &[0x20, 0x21],
    &[0x30, 0x31],
    &[0x40, 0x41, 0x42, 0x43],
    &[0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57],
    &[
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e,
        0x6f,
    ],
];

fn hex32(s: &str) -> Hash {
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect::<Vec<u8>>();
    bytes.try_into().unwrap()
}

/// `testonly.NodeHashes()[0]`: the leaf hashes, i.e. `HashLeaf` of each of
/// [`LEAVES`].
fn leaf_hashes() -> Vec<Hash> {
    [
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        "96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7",
        "0298d122906dcfc10892cb53a73992fc5b9f493ea4c9badb27b791b4127a7fe7",
        "07506a85fd9dd2f120eb694f86011e5bb4662e5c415a62917033d4a9624487e7",
        "bc1a0643b12e4d2d7c77918f44e0f4f79a838b6cf9ec5b5c283e1f4d88599e6b",
        "4271a26be0d8a84f0bd54c8c302e7cb3a3b5d1fa6780a40bcce2873477dab658",
        "b08693ec2e721597130641e8211e7eedccb4c26413963eee6c1e2ed16ffb1a5f",
        "46f6ffadd3d06a09ff3c5860d2755c8b9819db7df44251788c7d8e3180de8eb1",
    ]
    .iter()
    .map(|h| hex32(h))
    .collect()
}

/// `testonly.RootHashes()`: roots for trees of 0 to 8 leaves.
fn roots() -> Vec<Hash> {
    [
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
        "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
        "d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7",
        "4e3bbb1f7b478dcfe71fb631631519a3bca12c9aefca1612bfce4c13a86264d4",
        "76e67dadbcdf1e10e1b74ddc608abd2f98dfb16fbce75277b5232a127f2087ef",
        "ddb89be403809e325750d3d263cd78929c2942b7942a34b77e122c9594a74c8c",
        "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328",
    ]
    .iter()
    .map(|h| hex32(h))
    .collect()
}

/// The RFC 6962/9162 known-answer vectors: leaf hashing and the root for
/// every tree size from 0 to 8, byte for byte.
#[test]
fn roots_match_transparency_dev_merkles_vectors() {
    let hashes: Vec<Hash> = LEAVES.iter().map(|l| hash_leaf(l)).collect();
    assert_eq!(hashes, leaf_hashes(), "leaf hashing");

    let want_roots = roots();
    assert_eq!(want_roots[0], empty_root(), "empty tree");
    for size in 0..=LEAVES.len() {
        assert_eq!(
            root(&hashes[..size]),
            want_roots[size],
            "root at size {size}"
        );
    }
}

/// Computed via `testonly.Tree.ConsistencyProof`, over the same
/// `LeafInputs()`. `(first, second, proof)`.
fn consistency_vectors() -> Vec<(u64, u64, Vec<Hash>)> {
    vec![
        (
            6,
            8,
            vec![
                hex32("0ebc5d3437fbe2db158b9f126a1d118e308181031d0a949f8dededebc558ef6a"),
                hex32("ca854ea128ed050b41b35ffc1b87b8eb2bde461e9e3b5596ece6b9d5975a0ae0"),
                hex32("d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7"),
            ],
        ),
        (
            3,
            7,
            vec![
                hex32("0298d122906dcfc10892cb53a73992fc5b9f493ea4c9badb27b791b4127a7fe7"),
                hex32("07506a85fd9dd2f120eb694f86011e5bb4662e5c415a62917033d4a9624487e7"),
                hex32("fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125"),
                hex32("837dbb152e9b079010717e84e865da4ebc0fa198a806d59d31bf15accef22d0e"),
            ],
        ),
        (
            1,
            8,
            vec![
                hex32("96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7"),
                hex32("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e"),
                hex32("6b47aaf29ee3c2af9af889bc1fb9254dabd31177f16232dd6aab035ca39bf6e4"),
            ],
        ),
    ]
}

/// Computed via `testonly.Tree.InclusionProof`, over the same
/// `LeafInputs()`. `(index, size, proof)`.
fn inclusion_vectors() -> Vec<(u64, u64, Vec<Hash>)> {
    vec![
        (
            2,
            8,
            vec![
                hex32("07506a85fd9dd2f120eb694f86011e5bb4662e5c415a62917033d4a9624487e7"),
                hex32("fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125"),
                hex32("6b47aaf29ee3c2af9af889bc1fb9254dabd31177f16232dd6aab035ca39bf6e4"),
            ],
        ),
        (
            5,
            7,
            vec![
                hex32("bc1a0643b12e4d2d7c77918f44e0f4f79a838b6cf9ec5b5c283e1f4d88599e6b"),
                hex32("b08693ec2e721597130641e8211e7eedccb4c26413963eee6c1e2ed16ffb1a5f"),
                hex32("d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7"),
            ],
        ),
    ]
}

#[test]
fn consistency_proofs_match_the_reference_implementation() {
    let hashes: Vec<Hash> = LEAVES.iter().map(|l| hash_leaf(l)).collect();
    for (first, second, want) in consistency_vectors() {
        let got = consistency(first, second, &hashes);
        assert_eq!(got, want, "consistency({first}, {second})");
        assert!(
            verify_consistency(
                first,
                &root(&hashes[..first as usize]),
                second,
                &root(&hashes[..second as usize]),
                &got,
            ),
            "consistency({first}, {second}) must verify"
        );
    }
}

#[test]
fn inclusion_proofs_match_the_reference_implementation() {
    let hashes: Vec<Hash> = LEAVES.iter().map(|l| hash_leaf(l)).collect();
    for (index, size, want) in inclusion_vectors() {
        let got = inclusion(index, &hashes[..size as usize]);
        assert_eq!(got, want, "inclusion({index}, {size})");
        assert!(
            verify_inclusion(
                index,
                size,
                &hashes[index as usize],
                &got,
                &root(&hashes[..size as usize]),
            ),
            "inclusion({index}, {size}) must verify"
        );
    }
}

/// The [`Frontier`]'s incremental root must agree with the plain O(n)
/// recomputation at every size along the way, not just at the end.
#[test]
fn the_frontier_matches_recomputation_at_every_size() {
    let hashes: Vec<Hash> = LEAVES.iter().map(|l| hash_leaf(l)).collect();
    let mut frontier = Frontier::new();
    for (i, h) in hashes.iter().enumerate() {
        frontier.append(*h);
        assert_eq!(frontier.size(), i as u64 + 1);
        assert_eq!(frontier.root(), root(&hashes[..=i]), "size {}", i + 1);
    }
    assert_eq!(Frontier::rebuild(&hashes).root(), frontier.root());
}

/// The append-only property itself: a tree whose leaf 2 was rewritten
/// after a checkpoint was taken at size 4 can extend to any size it likes,
/// but no proof built from its own (tampered) leaves ever verifies against
/// the checkpoint's root — the only way to satisfy [`verify_consistency`]
/// is to have actually kept every leaf the checkpoint was taken over.
#[test]
fn a_rewritten_leaf_breaks_consistency_with_an_earlier_checkpoint() {
    let hashes: Vec<Hash> = (0u32..8).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let honest_root_at_4 = root(&hashes[..4]);

    // The same tree, but leaf 2 was rewritten after that checkpoint.
    let mut tampered = hashes.clone();
    tampered[2] = hash_leaf(b"not what was there before");
    let tampered_root_at_8 = root(&tampered);

    let forged_proof = consistency(4, 8, &tampered);
    assert!(
        !verify_consistency(4, &honest_root_at_4, 8, &tampered_root_at_8, &forged_proof),
        "a rewrite must not produce a tree that extends the honest checkpoint"
    );

    // The honest continuation — nothing before size 4 touched — still
    // verifies, which is what confirms the failure above is about the
    // rewrite and not some unrelated bug.
    let honest_proof = consistency(4, 8, &hashes);
    assert!(verify_consistency(
        4,
        &honest_root_at_4,
        8,
        &root(&hashes),
        &honest_proof
    ));
}

/// Every consistency proof between sizes up to 64 verifies, over an
/// arbitrary (not the 8-item KAT) run of leaves, so the property is checked
/// well past the vectors' own size.
#[test]
fn every_consistency_proof_up_to_64_leaves_verifies() {
    let hashes: Vec<Hash> = (0u32..64).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    for second in 1..=hashes.len() as u64 {
        for first in 1..=second {
            let proof = consistency(first, second, &hashes[..second as usize]);
            let first_root = root(&hashes[..first as usize]);
            let second_root = root(&hashes[..second as usize]);
            assert!(
                verify_consistency(first, &first_root, second, &second_root, &proof),
                "consistency({first}, {second}) failed to verify"
            );
        }
    }
}

/// Every inclusion proof up to 64 leaves verifies.
#[test]
fn every_inclusion_proof_up_to_64_leaves_verifies() {
    let hashes: Vec<Hash> = (0u32..64).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    for size in 1..=hashes.len() as u64 {
        for index in 0..size {
            let proof = inclusion(index, &hashes[..size as usize]);
            let want_root = root(&hashes[..size as usize]);
            assert!(
                verify_inclusion(index, size, &hashes[index as usize], &proof, &want_root),
                "inclusion({index}, {size}) failed to verify"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The mutations the spec's Tests table names, applied and reverted by hand:
// swap the two domain-separation prefixes, and split at n/2 instead of the
// largest power of two below n. Both must break the known-answer vectors.
// ---------------------------------------------------------------------------

fn hash_leaf_with_prefix(leaf: &[u8], prefix: u8) -> Hash {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([prefix]);
    h.update(leaf);
    h.finalize().into()
}

fn hash_children_with_prefix(left: &Hash, right: &Hash, prefix: u8) -> Hash {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([prefix]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Swapping the `0x00`/`0x01` prefixes must produce a different root than
/// the known-answer vector — pinning that the prefixes are load-bearing,
/// not decorative.
#[test]
fn swapping_the_leaf_and_node_prefixes_breaks_the_vectors() {
    // Leaves hashed with the node prefix, nodes with the leaf prefix: the
    // opposite of `hash_leaf`/`hash_children`.
    fn root_swapped(leaves: &[&[u8]]) -> Hash {
        let hashes: Vec<Hash> = leaves
            .iter()
            .map(|l| hash_leaf_with_prefix(l, 0x01))
            .collect();
        fn go(leaves: &[Hash]) -> Hash {
            match leaves.len() {
                0 => {
                    use sha2::{Digest, Sha256};
                    Sha256::digest([]).into()
                }
                1 => leaves[0],
                n => {
                    let k = split_point(n as u64) as usize;
                    hash_children_with_prefix(&go(&leaves[..k]), &go(&leaves[k..]), 0x00)
                }
            }
        }
        go(&hashes)
    }
    assert_ne!(root_swapped(LEAVES), roots()[8], "the swap must be caught");
}

/// Splitting at `n / 2` instead of the largest power of two below `n` must
/// also produce a different root for a non-power-of-two size.
#[test]
fn splitting_at_half_instead_of_the_largest_power_of_two_breaks_the_vectors() {
    fn root_half_split(leaves: &[Hash]) -> Hash {
        match leaves.len() {
            0 => empty_root(),
            1 => leaves[0],
            n => {
                let k = n / 2; // wrong: RFC 9162 wants the largest power of two below n
                hash_children(
                    &root_half_split(&leaves[..k]),
                    &root_half_split(&leaves[k..]),
                )
            }
        }
    }
    let hashes: Vec<Hash> = LEAVES.iter().map(|l| hash_leaf(l)).collect();
    // Size 8 is itself a power of two, where n/2 and the real split point
    // agree; size 7 is where they diverge.
    assert_ne!(
        root_half_split(&hashes[..7]),
        roots()[7],
        "the split bug must be caught"
    );
}
