#!/usr/bin/env python3
"""Model-check the GC's marking protocols (Satb.tla, Dijkstra.tla, and
SatbYoung.tla: the implementation's, with a young generation) with TLC,
each as it is and with each part of it taken out (its `Mutation`): the
first must hold, and each mutation must give the counterexample listed
below, or hold where taking that part out is harmless (say why).

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

# The model's sizes (objects, fields each), every check run on both: three
# objects of one field have the write barrier's classic counterexample (a
# black object, a grey one and its white child); two of two fields have the
# marker halfway through an object while the mutator moves an edge between
# its fields. Three of two fields is too large to check exhaustively (more
# than 12 million distinct states a minute, the queue still growing).
SIZES = [(3, 1), (2, 2)]

SAFETY = ["TypeOK", "MarkingComplete", "NoDangling"]
# SatbYoung's: also, no weak edge or root to a freed object.
WEAK_SAFETY = SAFETY + ["WeakSafe"]

# (spec, mutation, invariants checked, the invariant TLC must find
# violated or None when it must hold, why).
CHECKS = [
    ("Satb", "none", SAFETY, None, "the protocol"),
    ("Satb", "none", ["NothingSwept"], "NothingSwept", "the model does sweep garbage"),
    ("Satb", "no_write_barrier", SAFETY, "MarkingComplete",
     "an overwritten field was the only path to an object"),
    ("Satb", "no_black_allocation", SAFETY, "MarkingComplete",
     "a new object written into a scanned one"),
    ("Satb", "no_root_delete_barrier", SAFETY, "MarkingComplete",
     "a root deleted before the marker visits it, its object kept in a scanned one"),
    ("Satb", "no_weak_read_barrier", SAFETY, "MarkingComplete",
     "garbage the snapshot doesn't reach, read through a weak reference and kept"),
    ("SatbYoung", "none", WEAK_SAFETY, None, "the protocol"),
    ("SatbYoung", "none", ["NothingSwept"], "NothingSwept", "the model does sweep garbage"),
    ("SatbYoung", "no_write_barrier", WEAK_SAFETY, "MarkingComplete",
     "an overwritten field was the only path to an old object"),
    ("SatbYoung", "no_black_allocation", WEAK_SAFETY, "MarkingComplete",
     "an object allocated old while marking, kept by a root added since"),
    ("SatbYoung", "no_black_promotion", WEAK_SAFETY, "MarkingComplete",
     "a young object promoted while marking, never scanned nor marked"),
    ("SatbYoung", "no_scavenge_at_start", WEAK_SAFETY, "MarkingComplete",
     "a young survivor of the start the only path to an old object, which "
     "the marker never reaches (it skips the young)"),
    ("SatbYoung", "no_root_delete_barrier", WEAK_SAFETY, None,
     "harmless: every root is marked at the start, and a root added since "
     "holds what the snapshot, an allocation or a weak read gave it"),
    ("SatbYoung", "no_weak_read_barrier", WEAK_SAFETY, "MarkingComplete",
     "an object read through a weak edge, linked from a black one, never marked"),
    ("SatbYoung", "no_weak_clear", WEAK_SAFETY, "WeakSafe",
     "a weak edge to an old object swept"),
    ("SatbYoung", "no_weak_scavenge_clear", WEAK_SAFETY, "WeakSafe",
     "a weak edge to a young object a scavenge freed"),
    ("SatbYoung", "no_pair_fixpoint", WEAK_SAFETY, {(3, 1): "MarkingComplete", (2, 2): None},
     "an ephemeron discovered before its key was marked: its value never is "
     "(its holder, key and value three objects: two hold no counterexample)"),
    ("SatbYoung", "no_pair_value_barrier", WEAK_SAFETY, "MarkingComplete",
     "an ephemeron's value overwritten was the only path to an old object"),
    ("SatbYoung", "none", ["SlotsLive"], None,
     "the slot sets: an old object written an edge to a young one while "
     "marking runs is the snapshot's (marked) or allocated since (black)"),
    ("SatbYoung", "no_scavenge_at_start", ["SlotsLive"], "SlotsLive",
     "garbage in the old generation with an edge to a young survivor of the start"),
    ("SatbYoung", "no_black_allocation", ["SlotsLive"], "SlotsLive",
     "an object allocated old while marking, given an edge to a young one, "
     "then dropped: unmarked, its edge left to a scavenge"),
    ("Dijkstra", "none", SAFETY, None, "the protocol"),
    ("Dijkstra", "none", ["NothingSwept"], "NothingSwept", "the model does sweep garbage"),
    ("Dijkstra", "no_write_barrier", SAFETY, "MarkingComplete",
     "an object written into a scanned one"),
    ("Dijkstra", "no_root_rescan", SAFETY, "MarkingComplete",
     "an object made a root after the marker passed its entry"),
    ("Dijkstra", "no_black_allocation", SAFETY, None,
     "harmless for safety: a new object is marked once written into a field "
     "(the barrier) or the root table (the rescan), and raw pointers end at "
     "the safepoint; black allocation saves the marker work"),
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


def config(size: tuple[int, int], mutation: str, invariants: list[str]) -> str:
    objects = ", ".join(f"o{i}" for i in range(1, size[0] + 1))
    return "\n".join([
        "SPECIFICATION Spec",
        "CONSTANTS",
        f"    Obj = {{{objects}}}",
        f"    NF = {size[1]}",
        "    Null = Null",
        "    None = None",
        f'    Mutation = "{mutation}"',
        "SYMMETRY Symmetry",
        *[f"INVARIANT {name}" for name in invariants],
        "",
    ])


def run(java: str, jar: Path, spec: str, size: tuple[int, int], mutation: str,
        invariants: list[str]) -> dict:
    name = f"{spec}-{size[0]}x{size[1]}-{mutation}-{'-'.join(invariants)}"
    cfg = OUT / f"{name}.cfg"
    with open(cfg, "w", newline="\n") as f:
        f.write(config(size, mutation, invariants))
    # TLC looks for the config next to the spec, so both go in OUT.
    shutil.copyfile(HERE / f"{spec}.tla", OUT / f"{spec}.tla")
    start = time.monotonic()
    done = subprocess.run(
        [java, "-XX:+UseParallelGC", "-cp", str(jar), "tlc2.TLC",
         "-workers", "auto", "-deadlock", "-cleanup",
         "-metadir", str(OUT / "states" / name),
         "-config", cfg.name, f"{spec}.tla"],
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
    parser.add_argument("--spec", choices=["Satb", "SatbYoung", "Dijkstra"], help="only this spec's checks")
    args = parser.parse_args()

    java = java_binary(args.java)
    jar = tlc_jar()
    failed = []
    print("| Spec | Size | Mutation | Checked | Expected | TLC | Distinct states | s |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- |", flush=True)
    for spec, mutation, invariants, expected, why in CHECKS:
        if args.spec and spec != args.spec:
            continue
        for size in SIZES:
            # One expectation, or one for each size.
            want = expected.get(size) if isinstance(expected, dict) else expected
            result = run(java, jar, spec, size, mutation, invariants)
            ok = result["violated"] == want
            print(f"| {spec} | {size[0]}x{size[1]} | {mutation} | {', '.join(invariants)} "
                  f"| {want or 'holds'} | {result['violated'] or 'holds'}{'' if ok else ' (WRONG)'} "
                  f"| {result['states']} | {result['seconds']:.1f} |", flush=True)
            if not ok:
                failed.append(f"{spec} {size[0]}x{size[1]} {mutation}: expected {want or 'holds'} "
                              f"({why}), TLC: {result['violated'] or 'holds'}; see {result['output']}")
    if failed:
        raise SystemExit("\n".join(failed))


if __name__ == "__main__":
    main()
