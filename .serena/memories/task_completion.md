No CI config, formatter config, or lint config files exist yet. Until project-specific tooling is
added, treat a task as done when:
- `cargo build` succeeds
- `cargo test` passes (once tests exist)
- `cargo clippy` adds no new warnings in the touched code (the crate has pre-existing ones)
- Do NOT run `cargo fmt` on the crate: the codebase is not rustfmt-formatted (long lines are
  normal), and `cargo fmt` rewrites ~20 unrelated files (found the hard way on the game-cursor
  branch, 2026-10-23). Format new files with `rustfmt <file>` only, if at all.

Re-check this memory once CI/CONTRIBUTING conventions are actually established — this is a
placeholder inferred from Rust defaults, not a confirmed project rule.
