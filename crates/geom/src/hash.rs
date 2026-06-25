//! Deterministic, non-cryptographic hashing primitives.

/// UUIDv5 of `key` under `namespace`, in canonical hyphenated lowercase form.
///
/// Deterministic across machines and process runs — the basis for every
/// content-derived identifier we emit (see [`crate::stable_uuid`]).
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
        let ns = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
        let a = uuid_v5(ns, b"segment:1:0:0:1:1:F.Cu");
        assert_eq!(a.len(), 36);
        assert_eq!(a, uuid_v5(ns, b"segment:1:0:0:1:1:F.Cu"));
        assert_ne!(a, uuid_v5(ns, b"segment:2:0:0:1:1:F.Cu"));
    }

    #[test]
    fn fnv1a_matches_reference() {
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"GND"), fnv1a(b"GND"));
        assert_ne!(fnv1a(b"GND"), fnv1a(b"VCC"));
    }
}
