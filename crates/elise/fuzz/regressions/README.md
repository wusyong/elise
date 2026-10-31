# Regressions

Inputs the fuzzer found failing, minimized (`cargo fuzz tmin`), each named
after what it broke. `cargo test -p elise` replays every file here
(`tests/it/fuzz.rs`), Miri too. Add one with the fix it found.
