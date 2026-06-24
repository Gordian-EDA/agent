//! Content-derived identifiers and hashing shared across the PCB crates.
//!
//! Emitters derive UUIDs and palette entries from stable content (never random)
//! so the same input re-emits byte-identically. Each caller keeps its own fixed
//! `namespace` constant — passing it here keeps every emitted UUID bit-identical
//! to the hand-rolled copies these replace.

/// Content-derived UUIDv5 under `namespace`, as the canonical hyphenated lowercase
/// form (36 chars). The same `(namespace, key)` always yields the same UUID.
pub fn uuid_v5(namespace: uuid::Uuid, key: &[u8]) -> String {
    uuid::Uuid::new_v5(&namespace, key)
        .as_hyphenated()
        .to_string()
}

/// 64-bit FNV-1a hash of `bytes`. Deterministic, fast, non-cryptographic — used
/// for stable colour/palette derivation, not security.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v5_is_canonical_and_stable() {
        let ns = uuid::Uuid::from_u128(0x5c1a_7d4e_3f62_5b89_a0d1_2e3f_4a5b_6c7d);
        let a = uuid_v5(ns, b"segment:1:0:0:1:1:F.Cu");
        assert_eq!(a.len(), 36);
        assert_eq!(a, uuid_v5(ns, b"segment:1:0:0:1:1:F.Cu"));
        assert_ne!(a, uuid_v5(ns, b"segment:2:0:0:1:1:F.Cu"));
    }

    #[test]
    fn fnv1a_matches_reference() {
        // FNV-1a("") is the offset basis; one byte folds in deterministically.
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"GND"), fnv1a(b"GND"));
        assert_ne!(fnv1a(b"GND"), fnv1a(b"VCC"));
    }
}
