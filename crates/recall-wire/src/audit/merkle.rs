//! The Merkle tree hash, RFC 9162 §2.1 exactly (the same tree RFC 6962
//! defines, restated for Certificate Transparency 2.0). SHA-256 throughout.
//!
//! A leaf hashes as `SHA-256(0x00 || leaf)`, an inner node as
//! `SHA-256(0x01 || left || right)`, and a tree of `n` leaves splits at `k`,
//! the largest power of two strictly smaller than `n`: the left subtree
//! holds the first `k` leaves, the right the rest, each hashed the same way
//! recursively. The two prefixes are what the RFC calls the domain
//! separation "required to give second preimage resistance" — without them
//! a leaf `L` and an internal node whose children happen to concatenate to
//! the same bytes as `L` would collide.
//!
//! Every function here is a pure computation over hashes already in memory;
//! nothing touches the database. The free functions ([`root`],
//! [`consistency`], [`inclusion`]) walk a slice of leaf hashes and cost O(n):
//! they are the plain statement of the RFC, and the reference the tests
//! hold [`Tree`] to. [`Tree`] is what the server keeps: the hash of every
//! complete subtree, so an append, a root and a consistency proof each cost
//! a handful of hashes rather than a pass over every leaf.
//!
//! It is here, in the contract, rather than in `recall-server` where it was
//! written, because both halves run it: the server builds its roots and
//! proofs with it, and the client checks those with [`verify_consistency`]
//! and rebuilds an export's roots with [`Tree`]. One implementation on both
//! sides of a proof means a bug in it is one bug to find, not two that agree
//! with each other by accident; `scripts/audit-verify.py` is the second,
//! independent implementation that guards against that.
//!
//! Known-answer vectors are in `merkle_tests.rs`, from
//! `transparency-dev/merkle` (`testonly/constants.go` and
//! `testdata/consistency/`, commit `fbbcd741c3d1c69d8498487baa8edc9e5824847c`)
//! — the reference implementation RFC 9162 §2.1 itself points to via RFC
//! 6962. rfc-editor.org was not reachable from where this was written; the
//! algorithm below was checked against `google/certificate-transparency-rfcs`'s
//! text of RFC 9162 on GitHub.

use sha2::{Digest, Sha256};

/// Domain separation for a leaf hash (RFC 9162 §2.1.1).
const LEAF_PREFIX: u8 = 0x00;
/// Domain separation for an internal node hash (RFC 9162 §2.1.1).
const NODE_PREFIX: u8 = 0x01;

/// A node or leaf hash: 32 bytes of SHA-256.
pub type Hash = [u8; 32];

/// `SHA-256(0x00 || leaf)`.
pub fn hash_leaf(leaf: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([LEAF_PREFIX]);
    h.update(leaf);
    h.finalize().into()
}

/// `SHA-256(0x01 || left || right)`.
pub fn hash_children(left: &Hash, right: &Hash) -> Hash {
    #[cfg(test)]
    tests::count_node_hash();
    let mut h = Sha256::new();
    h.update([NODE_PREFIX]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// `MTH({}) = SHA-256()`: the hash of an empty string, carrying neither
/// prefix, because there is no leaf and no node to separate it from. RFC
/// 9162 §2.1.1 gives this as the root of a tree with no leaves.
pub fn empty_root() -> Hash {
    Sha256::digest([]).into()
}

/// The largest power of two strictly smaller than `n`. RFC 9162 §2.1.1's
/// split point; `n` must be at least 2.
fn split_point(n: u64) -> u64 {
    debug_assert!(n >= 2);
    1u64 << (63 - (n - 1).leading_zeros())
}

/// `MTH(D[n])`: the root over `leaves`, already hashed with [`hash_leaf`].
///
/// A plain recursive walk of the split rule, O(n) hashes: the reference
/// [`Tree::root`] is tested against, and what an offline verifier that
/// holds every leaf computes.
pub fn root(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => empty_root(),
        1 => leaves[0],
        n => {
            let k = split_point(n as u64) as usize;
            hash_children(&root(&leaves[..k]), &root(&leaves[k..]))
        }
    }
}

/// The inclusion proof for leaf `index` (0-based) in the tree over `leaves`
/// (RFC 9162 §2.1.3's `PATH`).
///
/// Not used by any route PR 1 adds — the wire contract calls only for a
/// consistency proof — but it is the natural companion of [`consistency`]
/// and the two share known-answer vectors, so it is implemented and pinned
/// alongside it rather than left to a later pull request to get right under
/// less scrutiny.
pub fn inclusion(index: u64, leaves: &[Hash]) -> Vec<Hash> {
    fn path(m: u64, leaves: &[Hash]) -> Vec<Hash> {
        let n = leaves.len() as u64;
        if n <= 1 {
            return Vec::new();
        }
        let k = split_point(n);
        if m < k {
            let mut proof = path(m, &leaves[..k as usize]);
            proof.push(root(&leaves[k as usize..]));
            proof
        } else {
            let mut proof = path(m - k, &leaves[k as usize..]);
            proof.push(root(&leaves[..k as usize]));
            proof
        }
    }
    path(index, leaves)
}

/// Whether `proof` is a valid inclusion proof that `leaf_hash` is leaf
/// `index` of the tree whose root is `want_root`, over `size` leaves.
pub fn verify_inclusion(
    index: u64,
    size: u64,
    leaf_hash: &Hash,
    proof: &[Hash],
    want_root: &Hash,
) -> bool {
    fn recompute(m: u64, n: u64, leaf_hash: &Hash, proof: &[Hash]) -> Option<Hash> {
        if n <= 1 {
            return if proof.is_empty() {
                Some(*leaf_hash)
            } else {
                None
            };
        }
        let k = split_point(n);
        let (&sibling, rest) = proof.split_last()?;
        if m < k {
            let left = recompute(m, k, leaf_hash, rest)?;
            Some(hash_children(&left, &sibling))
        } else {
            let right = recompute(m - k, n - k, leaf_hash, rest)?;
            Some(hash_children(&sibling, &right))
        }
    }
    index < size && recompute(index, size, leaf_hash, proof).as_ref() == Some(want_root)
}

/// The consistency proof between tree sizes `first` and `second` (RFC 9162
/// §2.1.4's `PROOF`, via `SUBPROOF`), over `leaves`, which must hold at
/// least `second` hashes. `first` and `second` are the tree's *sizes*, and
/// `leaves[..second]` is the state of the tree that had that many leaves —
/// which for an append-only tree is simply a prefix of every leaf it has
/// ever held.
///
/// O(n) hashes: the reference [`Tree::consistency`] is held to, not what
/// the server runs.
pub fn consistency(first: u64, second: u64, leaves: &[Hash]) -> Vec<Hash> {
    assert!(first <= second && second as usize <= leaves.len());
    if first == 0 || first == second {
        return Vec::new();
    }
    subproof(first, &leaves[..second as usize], true)
}

/// `SUBPROOF(m, D[n], b)`. `b` is true while the subtree is still known to
/// be the one the proof was requested against — RFC 9162's flag for whether
/// `MTH(D[m])` needs to be included outright rather than assumed known.
fn subproof(m: u64, leaves: &[Hash], b: bool) -> Vec<Hash> {
    let n = leaves.len() as u64;
    if m == n {
        return if b { Vec::new() } else { vec![root(leaves)] };
    }
    let k = split_point(n);
    if m <= k {
        let mut proof = subproof(m, &leaves[..k as usize], b);
        proof.push(root(&leaves[k as usize..]));
        proof
    } else {
        let mut proof = subproof(m - k, &leaves[k as usize..], false);
        proof.push(root(&leaves[..k as usize]));
        proof
    }
}

/// Whether `proof` shows that a tree with root `second_root` at size
/// `second` really is the tree with root `first_root` at the earlier size
/// `first`, with only appends between them (RFC 9162 §2.1.4's verification).
///
/// It walks the recursion [`consistency`] builds a proof with — the same
/// `m <= k` / `m > k` decisions of RFC 9162's `SUBPROOF`, made from `first`
/// and `second` alone — consuming the proof in
/// the order it was produced, and rebuilds **two** roots from it at once:
/// the old tree's and the new one's. Every proof node is a subtree both
/// trees share, or part of the new tree only; which, is fixed by the sizes.
/// The proof is accepted only when the old root it rebuilds is `first_root`
/// and the new one is `second_root`, with no node left over.
///
/// Both halves of that are the point. Rebuilding only the new root checks
/// that the proof is *a* proof for `second_root`; it says nothing about
/// `first_root`, which enters the recursion only when `first` is a power of
/// two. The version this replaced did exactly that, and so accepted a proof
/// cut from a tree whose first `first` leaves had been rewritten, against
/// the honest checkpoint — the one thing a consistency proof exists to
/// refuse.
pub fn verify_consistency(
    first: u64,
    first_root: &Hash,
    second: u64,
    second_root: &Hash,
    proof: &[Hash],
) -> bool {
    if first == 0 || first > second {
        return false;
    }
    if first == second {
        return proof.is_empty() && first_root == second_root;
    }
    let mut cursor = proof.iter();
    let Some((old, new)) = reconstruct(first, second, true, first_root, &mut cursor) else {
        return false;
    };
    cursor.next().is_none() && &old == first_root && &new == second_root
}

/// The mirror image of [`subproof`]: from `m`, `n`, `b` and the proof nodes
/// still to come, `(MTH(D[m]), MTH(D[n]))` for this subtree — its first `m`
/// leaves, which the old tree also had, and all `n` of them.
///
/// Where [`subproof`]'s base case (`m == n` with `b`) put nothing in the
/// proof, the subtree is the whole old tree, a complete one on the left
/// edge, and its hash is `first_root` itself; [`verify_consistency`] still
/// compares the old root rebuilt against `first_root`, which in that case
/// holds by construction. [`None`] if the proof runs out before the
/// recursion does.
fn reconstruct(
    m: u64,
    n: u64,
    b: bool,
    first_root: &Hash,
    proof: &mut std::slice::Iter<'_, Hash>,
) -> Option<(Hash, Hash)> {
    if m == n {
        let node = if b { *first_root } else { *proof.next()? };
        return Some((node, node));
    }
    let k = split_point(n);
    if m <= k {
        // The old tree ends inside the left subtree: whatever it rebuilds
        // for the old tree is the answer here too, and the right subtree,
        // new leaves only, is one node of the proof.
        let (old, new_left) = reconstruct(m, k, b, first_root, proof)?;
        Some((old, hash_children(&new_left, proof.next()?)))
    } else {
        // The old tree holds the whole left subtree and part of the right:
        // both roots share the left node, and differ on the right.
        let (old_right, new_right) = reconstruct(m - k, n - k, false, first_root, proof)?;
        let left = proof.next()?;
        Some((
            hash_children(left, &old_right),
            hash_children(left, &new_right),
        ))
    }
}

/// The tree the server keeps in memory: the hash of every complete
/// (power-of-two, aligned) subtree, level by level. `levels[0]` is every
/// leaf hash; `levels[l][i]` is the root of leaves `i·2^l` to
/// `(i+1)·2^l - 1`, present once all of those exist.
///
/// Any subtree RFC 9162's split rule ever asks about starts at a multiple of
/// the largest power of two it holds, so it is at most `log2(n)` of these
/// nodes hashed together. That makes a root O(log n) hashes and a
/// consistency proof O(log² n) — about 400 at a million leaves — without a
/// pass over the leaves or a read of the database. The cost is memory: two
/// hashes per leaf, 64 bytes, 64 MB for a million leaves.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    levels: Vec<Vec<Hash>>,
}

impl Tree {
    /// An empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the tree from every leaf hash it has, in order — what the
    /// server does at start.
    pub fn rebuild(leaf_hashes: impl IntoIterator<Item = Hash>) -> Self {
        let mut tree = Self::new();
        for h in leaf_hashes {
            tree.append(h);
        }
        tree
    }

    /// How many leaves have been appended.
    pub fn size(&self) -> u64 {
        self.levels.first().map_or(0, |leaves| leaves.len() as u64)
    }

    /// Appends one more leaf hash, and the hash of every subtree it
    /// completes: one per level whose count it makes even.
    pub fn append(&mut self, leaf_hash: Hash) {
        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }
        self.levels[0].push(leaf_hash);
        let mut level = 0;
        while self.levels[level].len().is_multiple_of(2) {
            let nodes = &self.levels[level];
            let parent = hash_children(&nodes[nodes.len() - 2], &nodes[nodes.len() - 1]);
            if self.levels.len() == level + 1 {
                self.levels.push(Vec::new());
            }
            self.levels[level + 1].push(parent);
            level += 1;
        }
    }

    /// The root over every leaf appended so far.
    pub fn root(&self) -> Hash {
        match self.size() {
            0 => empty_root(),
            n => self.subtree(0, n),
        }
    }

    /// The root over the first `size` leaves: the tree as it was when it
    /// had that many.
    pub fn root_at(&self, size: u64) -> Hash {
        assert!(size <= self.size());
        match size {
            0 => empty_root(),
            n => self.subtree(0, n),
        }
    }

    /// [`consistency`], from the stored subtrees rather than every leaf.
    /// `first` and `second` as there; `second` must not be past the tree's
    /// size.
    pub fn consistency(&self, first: u64, second: u64) -> Vec<Hash> {
        assert!(first <= second && second <= self.size());
        let mut proof = Vec::new();
        if first != 0 && first != second {
            self.subproof(first, 0, second, true, &mut proof);
        }
        proof
    }

    /// [`subproof`], for the `n` leaves from `start`.
    fn subproof(&self, m: u64, start: u64, n: u64, b: bool, proof: &mut Vec<Hash>) {
        if m == n {
            if !b {
                proof.push(self.subtree(start, n));
            }
            return;
        }
        let k = split_point(n);
        if m <= k {
            self.subproof(m, start, k, b, proof);
            proof.push(self.subtree(start + k, n - k));
        } else {
            self.subproof(m - k, start + k, n - k, false, proof);
            proof.push(self.subtree(start, k));
        }
    }

    /// `MTH` of the `n` leaves from `start`, which must be a subtree the
    /// split rule produces: `start` a multiple of the largest power of two
    /// no greater than `n`, and every leaf in it appended.
    fn subtree(&self, start: u64, n: u64) -> Hash {
        if n.is_power_of_two() {
            let level = n.trailing_zeros() as usize;
            debug_assert_eq!(start % n, 0, "a complete subtree is aligned");
            return self.levels[level][(start >> level) as usize];
        }
        let k = split_point(n);
        hash_children(&self.subtree(start, k), &self.subtree(start + k, n - k))
    }
}

#[cfg(test)]
#[path = "merkle_tests.rs"]
mod tests;
