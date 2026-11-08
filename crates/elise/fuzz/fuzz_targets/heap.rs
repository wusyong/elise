//! The heap's operations against a model that collects nothing: the
//! operations and the model are the integration tests' (`tests/it/ops.rs`),
//! which also replay `regressions/`.

#![no_main]

use libfuzzer_sys::fuzz_target;

// The encoder is for seeds, which machopo's `gc_bench` makes.
#[path = "../../tests/it/ops.rs"]
#[allow(dead_code)]
mod ops;

fuzz_target!(|data: &[u8]| ops::run(data));
