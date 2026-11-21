//! End-to-end tests, behind the `e2e` feature, so a plain `cargo test` and the coverage gate never
//! run them. M1 is `tests/e2e.rs` (see `docs/milestones/m1.md`); M2 is `tests/m2.rs` and the
//! browser suite `tests/ui.rs` (see `docs/milestones/m2.md`). Shared helpers are in
//! `tests/common/mod.rs`.
