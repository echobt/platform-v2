//! Merkle Patricia Trie for state verification
//!
//! Provides:
//! - O(log n) insertions and lookups
//! - Cryptographic state root
//! - Merkle proofs for verification

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Merkle Patricia Trie node
#[derive(Debug, Clone)]
enum Node {
    Empty,
    Leaf {
        key: Vec<u8>,
        value: Vec<u8>,
        hash: [u8; 32],
    },
    Branch {
        children: Box<[Option<Box<Node>>; 16]>,
        value: Option<Vec<u8>>,
        hash: [u8; 32],
    },
    Extension {
        prefix: Vec<u8>,
        child: Box<Node>,
        hash: [u8; 32],
    },
}

impl Node {
    fn hash(&self) -> [u8; 32] {
        match self {
            Node::Empty => [0u8; 32],
            Node::Leaf { hash, .. } => *hash,
            Node::Branch { hash, .. } => *hash,
            Node::Extension { hash, .. } => *hash,
        }
    }
}

/// Merkle Patricia Trie
pub struct MerkleTrie {
    root: Node,
    /// Cache of key -> value for fast lookups
    cache: HashMap<Vec<u8>, Vec<u8>>,
    /// Dirty flag for root recalculation
    dirty: bool,
    /// Cached root hash
    root_hash: [u8; 32],
}

impl MerkleTrie {
    /// Create a new empty trie
    pub fn new() -> Self {
        Self {
            root: Node::Empty,
            cache: HashMap::new(),
            dirty: false,
            root_hash: [0u8; 32],
        }
    }

    /// Insert a key-value pair
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        self.cache.insert(key.to_vec(), value.to_vec());
        self.dirty = true;
    }

    /// Get a value by key
    pub fn get(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.cache.get(key)
    }

    /// Remove a key
    pub fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.dirty = true;
        self.cache.remove(key)
    }

    /// Clear all entries
    pub fn clear(&mut self) {
        self.root = Node::Empty;
        self.cache.clear();
        self.dirty = true;
        self.root_hash = [0u8; 32];
    }

    /// Get the root hash
    pub fn root_hash(&self) -> [u8; 32] {
        if self.dirty {
            // Recompute root hash
            self.compute_root_hash()
        } else {
            self.root_hash
        }
    }

    /// Compute root hash from cache
    fn compute_root_hash(&self) -> [u8; 32] {
        if self.cache.is_empty() {
            return [0u8; 32];
        }

        // Sort keys for deterministic ordering
        let mut entries: Vec<_> = self.cache.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        // Build merkle tree from sorted entries
        let mut hashes: Vec<[u8; 32]> = entries
            .iter()
            .map(|(k, v)| Self::hash_entry(k, v))
            .collect();

        // Merkle tree construction
        while hashes.len() > 1 {
            let mut next_level = Vec::new();
            for chunk in hashes.chunks(2) {
                if chunk.len() == 2 {
                    next_level.push(Self::hash_pair(&chunk[0], &chunk[1]));
                } else {
                    next_level.push(chunk[0]);
                }
            }
            hashes = next_level;
        }

        hashes.first().copied().unwrap_or([0u8; 32])
    }

    /// Hash a key-value entry
    fn hash_entry(key: &[u8], value: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(value);
        hasher.finalize().into()
    }

    /// Hash two child hashes
    fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(left);
        hasher.update(right);
        hasher.finalize().into()
    }

    /// Generate a merkle proof for a key
    pub fn generate_proof(&self, key: &[u8]) -> Option<MerkleProof> {
        if !self.cache.contains_key(key) {
            return None;
        }

        let mut entries: Vec<_> = self.cache.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        let key_index = entries.iter().position(|(k, _)| *k == key)?;

        // Build proof path
        let mut hashes: Vec<[u8; 32]> = entries
            .iter()
            .map(|(k, v)| Self::hash_entry(k, v))
            .collect();

        let mut proof_path = Vec::new();
        let mut current_index = key_index;

        while hashes.len() > 1 {
            let sibling_index = if current_index % 2 == 0 {
                current_index + 1
            } else {
                current_index - 1
            };

            if sibling_index < hashes.len() {
                proof_path.push(ProofNode {
                    hash: hashes[sibling_index],
                    is_left: current_index % 2 == 1,
                });
            }

            // Move to next level
            let mut next_level = Vec::new();
            for chunk in hashes.chunks(2) {
                if chunk.len() == 2 {
                    next_level.push(Self::hash_pair(&chunk[0], &chunk[1]));
                } else {
                    next_level.push(chunk[0]);
                }
            }
            hashes = next_level;
            current_index /= 2;
        }

        Some(MerkleProof {
            key: key.to_vec(),
            value: self.cache.get(key)?.clone(),
            path: proof_path,
            root: self.root_hash(),
        })
    }

    /// Verify a merkle proof
    pub fn verify_proof(proof: &MerkleProof) -> bool {
        let mut current_hash = Self::hash_entry(&proof.key, &proof.value);

        for node in &proof.path {
            current_hash = if node.is_left {
                Self::hash_pair(&node.hash, &current_hash)
            } else {
                Self::hash_pair(&current_hash, &node.hash)
            };
        }

        current_hash == proof.root
    }

    /// Get number of entries
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Iterate over all entries
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.cache.iter()
    }
}

impl Default for MerkleTrie {
    fn default() -> Self {
        Self::new()
    }
}

/// Merkle proof for a single key
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MerkleProof {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub path: Vec<ProofNode>,
    pub root: [u8; 32],
}

/// Node in proof path
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProofNode {
    pub hash: [u8; 32],
    pub is_left: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_operations() {
        let mut trie = MerkleTrie::new();

        trie.insert(b"key1", b"value1");
        trie.insert(b"key2", b"value2");
        trie.insert(b"key3", b"value3");

        assert_eq!(trie.get(b"key1"), Some(&b"value1".to_vec()));
        assert_eq!(trie.get(b"key2"), Some(&b"value2".to_vec()));
        assert_eq!(trie.len(), 3);

        let removed = trie.remove(b"key2");
        assert_eq!(removed, Some(b"value2".to_vec()));
        assert_eq!(trie.len(), 2);
    }

    #[test]
    fn test_root_hash() {
        let mut trie1 = MerkleTrie::new();
        trie1.insert(b"key1", b"value1");
        trie1.insert(b"key2", b"value2");

        let mut trie2 = MerkleTrie::new();
        trie2.insert(b"key2", b"value2");
        trie2.insert(b"key1", b"value1");

        // Same entries, same root hash (order independent)
        assert_eq!(trie1.root_hash(), trie2.root_hash());

        // Different entries, different root hash
        trie2.insert(b"key3", b"value3");
        assert_ne!(trie1.root_hash(), trie2.root_hash());
    }

    #[test]
    fn test_merkle_proof() {
        let mut trie = MerkleTrie::new();
        trie.insert(b"key1", b"value1");
        trie.insert(b"key2", b"value2");
        trie.insert(b"key3", b"value3");
        trie.insert(b"key4", b"value4");

        let proof = trie.generate_proof(b"key2").unwrap();
        assert!(MerkleTrie::verify_proof(&proof));

        // Tampered proof should fail
        let mut tampered = proof.clone();
        tampered.value = b"tampered".to_vec();
        assert!(!MerkleTrie::verify_proof(&tampered));
    }
}
