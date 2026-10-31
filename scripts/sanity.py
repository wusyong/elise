#!/usr/bin/env python3
"""Quick checks of the repo's rules, that read the sources and build nothing.

    python scripts/sanity.py tests     # each crate's integration tests are one binary, `tests/it/`
    python scripts/sanity.py numerics  # integer arithmetic and casts say what overflow does

None takes arguments; each prints what breaks its rule, file and line, and
what to do. Each command's rules are below, and in `sanity.py <command>
-h`. The test runs `cargo test` leaves out are in `test.py`.

Standard library only. Every command exits non-zero on failure.
"""

from __future__ import annotations

import argparse
import inspect
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# --- Tests --------------------------------------------------------------------

# Integration tests that are binaries of their own, and why. The others are
# modules of their crate's one test binary, `tests/it/`: cargo makes each
# file right under `tests/` a binary, and a change below links every one.
# The ones with `harness = false` in their crate's Cargo.toml (a `main` of
# their own) need no entry.
OWN_TEST_BINARIES = {
    "crates/elise/tests/compile_fail.rs": "trybuild, not libtest",
}


def cmd_tests(args: argparse.Namespace) -> int:
    """Each crate's integration tests are one binary, `tests/it/` (each file
    a module of `tests/it/main.rs`), so that a change below links it once,
    not once per file. A file right under `tests/` is a binary of its own:
    only one with `harness = false` in the crate's Cargo.toml, or one in
    `OWN_TEST_BINARIES` in this script with its reason (not libtest, as
    trybuild). And a file in `tests/it/` that `main.rs` doesn't declare is
    never built, so its tests never run."""
    problems = []
    for tests in sorted((ROOT / "crates").glob("*/tests")):
        crate = tests.parent.name
        manifest = (tests.parent / "Cargo.toml").read_text(encoding="utf-8")
        harnessless = set(re.findall(r'\[\[test\]\]\s*\nname\s*=\s*"([^"]+)"\s*\nharness\s*=\s*false', manifest))
        for path in sorted(tests.glob("*.rs")):
            rel = path.relative_to(ROOT).as_posix()
            if path.stem not in harnessless and rel not in OWN_TEST_BINARIES:
                problems.append(
                    f"{rel}: a test binary of its own: make it a module of crates/{crate}/tests/it/ "
                    "(or, if it can't share a process, list it in OWN_TEST_BINARIES with why)"
                )
        main = tests / "it" / "main.rs"
        if main.exists():
            declared = set(re.findall(r"^\s*(?:pub\s+)?mod\s+(\w+)\s*;", main.read_text(encoding="utf-8"), re.M))
            for path in sorted((tests / "it").glob("*.rs")):
                if path.stem != "main" and path.stem not in declared:
                    rel = path.relative_to(ROOT).as_posix()
                    problems.append(f"{rel}: not a module of tests/it/main.rs, so never built: add `mod {path.stem};`")
    for rel in sorted(OWN_TEST_BINARIES):
        if not (ROOT / rel).exists():
            problems.append(f"{rel}: in OWN_TEST_BINARIES but not there any more: take it off")

    for problem in problems:
        print(problem)
    if problems:
        return 1
    print("integration tests are one binary per crate (but for the listed ones)")
    return 0


# --- Numerics -------------------------------------------------------------------

# The lints that make integer arithmetic and casts say what overflow does
# (the rules are in docs/contributing.md).
NUMERIC_LINTS = (
    "arithmetic_side_effects",
    "cast_possible_truncation",
    "cast_sign_loss",
    "cast_possible_wrap",
)
TESTS_REASON = "tests"

ATTRIBUTE_START = re.compile(r"#(!?)\[")
LINT_LEVEL = re.compile(r"\b(allow|expect)\s*\(")
REASON = re.compile(r'\breason\s*=\s*"((?:[^"\\]|\\.)*)"', re.S)


def rust_attributes(text: str):
    """Each attribute in `text` (comments left out): (line, inner, its text
    between the brackets)."""
    code = re.sub(r"//[^\n]*", lambda m: " " * len(m.group(0)), text)
    for match in ATTRIBUTE_START.finditer(code):
        depth, i, in_string = 1, match.end(), False
        while i < len(code) and depth:
            c = code[i]
            if in_string:
                if c == "\\":
                    i += 1
                elif c == '"':
                    in_string = False
            elif c == '"':
                in_string = True
            elif c == "[":
                depth += 1
            elif c == "]":
                depth -= 1
            i += 1
        line = code.count("\n", 0, match.start()) + 1
        yield line, match.group(1) == "!", code[match.end() : i - 1]


def numeric_lints_in(body: str) -> set[str]:
    return {lint for lint in NUMERIC_LINTS if re.search(rf"\bclippy::{lint}\b", body)}


def crate_roots(crate: Path) -> list[Path]:
    """The library's and the binaries' roots."""
    roots = [crate / "src" / "lib.rs", crate / "src" / "main.rs"]
    roots += sorted((crate / "src" / "bin").glob("*.rs"))
    manifest = (crate / "Cargo.toml").read_text(encoding="utf-8")
    roots += [crate / p for p in re.findall(r'^path\s*=\s*"([^"]+\.rs)"', manifest, re.M)]
    return sorted({p for p in roots if p.exists() and "tests" not in p.relative_to(crate).parts})


def test_roots(crate: Path) -> list[Path]:
    """The integration tests' roots: each file right under `tests/`, and each
    `tests/<dir>/main.rs`."""
    tests = crate / "tests"
    return sorted(set(tests.glob("*.rs")) | set(tests.glob("*/main.rs")))


def numerics_problems(root: Path) -> list[str]:
    problems = []
    workspace = (root / "Cargo.toml").read_text(encoding="utf-8")
    lints = re.search(r"^\[workspace\.lints\.clippy\]\n(.*?)(?=^\[|\Z)", workspace, re.M | re.S)
    for lint in NUMERIC_LINTS:
        if not lints or not re.search(rf'^{lint}\s*=\s*"(warn|deny|forbid)"', lints.group(1), re.M):
            problems.append(f'Cargo.toml: [workspace.lints.clippy] must set {lint} = "warn"')

    for crate in sorted(p.parent for p in (root / "crates").glob("*/Cargo.toml")):
        # A crate without the workspace's lints has none of the above.
        manifest = crate / "Cargo.toml"
        if not re.search(r"^\[lints\]\s*\nworkspace\s*=\s*true", manifest.read_text(encoding="utf-8"), re.M):
            rel = manifest.relative_to(root).as_posix()
            problems.append(f"{rel}: must inherit the workspace lints ([lints] workspace = true)")
        roots = set(crate_roots(crate))
        tests = set(test_roots(crate))
        for path in sorted(crate.rglob("*.rs")):
            rel = path.relative_to(root).as_posix()
            if "/target/" in f"/{rel}":
                continue
            # What each root must allow: the lints for its tests, a crate's
            # own `cfg(test)` code or its integration tests.
            if path in roots:
                needed = "tests"
            elif path in tests:
                needed = "integration tests"
            else:
                needed = None
            found = False
            for line, inner, body in rust_attributes(path.read_text(encoding="utf-8")):
                lints = numeric_lints_in(body)
                if not lints:
                    continue
                level = LINT_LEVEL.search(body)
                reason = REASON.search(body)
                reason = reason.group(1) if reason else None
                where = f"{rel}:{line}"
                is_cfg_test = re.match(r"\s*cfg_attr\s*\(\s*test\s*,", body) is not None
                if level and level.group(1) == "expect" and not is_cfg_test:
                    if not reason or not reason.strip():
                        problems.append(f"{where}: #[expect] without a reason: say why it can't overflow")
                    continue
                fits = inner and reason == TESTS_REASON and (
                    (needed == "tests" and is_cfg_test) or (needed == "integration tests" and not is_cfg_test)
                )
                if not fits:
                    problems.append(
                        f"{where}: allows {', '.join(sorted(lints))}: write what overflow does "
                        '(docs/contributing.md), or #[expect(..., reason = "why it can\'t")]'
                    )
                    continue
                missing = set(NUMERIC_LINTS) - lints
                if missing:
                    problems.append(f"{where}: also allow {', '.join(sorted(missing))}")
                found = True
            if needed and not found:
                if needed == "tests":
                    want = f'#![cfg_attr(test, allow(<the lints>, reason = "{TESTS_REASON}"))]'
                else:
                    want = f'#![allow(<the lints>, reason = "{TESTS_REASON}")]'
                problems.append(f"{rel}: a {'crate' if path in roots else 'test'} root needs {want}")
    return problems


def cmd_numerics(args: argparse.Namespace) -> int:
    """Integer arithmetic and casts say what overflow does (the rules are in
    docs/contributing.md): the workspace turns on clippy's
    arithmetic_side_effects, cast_possible_truncation, cast_sign_loss and
    cast_possible_wrap, and every crate inherits the workspace's lints.
    Allowing them takes one of two lines, each at a root of its own: a
    crate root's (a library's or a binary's) `#![cfg_attr(test, allow(...,
    reason = "tests"))]` (its `cfg(test)` code), and an integration test
    root's `#![allow(..., reason = "tests")]`. Each root has its line.
    Anywhere else an exception is `#[expect(lint, reason = "...")]` with why
    it can't overflow: it fails once nothing overflows there any more. No
    crate allows them whole."""
    problems = numerics_problems(ROOT)
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print("integer overflow is spelled out")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True, metavar="command")

    def command(name: str, func, help: str) -> None:
        """A subcommand, described by its function's docstring in its `-h`."""
        sub.add_parser(
            name,
            help=help,
            description=inspect.cleandoc(func.__doc__),
            formatter_class=argparse.RawDescriptionHelpFormatter,
        ).set_defaults(func=func)

    command("tests", cmd_tests, "check that each crate's integration tests are one binary, tests/it/")
    command("numerics", cmd_numerics, "check that integer arithmetic and casts say what overflow does")

    # Every command's own help, so that `sanity.py -h` explains each rule.
    parser.epilog = "\n".join(f"--- {name} ---\n\n{p.format_help()}" for name, p in sub.choices.items())

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
