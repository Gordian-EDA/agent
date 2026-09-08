# Contributing to Gordian

Build with `cargo build --release`; a change must keep `cargo test --workspace --quiet`,
`cargo clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --all -- --check` clean, and
must not regress a clean KiCad ERC/DRC on the quality harness (`python3 quality/run.py`, see
README). Requires KiCad 10 (only) and Java 25 for the bundled Freerouting, as in the README's
Quick start.
