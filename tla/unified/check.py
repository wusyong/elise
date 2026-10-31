#!/usr/bin/env python3
"""Model-check Unified.tla (elise's marking with V8's in one major GC, as
cppgc's) with TLC, as it is and with each part of it taken out (its
`Mutation`): the first must hold, and each mutation must give the
counterexample listed below, or hold where taking that part out is
harmless (say why). The runner is ../marking/check.py's.

TLC comes from the TLA+ tools' release (pinned, checked against its
SHA-256, kept in target/tla/); Java is the machine's (`java` on PATH,
$JAVA_HOME, or Homebrew's openjdk; --java to name one).
"""

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
OUT = ROOT / "target" / "tla"

TLC_VERSION = "1.7.4"
TLC_URL = f"https://github.com/tlaplus/tlaplus/releases/download/v{TLC_VERSION}/tla2tools.jar"
TLC_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"

# The model's sizes (the new heap's objects, JS objects, fields each). The
# protocol holds at (2, 2, 2) too (88.6 million states, 17 minutes on an
# Apple M4: too long for every mutation); (3, 2, 1) and (2, 3, 1) didn't
# finish in 15 minutes.
SIZES = [(2, 2, 1)]

SAFETY = ["TypeOK", "MarkingComplete", "NoDangling", "WrappedNotYoung", "ExpandosKept"]

# (mutation, invariants checked, the invariant TLC must find violated or
# None when it must hold, why).
CHECKS = [
    ("none", SAFETY, None, "the protocol"),
    ("none", ["WrappedNeverFreed"], "WrappedNeverFreed",
     "a node and its wrapper, a cycle across the heaps, are collected"),
    ("no_write_barrier", SAFETY, "MarkingComplete",
     "an overwritten field was the only path to an old object"),
    ("no_black_allocation", SAFETY, "MarkingComplete",
     "an object allocated old while marking, kept by a root added since"),
    ("no_black_promotion", SAFETY, "MarkingComplete",
     "a young object promoted while marking, never scanned nor marked"),
    ("young_wrapped_nodes", SAFETY, "WrappedNotYoung",
     "a young node wrapped: its address would change as it survives"),
    ("young_wrapped_nodes", ["TypeOK", "MarkingComplete", "NoDangling", "ExpandosKept"], "NoDangling",
     "and a scavenge, which takes nothing from wrappers, frees it"),
    ("no_wrapper_handoff", SAFETY, "MarkingComplete",
     "an old node only its wrapper holds, never marked"),
    ("no_js_edge_handoff", SAFETY, "MarkingComplete",
     "a wrapper only its node holds (its expandos with it), never marked"),
    ("no_unwrap_barrier", SAFETY, "MarkingComplete",
     "a node read through a wrapper V8 hadn't scanned, linked from a black "
     "object, the wrapper then dropped: V8 never hands the node over"),
    ("no_js_write_barrier", SAFETY, "MarkingComplete",
     "V8's side: a JS object written into a scanned one"),
    ("no_js_root_rescan", SAFETY, "MarkingComplete",
     "V8's side: a JS object script took as a root after the start"),
    ("finish_on_v8_alone", SAFETY, "MarkingComplete",
     "the pause ends with the new heap's worklist not empty"),
    ("finish_on_new_heap_alone", SAFETY, "MarkingComplete",
     "the pause ends with V8's worklist not empty"),
    ("no_reset_root", SAFETY, "NoDangling",
     "V8's scavenge drops a wrapper and its node still points to it"),
    ("drop_modified", SAFETY, "ExpandosKept",
     "V8's scavenge drops a wrapper script wrote an expando on"),
    ("drop_reachable", SAFETY, "NoDangling",
     "V8's scavenge drops a wrapper script still holds"),
]


def tlc_jar() -> Path:
    jar = OUT / f"tla2tools-{TLC_VERSION}.jar"
    if not jar.exists() or hashlib.sha256(jar.read_bytes()).hexdigest() != TLC_SHA256:
        OUT.mkdir(parents=True, exist_ok=True)
        print(f"downloading TLC {TLC_VERSION}", file=sys.stderr)
        with urllib.request.urlopen(TLC_URL) as response:
            data = response.read()
        digest = hashlib.sha256(data).hexdigest()
        if digest != TLC_SHA256:
            raise SystemExit(f"{TLC_URL}: SHA-256 {digest}, expected {TLC_SHA256}")
        jar.write_bytes(data)
    return jar


def java_binary(given: str | None) -> str:
    candidates = [given] if given else [
        shutil.which("java"),
        os.path.join(os.environ["JAVA_HOME"], "bin", "java") if os.environ.get("JAVA_HOME") else None,
        "/opt/homebrew/opt/openjdk/bin/java",
        "/usr/local/opt/openjdk/bin/java",
    ]
    for java in filter(None, candidates):
        # macOS has a /usr/bin/java that only says there is no Java.
        if os.path.exists(java) and subprocess.run([java, "-version"], capture_output=True).returncode == 0:
            return java
    raise SystemExit("no Java: install a JDK (for example `brew install openjdk`) or pass --java")


def config(size: tuple[int, int, int], mutation: str, invariants: list[str]) -> str:
    objects = ", ".join(f"o{i}" for i in range(1, size[0] + 1))
    js = ", ".join(f"j{i}" for i in range(1, size[1] + 1))
    return "\n".join([
        "SPECIFICATION Spec",
        "CONSTANTS",
        f"    Obj = {{{objects}}}",
        f"    Js = {{{js}}}",
        f"    NF = {size[2]}",
        "    Null = Null",
        "    None = None",
        f'    Mutation = "{mutation}"',
        "SYMMETRY Symmetry",
        *[f"INVARIANT {name}" for name in invariants],
        "",
    ])


def run(java: str, jar: Path, size: tuple[int, int, int], mutation: str,
        invariants: list[str]) -> dict:
    name = f"Unified-{'x'.join(map(str, size))}-{mutation}-{'-'.join(invariants)}"
    cfg = OUT / f"{name}.cfg"
    OUT.mkdir(parents=True, exist_ok=True)
    with open(cfg, "w", newline="\n") as f:
        f.write(config(size, mutation, invariants))
    # TLC looks for the config next to the spec, so both go in OUT.
    shutil.copyfile(HERE / "Unified.tla", OUT / "Unified.tla")
    start = time.monotonic()
    done = subprocess.run(
        [java, "-XX:+UseParallelGC", "-cp", str(jar), "tlc2.TLC",
         "-workers", "auto", "-deadlock", "-cleanup",
         "-metadir", str(OUT / "states" / name),
         "-config", cfg.name, "Unified.tla"],
        cwd=OUT, capture_output=True, text=True)
    seconds = time.monotonic() - start
    with open(OUT / f"{name}.out", "w", newline="\n") as f:
        f.write(done.stdout + done.stderr)
    violated = re.search(r"Invariant (\w+) is violated", done.stdout)
    # The last count is the total; the progress lines before it aren't.
    states = re.findall(r"([\d,]+) distinct states found", done.stdout)
    # TLC exits 0 when every invariant holds and 12 on a safety violation;
    # anything else is a broken model or setup.
    if done.returncode not in (0, 12) or (done.returncode == 12) != bool(violated):
        raise SystemExit(f"TLC failed on {name} (exit {done.returncode}): see {OUT / (name + '.out')}")
    return {
        "violated": violated.group(1) if violated else None,
        "states": states[-1] if states else "?",
        "seconds": seconds,
        "output": OUT / f"{name}.out",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--java", help="the java binary to run TLC with")
    parser.add_argument("--mutation", help="only this mutation's checks")
    args = parser.parse_args()

    java = java_binary(args.java)
    jar = tlc_jar()
    failed = []
    print("| Size | Mutation | Checked | Expected | TLC | Distinct states | s |")
    print("| --- | --- | --- | --- | --- | --- | --- |", flush=True)
    for mutation, invariants, expected, why in CHECKS:
        if args.mutation and mutation != args.mutation:
            continue
        for size in SIZES:
            want = expected.get(size) if isinstance(expected, dict) else expected
            result = run(java, jar, size, mutation, invariants)
            ok = result["violated"] == want
            print(f"| {'x'.join(map(str, size))} | {mutation} | {', '.join(invariants)} "
                  f"| {want or 'holds'} | {result['violated'] or 'holds'}{'' if ok else ' (WRONG)'} "
                  f"| {result['states']} | {result['seconds']:.1f} |", flush=True)
            if not ok:
                failed.append(f"{'x'.join(map(str, size))} {mutation}: expected {want or 'holds'} "
                              f"({why}), TLC: {result['violated'] or 'holds'}; see {result['output']}")
    if failed:
        raise SystemExit("\n".join(failed))


if __name__ == "__main__":
    main()
