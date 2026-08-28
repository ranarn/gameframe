No CI config, formatter config, or lint config files exist yet. Until project-specific tooling is
added, treat a task as done when:
- `cargo build` succeeds
- `cargo test` passes (once tests exist)
- `cargo fmt` and `cargo clippy` are clean

Re-check this memory once CI/CONTRIBUTING conventions are actually established — this is a
placeholder inferred from Rust defaults, not a confirmed project rule.
