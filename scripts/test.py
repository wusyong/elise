#!/usr/bin/env python3
"""Test runs too slow, or too demanding of the machine, for `cargo test`.

    python scripts/test.py models      # the parallel code under loom and shuttle
    python scripts/test.py fuzz [--minutes N] [--seeds DIR]
                                       # the fuzzer (cargo-fuzz)
    python scripts/test.py verus       # the Verus proofs (cargo verus verify)
    python scripts/test.py miri [--partition I/N] [NEXTEST_ARG ...]
                                       # the tests under Miri (cargo-nextest)

`models` builds with `--cfg loom` and `--cfg shuttle`, each in a target
directory of its own. `fuzz` and `miri` run on the nightly the fuzzer's
rust-toolchain.toml pins. The checks of the repo's rules are in
`sanity.py`. Each command's arguments are below, and in `test.py <command>
-h`.

Standard library only. Every command exits non-zero on failure.
"""

from __future__ import annotations

import argparse
import inspect
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FUZZ = ROOT / "crates" / "elise" / "fuzz"


def nightly() -> str:
    """The nightly the fuzzer pins, which Miri runs on too."""
    text = (FUZZ / "rust-toolchain.toml").read_text(encoding="utf-8")
    return re.search(r'^channel\s*=\s*"([^"]+)"', text, re.M).group(1)


def host() -> str:
    """The machine's target triple, as the fuzzer's nightly names it."""
    out = subprocess.run(["rustc", "-vV"], cwd=FUZZ, capture_output=True, text=True, check=True).stdout
    return re.search(r"^host: (\S+)", out, re.M).group(1)


def cmd_models(args: argparse.Namespace) -> int:
    """`elise`'s parallel code under loom and shuttle, which stand in for its
    atomics, locks and threads when it is built with `--cfg loom` or `--cfg
    shuttle` (its `sync` module): loom tries every interleaving, up to a
    bound, of the marking worklist's small models (its unit tests named
    `loom_tests`); shuttle runs random schedules of whole GCs marking on a
    pool of workers (`tests/it/shuttle.rs`). Each builds in a target directory
    of its own (`target/loom`, `target/shuttle`): a change of RUSTFLAGS
    rebuilds every dependency. Release builds, for speed. Non-zero if either
    fails."""
    runs = [
        ("loom", ["cargo", "test", "-q", "-p", "elise", "--release", "--lib", "loom_tests"]),
        ("shuttle", ["cargo", "test", "-q", "-p", "elise", "--release", "--test", "it"]),
    ]
    failed = []
    for name, command in runs:
        env = dict(os.environ, RUSTFLAGS=f"--cfg {name}", CARGO_TARGET_DIR=str(ROOT / "target" / name))
        print(f"--- {name}: {' '.join(command)}", flush=True)
        if subprocess.run(command, cwd=ROOT, env=env).returncode != 0:
            failed.append(name)
    if failed:
        print(f"failed: {', '.join(failed)}")
        return 1
    return 0


def cmd_fuzz(args: argparse.Namespace) -> int:
    """`elise`'s fuzzer (`crates/elise/fuzz`, cargo-fuzz on the nightly its
    rust-toolchain.toml pins, with ASan), for N minutes (default 10):
    operations on a heap against a model that collects nothing
    (`crates/elise/tests/it/ops.rs`). It keeps what it finds in
    target/fuzz/corpus/, run after run, and with `--seeds` starts from that
    directory's inputs too (kun's `gc_bench fuzz-seeds` makes them of its
    heap recordings). On a failure it minimizes the input and prints where
    it is: put it in crates/elise/fuzz/regressions/ with the fix, and `cargo
    test -p elise` replays it. Needs `cargo install cargo-fuzz`. Built for
    this machine's target, not the one cargo-fuzz defaults to (the one it
    was built for: a prebuilt cargo-fuzz is musl's, which ASan can't link).
    Non-zero on a failure."""
    work = ROOT / "target" / "fuzz"
    corpus, artifacts = work / "corpus", work / "artifacts"
    for directory in (corpus, artifacts):
        directory.mkdir(parents=True, exist_ok=True)
    dirs = [str(corpus)]
    if args.seeds is not None:
        seeds = Path(args.seeds).resolve()
        if not seeds.is_dir():
            print(f"no such directory: {seeds}")
            return 1
        dirs.append(str(seeds))
    env = dict(os.environ, CARGO_TARGET_DIR=str(work / "build"))
    flags = [f"-max_total_time={args.minutes * 60}", "-rss_limit_mb=4096", f"-artifact_prefix={artifacts}/"]
    target = ["--target", host()]
    started = time.time()
    print(f"--- fuzzing for {args.minutes} minutes", flush=True)
    status = subprocess.run(["cargo", "fuzz", "run", *target, "heap", *dirs, "--", *flags], cwd=FUZZ, env=env).returncode
    if status == 0:
        return 0
    found = [path for path in artifacts.iterdir() if path.stat().st_mtime >= started and not path.name.startswith("minimized")]
    if not found:
        print("the fuzzer failed without leaving an input")
        return 1
    crash = max(found, key=lambda path: path.stat().st_mtime)
    print(f"--- minimizing {crash}", flush=True)
    subprocess.run(
        ["cargo", "fuzz", "tmin", *target, "heap", str(crash), "--", "-max_total_time=120", f"-exact_artifact_path={crash}.min"],
        cwd=FUZZ,
        env=env,
    )
    print(f"failing input: {crash}" + (f"\nminimized: {crash}.min" if Path(f"{crash}.min").exists() else ""))
    print("put it in crates/elise/fuzz/regressions/ with the fix: cargo test -p elise replays it")
    return 1


# The Verus release the proofs are checked with: its `vstd` is the one the
# workspace's Cargo.toml pins. Installed under ~/verus/<version>/ (the
# release's zip from github.com/verus-lang/verus, and `rustup install` of
# the toolchain its `verus --version` names), or wherever VERUS_DIR says.
VERUS_VERSION = "0.2026.10.18.7d82994"


def cmd_verus(args: argparse.Namespace) -> int:
    """`elise`'s Verus proofs (`cargo verus verify -p elise`): Verus builds the
    whole crate and proves what is inside `verus!` (the pages' bitmap
    search, `crates/elise/src/bits.rs`), which a plain build compiles with
    its specs and proofs erased. Needs Verus VERUS_VERSION (this script's),
    from VERUS_DIR or ~/verus/<version>/verus-<arch>-<os>/. About a second
    once built; the first build of the crate with Verus's toolchain takes a
    minute. Non-zero if a proof fails."""
    home = os.environ.get("VERUS_DIR")
    if home is None:
        found = sorted((Path.home() / "verus" / VERUS_VERSION).glob("verus-*"))
        home = str(found[0]) if found else None
    if home is None or not (Path(home) / "cargo-verus").exists():
        print(f"Verus {VERUS_VERSION} isn't installed: see VERUS_VERSION in this script")
        return 1
    env = dict(os.environ, PATH=f"{home}{os.pathsep}{os.environ.get('PATH', '')}")
    command = [str(Path(home) / "cargo-verus"), "verify", "-p", "elise"]
    return subprocess.run(command, cwd=ROOT / "crates" / "elise", env=env).returncode


# Stacked Borrows (Miri's default) with strict provenance; the regressions'
# files are read from disk, which isolation forbids (the tests depend on
# neither time nor randomness).
MIRIFLAGS = "-Zmiri-strict-provenance -Zmiri-disable-isolation"
# Leaks the heap's root table on purpose (a root outliving its heap), which
# Miri's leak check reports: ignored under Miri, and run on its own here
# with the leak check off.
LEAKING_TEST = "heap::a_root_outliving_its_heap_is_caught"


def cmd_miri(args: argparse.Namespace) -> int:
    """`elise`'s unit and integration tests under Miri, in parallel with
    cargo-nextest (a process a test; `cargo install cargo-nextest`), on the
    nightly the fuzzer pins (`rustup +<it> component add miri`). Then the
    one test that leaks on purpose, alone, with `-Zmiri-ignore-leaks`.
    Every test runs, even after one fails. `--partition I/N` runs only the
    I-th of N shares of the tests (nextest's `count:I/N`), for machines
    that split the run between them: the leaking test goes with the
    first. Arguments after the command go to nextest (a filter, say), and
    skip the second run. The whole run takes over an hour; the slowest
    test over half of one. If Miri reports what looks like a false
    positive, rerun with MIRIFLAGS plus `-Zmiri-tree-borrows`.
    Non-zero if a test fails."""
    toolchain = f"+{nightly()}"
    base = ["cargo", toolchain, "miri", "nextest", "run", "--no-fail-fast", "-p", "elise", "--lib", "--test", "it"]
    share = []
    if args.partition is not None:
        if not re.fullmatch(r"[1-9]\d*/[1-9]\d*", args.partition):
            print(f"--partition takes I/N, as 1/4: not {args.partition}")
            return 2
        share = ["--partition", f"count:{args.partition}"]
    env = dict(os.environ, MIRIFLAGS=MIRIFLAGS)
    print(f"--- {' '.join(base + share + args.rest)}", flush=True)
    status = subprocess.run(base + share + args.rest, cwd=ROOT, env=env).returncode
    if args.rest or (args.partition is not None and not args.partition.startswith("1/")):
        return status
    leaking = ["--run-ignored", "all", "-E", f"test(={LEAKING_TEST})"]
    env["MIRIFLAGS"] = f"{MIRIFLAGS} -Zmiri-ignore-leaks"
    print(f"--- {LEAKING_TEST}, with -Zmiri-ignore-leaks", flush=True)
    leak_status = subprocess.run(base + leaking, cwd=ROOT, env=env).returncode
    return 1 if status or leak_status else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True, metavar="command")

    def command(name: str, func, help: str) -> argparse.ArgumentParser:
        """A subcommand, described by its function's docstring in its `-h`."""
        p = sub.add_parser(
            name,
            help=help,
            description=inspect.cleandoc(func.__doc__),
            formatter_class=argparse.RawDescriptionHelpFormatter,
        )
        p.set_defaults(func=func)
        return p

    command("models", cmd_models, "the parallel code under loom and shuttle")
    fuzz = command("fuzz", cmd_fuzz, "the fuzzer (cargo-fuzz)")
    fuzz.add_argument("--minutes", type=int, default=10, metavar="N", help="how long to fuzz (default 10)")
    fuzz.add_argument("--seeds", metavar="DIR", help="a directory of inputs to start from, besides the corpus")
    command("verus", cmd_verus, "the Verus proofs (cargo verus verify)")
    miri = command("miri", cmd_miri, "the tests under Miri (cargo-nextest)")
    miri.add_argument("--partition", metavar="I/N", help="only the I-th of N shares of the tests")
    miri.add_argument("rest", nargs=argparse.REMAINDER, metavar="NEXTEST_ARG", help="passed to cargo nextest")

    # Every command's own help, so that `test.py -h` explains each one.
    parser.epilog = "\n".join(f"--- {name} ---\n\n{p.format_help()}" for name, p in sub.choices.items())

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
