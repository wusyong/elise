//! What the safe API refuses to compile: each `ui/*.rs` and the error it
//! gets (`ui/*.stderr`). Regenerate them after a toolchain upgrade with
//! `TRYBUILD=overwrite cargo test -p elise --test compile_fail`, and review
//! the diff. Not under Miri, which doesn't build crates.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "tests"
)]

#[test]
fn compile_fail() {
    if cfg!(miri) {
        return;
    }
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
