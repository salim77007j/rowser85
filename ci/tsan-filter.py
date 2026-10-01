#!/usr/bin/env python3
"""TSan report filter: fail only on races in *workspace* code.

Prebuilt std is not instrumented, so TSan cannot see std's internal
synchronisation. This produces two classes of false positives:

1. Races whose access stacks are entirely std/core (e.g. the libtest
   harness's own mpmc channels).
2. `OnceLock`/lazy-init reports from dependencies (jiff's timezone db,
   etc.) where the happens-before edge lives in uninstrumented std.

A report is *actionable* only when one of its racing-access stacks
(the `Read/Write/Atomic ... of size` sections, NOT the thread-creation
stack, which naturally contains the code that spawned the thread)
contains a frame compiled from this repository — workspace-relative
paths or the CI checkout directory. Dependency crates
(`~/.cargo/registry`) and std (`/rustc/...`) are not actionable.

Usage: tsan-filter.py <log-file>
"""

import sys

def access_sections(block: str) -> list[list[str]]:
    """Frame lists of the racing accesses only (stops at 'Location is' / 'Thread T.. created by')."""
    sections: list[list[str]] = []
    current: list[str] | None = None
    for line in block.splitlines():
        stripped = line.strip()
        if " of size " in stripped and stripped.startswith(("Read", "Write", "Previous", "Atomic")):
            current = []
            sections.append(current)
        elif current is not None:
            if (
                stripped.startswith("Location is")
                or " created by " in stripped
                or stripped.startswith("Mutex")
                or stripped == ""
                or not stripped.startswith("#")
            ):
                if stripped == "" or not stripped.startswith("#"):
                    current = None if not stripped.startswith("#") else current
                    if stripped == "":
                        current = None
                continue
            current.append(stripped)
    return [s for s in sections if s]

def frame_path(frame: str) -> str | None:
    parts = frame.split()
    # `#N <function> <path> (binary+off) (BuildId: ..)` — path is parts[2].
    if len(parts) >= 3:
        return parts[2]
    return None

def workspace_frame(frame: str) -> bool:
    path = frame_path(frame)
    if path is None or path == "??:?":
        return False
    if path.startswith("/rustc/"):          # std / compiler-builtins
        return False
    if path.startswith("/home/runner/.cargo"):  # third-party dependencies
        return False
    if ".cargo/registry" in path or ".cargo\\registry" in path:
        return False
    if path.startswith("/home/runner/work/"):   # CI checkout = this repo
        return True
    # Workspace-relative paths (local runs): `engine/src/lib.rs`, `./js/...`
    if path.startswith("./") or path.startswith("../"):
        return True
    return "/src/" in path and not path.startswith("/")

def actionable(block: str) -> bool:
    for section in access_sections(block):
        if any(workspace_frame(f) for f in section):
            return True
    return False

def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    log = open(sys.argv[1], encoding="utf-8", errors="replace").read()
    blocks = [b for b in log.split("==================") if "WARNING: ThreadSanitizer" in b]
    bad = [b for b in blocks if actionable(b)]
    print(f"tsan-filter: {len(blocks)} race report(s); {len(bad)} actionable "
          f"(workspace access frames), {len(blocks) - len(bad)} std/deps-internal.")
    if bad:
        print("\nACTIONABLE RACES:\n")
        for b in bad:
            print("==================")
            print(b)
        return 1
    print("tsan-filter: OK — no races attributed to workspace code.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
