## What & why

<!-- What does this change, and why? -->

## Verification

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace` clean (for the crates you touched)
- [ ] `cargo run --release -p agent --example board_harness` reports **0 copper DRC faults** (for engine changes)
- [ ] Determinism preserved (no wall-clock / RNG in layout)

## Notes
