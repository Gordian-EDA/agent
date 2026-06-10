//! Deterministic, content-derived identifiers.
//!
//! Spec §5.1 requires byte-identical output for the same `Design`, so every
//! UUID we emit is derived from stable content via UUIDv5 — never random.

/// Fixed project namespace UUID for auto-pcb schematic identifiers
/// (`6f6a4e2c-8b1d-5a3f-9c0e-1d2b3a4c5d6e`).
///
/// Generated once and hardcoded so that `stable_uuid` is reproducible across
/// machines and process runs. Do not change this value: doing so would alter
/// every emitted UUID and break reconciliation against existing schematics.
/// Expressed as a `u128` literal to stay independent of any `uuid` macro feature.
const NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6f6a_4e2c_8b1d_5a3f_9c0e_1d2b_3a4c_5d6e);

/// Content-derived UUID for an entity of the given `kind` identified by `key`.
///
/// Returns the canonical hyphenated lowercase form (36 chars). Deterministic:
/// the same `(kind, key)` always yields the same UUID; differing inputs yield
/// (with overwhelming probability) differing UUIDs.
pub fn stable_uuid(kind: &str, key: &str) -> String {
    let name = format!("{kind}:{key}");
    uuid::Uuid::new_v5(&NAMESPACE, name.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_deterministic_and_keyed() {
        let a = stable_uuid("symbol", "U1");
        let b = stable_uuid("symbol", "U1");
        let c = stable_uuid("symbol", "U2");
        assert_eq!(a, b); // same key -> same uuid (byte-identical re-emit)
        assert_ne!(a, c); // different key -> different uuid
        assert_eq!(a.len(), 36); // canonical hyphenated form
    }

    #[test]
    fn uuid_is_canonical_lowercase_hyphenated() {
        let u = stable_uuid("net", "GND");
        assert_eq!(u, u.to_lowercase());
        assert_eq!(u.matches('-').count(), 4);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{u}");
    }

    #[test]
    fn kind_namespacing_separates_collisions() {
        // Same key, different kind must not collide.
        assert_ne!(stable_uuid("symbol", "X"), stable_uuid("net", "X"));
    }
}
