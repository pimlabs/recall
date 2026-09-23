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
//! nothing touches the database. [`Frontier`] is the one piece of state:
//! the O(log n) per level rather than the O(n) leaves, so an append costs
//! O(log n) hashes instead of replaying the whole tree.
//!
//! Known-answer vectors are in `tests.rs`, from
//! `transparency-dev/merkle` (`testonly/constants.go`, commit
//! `fbbcd741c3d1c69d8498487baa8edc9e5824847c`) — the reference
//! implementation RFC 9162 §2.1 itself points to via RFC 6962. rfc-editor.org
//! was not reachable from where this was written; the algorithm below was
//! checked against `google/certificate-transparency-rfcs`'s text of RFC 9162
//! on GitHub.

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
/// A plain recursive walk of the split rule. It costs O(n) hashes, so it is
/// used for a tree freshly rebuilt from storage and for the on-demand proofs
/// below — not for every append, which is what [`Frontier`] is for.
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
    index < size
        && recompute(index, size, leaf_hash, proof).as_ref() == Some(want_root)
}

/// The consistency proof between tree sizes `first` and `second` (RFC 9162
/// §2.1.4's `PROOF`, via `SUBPROOF`), over `leaves`, which must hold at
/// least `second` hashes. `first` and `second` are the tree's *sizes*, and
/// `leaves[..second]` is the state of the tree that had that many leaves —
/// which for an append-only tree is simply a prefix of every leaf it has
/// ever held.
///
/// Computed fresh from `leaves` on every call. For one owner's log this is
/// fast enough not to cache — the design's own call, made once the
/// `Frontier` above existed as the alternative and was judged unnecessary
/// here.
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
/// `first`, with only appends between them (RFC 9162 §2.1.4's verification
/// algorithm).
///
/// This mirrors [`subproof`]'s own recursion exactly — the same `m <= k` /
/// `m > k` decisions, made from `first` and `second` alone, in the same
/// order — rather than a separately-derived folding algorithm: since
/// [`subproof`] appends each level's sibling *after* recursing, walking the
/// proof forward while making the identical recursive calls consumes it in
/// exactly the order it was produced. A verifier that is structurally the
/// mirror image of the generator is correct because the generator is,
/// rather than for a reason of its own that could disagree with it.
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
    let Some(got) = reconstruct(first, second, true, first_root, &mut cursor) else {
        return false;
    };
    cursor.next().is_none() && &got == second_root
}

/// The mirror image of [`subproof`]: reconstructs `MTH(D[n])` given `m`,
/// `n`, `b`, the trusted `first_root` (`MTH(D[m])`, used exactly where
/// [`subproof`]'s base case `m == n, b` needed nothing from the proof), and
/// the remaining proof elements in production order. [`None`] if the proof
/// runs out before the recursion does.
fn reconstruct(
    m: u64,
    n: u64,
    b: bool,
    first_root: &Hash,
    proof: &mut std::slice::Iter<'_, Hash>,
) -> Option<Hash> {
    if m == n {
        return Some(if b { *first_root } else { *proof.next()? });
    }
    let k = split_point(n);
    if m <= k {
        let left = reconstruct(m, k, b, first_root, proof)?;
        Some(hash_children(&left, proof.next()?))
    } else {
        let right = reconstruct(m - k, n - k, false, first_root, proof)?;
        Some(hash_children(proof.next()?, &right))
    }
}

/// The RFC 9162 §2.1.2 stack of complete-subtree hashes: `levels[i]` is the
/// hash of the perfect subtree of `2^i` leaves at that position, or [`None`]
/// while no such subtree is complete yet. Isomorphic to a binary counter —
/// an append is exactly incrementing it, carrying (hashing two children
/// together) at every level whose bit was already set.
///
/// This is what makes an append O(log n): the root after each one is folded
/// from at most `log2(size)` stored hashes rather than recomputed over every
/// leaf.
#[derive(Debug, Clone, Default)]
pub struct Frontier {
    levels: Vec<Option<Hash>>,
    size: u64,
}

impl Frontier {
    /// An empty frontier.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the frontier from every leaf hash a tree has, in order —
    /// what the server does at start, from `audit_log.leaf_hash`.
    pub fn rebuild(leaf_hashes: &[Hash]) -> Self {
        let mut f = Self::new();
        for h in leaf_hashes {
            f.append(*h);
        }
        f
    }

    /// How many leaves have been appended.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Appends one more leaf hash, carrying up the levels it completes.
    pub fn append(&mut self, leaf_hash: Hash) {
        let mut hash = leaf_hash;
        let mut level = 0usize;
        while (self.size >> level) & 1 == 1 {
            let left = self.levels[level].take().expect("bit set implies a hash");
            hash = hash_children(&left, &hash);
            level += 1;
        }
        if level == self.levels.len() {
            self.levels.push(None);
        }
        self.levels[level] = Some(hash);
        self.size += 1;
    }

    /// The root over every leaf appended so far.
    ///
    /// Folds the complete-subtree hashes from smallest to largest, which is
    /// RFC 9162's split rule read the other way: the largest chunk is the
    /// leftmost, so each larger chunk folded in becomes the new left side of
    /// everything smaller than it.
    pub fn root(&self) -> Hash {
        let mut acc: Option<Hash> = None;
        for level in &self.levels {
            if let Some(h) = level {
                acc = Some(match acc {
                    None => *h,
                    Some(prev) => hash_children(h, &prev),
                });
            }
        }
        acc.unwrap_or_else(empty_root)
    }
}

#[cfg(test)]
#[path = "merkle_tests.rs"]
mod tests;
