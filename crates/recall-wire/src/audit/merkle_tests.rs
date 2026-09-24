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
//!
//! `consistency_probes.json` is that repository's `testdata/consistency/`
//! at the same commit, every file, gathered into one: the good and bad
//! proofs its own verifier is tested with.
//!
//! What these pin is the production code itself. A mutation — the two
//! prefixes swapped, the split taken at `n / 2`, a check dropped from the
//! verifier — changes a root or a verdict here, so no test reimplements a
//! mutated function to show that the mutation would differ.

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

/// The [`Tree`]'s root, and its proofs, must agree with the plain O(n)
/// recomputation at every size along the way, not just at the end.
#[test]
fn the_tree_matches_recomputation_at_every_size() {
    let hashes: Vec<Hash> = (0u32..64).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let mut tree = Tree::new();
    assert_eq!(tree.root(), empty_root());
    for (i, h) in hashes.iter().enumerate() {
        tree.append(*h);
        assert_eq!(tree.size(), i as u64 + 1);
        assert_eq!(tree.root(), root(&hashes[..=i]), "size {}", i + 1);
    }
    let rebuilt = Tree::rebuild(hashes.iter().copied());
    assert_eq!(rebuilt.root(), tree.root());
    for second in 0..=hashes.len() as u64 {
        assert_eq!(
            tree.root_at(second),
            root(&hashes[..second as usize]),
            "root_at({second})"
        );
        for first in 0..=second {
            assert_eq!(
                tree.consistency(first, second),
                consistency(first, second, &hashes),
                "consistency({first}, {second})"
            );
        }
    }
}

/// The known-answer vectors, served from the [`Tree`] rather than the
/// reference walk.
#[test]
fn the_tree_serves_the_reference_implementations_proofs() {
    let tree = Tree::rebuild(LEAVES.iter().map(|l| hash_leaf(l)));
    assert_eq!(tree.root(), roots()[8]);
    for (first, second, want) in consistency_vectors() {
        assert_eq!(tree.consistency(first, second), want, "({first}, {second})");
    }
}

thread_local! {
    /// How many times [`hash_children`] ran on this thread.
    static NODE_HASHES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one [`hash_children`] call, for the test below.
pub(super) fn count_node_hash() {
    NODE_HASHES.with(|c| c.set(c.get() + 1));
}

fn node_hashes() -> u64 {
    NODE_HASHES.with(std::cell::Cell::get)
}

/// A proof, and the roots either side of it, cost a few hundred hashes at
/// most however long the log is — the reason `/v1/audit/consistency` no
/// longer reads every `leaf_hash` under the store's lock. Counted rather
/// than timed, so it cannot flake: the walk it replaced hashed every leaf,
/// about `n` node hashes here.
#[test]
fn a_proof_costs_log_squared_hashes_not_one_per_leaf() {
    let n: u64 = (1 << 17) + 12_345;
    let leaves: Vec<Hash> = (0..n).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let tree = Tree::rebuild(leaves.iter().copied());
    for (first, second) in [(3, n), (n - 1, n), (n / 2 + 1, n), (1 << 16, n - 7)] {
        let before = node_hashes();
        let proof = tree.consistency(first, second);
        let first_root = tree.root_at(first);
        let second_root = tree.root_at(second);
        let cost = node_hashes() - before;
        assert!(
            cost <= 18 * 18 * 2,
            "consistency({first}, {second}) cost {cost} hashes"
        );
        assert!(verify_consistency(
            first,
            &first_root,
            second,
            &second_root,
            &proof
        ));
        if first == 3 {
            // Once, against the O(n) reference walk.
            assert_eq!(proof, consistency(first, second, &leaves));
        }
    }
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

/// The append-only property itself, exhaustively to 64 leaves: for every
/// pair of sizes, rewrite any one leaf of the first tree and build the
/// proof honestly from the rewritten tree. It must not verify against the
/// checkpoint taken before the rewrite — the one thing a consistency proof
/// exists to refuse.
///
/// This is the test the first verifier failed: it rebuilt only the second
/// root, used `first_root` only when `first` was a power of two, and so
/// accepted 4805 of these 5456 rewrites up to size 32.
#[test]
fn a_rewritten_leaf_breaks_every_proof_from_an_earlier_checkpoint() {
    let honest: Vec<Hash> = (0u32..64).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let honest_tree = Tree::rebuild(honest.iter().copied());
    let mut checked = 0u32;
    for rewritten in 0..honest.len() {
        let mut leaves = honest.clone();
        leaves[rewritten] = hash_leaf(b"not what was there before");
        let tampered = Tree::rebuild(leaves);
        // The rewrite is inside the first tree when first > rewritten.
        for first in rewritten as u64 + 1..honest.len() as u64 {
            let checkpoint = honest_tree.root_at(first);
            for second in first + 1..=honest.len() as u64 {
                let forged = tampered.consistency(first, second);
                assert!(
                    !verify_consistency(
                        first,
                        &checkpoint,
                        second,
                        &tampered.root_at(second),
                        &forged
                    ),
                    "leaf {rewritten} rewritten: consistency({first}, {second}) still verified"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, (1..64u32).map(|f| f * (64 - f)).sum::<u32>());
}

/// The honest proof checked against a wrong `first_root` fails for every
/// pair of sizes — including the ones that are not a power of two, where
/// the first verifier never looked at `first_root` at all (it accepted
/// `[0; 32]` for `first` = 3).
#[test]
fn a_wrong_first_root_fails_at_every_size() {
    let hashes: Vec<Hash> = (0u32..64).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let tree = Tree::rebuild(hashes.iter().copied());
    for second in 2..=hashes.len() as u64 {
        for first in 1..second {
            let proof = tree.consistency(first, second);
            let second_root = tree.root_at(second);
            for wrong in [[0u8; 32], [0xff; 32], tree.root_at(first - 1)] {
                assert!(
                    !verify_consistency(first, &wrong, second, &second_root, &proof),
                    "consistency({first}, {second}) accepted a wrong first root"
                );
            }
        }
    }
}

/// Every single node of every honest proof, corrupted in turn, and every
/// proof with a node dropped or one added, fails.
#[test]
fn a_corrupted_truncated_or_padded_proof_fails() {
    let hashes: Vec<Hash> = (0u32..40).map(|i| hash_leaf(&i.to_be_bytes())).collect();
    let tree = Tree::rebuild(hashes.iter().copied());
    for second in 2..=hashes.len() as u64 {
        for first in 1..second {
            let (r1, r2) = (tree.root_at(first), tree.root_at(second));
            let proof = tree.consistency(first, second);
            for i in 0..proof.len() {
                let mut bad = proof.clone();
                bad[i][0] ^= 1;
                assert!(!verify_consistency(first, &r1, second, &r2, &bad));
                let mut short = proof.clone();
                short.remove(i);
                assert!(!verify_consistency(first, &r1, second, &r2, &short));
            }
            let mut long = proof.clone();
            long.push(r1);
            assert!(!verify_consistency(first, &r1, second, &r2, &long));
            let mut long = proof.clone();
            long.insert(0, r2);
            assert!(!verify_consistency(first, &r1, second, &r2, &long));
        }
    }
}

/// transparency-dev/merkle's own probes for its `VerifyConsistency`, every
/// one (see `consistency_probes.json` for the commit): its happy paths must
/// verify here, and each of its bad proofs — a flipped bit, a node too many
/// or too few, a size off by one, the roots swapped, sizes out of order —
/// must not.
///
/// A few probes use byte strings that are not 32 bytes — `"don't care"` for
/// a root that only has to equal itself, `""` for garbage around a proof.
/// `&[Hash]` cannot hold those, so each is read as its SHA-256 instead:
/// equal strings stay equal and different ones different, which is all
/// such a probe depends on.
#[test]
fn transparency_devs_consistency_probes_agree() {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let doc: serde_json::Value =
        serde_json::from_str(include_str!("consistency_probes.json")).unwrap();
    let probes = doc["probes"].as_array().unwrap();
    assert_eq!(probes.len(), 98, "every file under testdata/consistency/");
    let hash = |v: &serde_json::Value| -> Hash {
        let bytes = STANDARD.decode(v.as_str().unwrap()).unwrap();
        bytes
            .clone()
            .try_into()
            .unwrap_or_else(|_| sha2::Sha256::digest(&bytes).into())
    };
    let (mut verified, mut refused) = (0, 0);
    for p in probes {
        let name = p["file"].as_str().unwrap();
        let want_ok = !p["want_err"].as_bool().unwrap();
        let proof: Vec<Hash> = p["proof"].as_array().unwrap().iter().map(hash).collect();
        let got_ok = verify_consistency(
            p["size1"].as_u64().unwrap(),
            &hash(&p["root1"]),
            p["size2"].as_u64().unwrap(),
            &hash(&p["root2"]),
            &proof,
        );
        assert_eq!(got_ok, want_ok, "{name}");
        if got_ok {
            verified += 1;
        } else {
            refused += 1;
        }
    }
    assert_eq!((verified, refused), (6, 92));
}
