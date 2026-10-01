#!/usr/bin/env python3
"""TSan report filter: fail only on races involving workspace frames.

Prebuilt std is not instrumented (see the CI job notes), so std-internal
synchronisation (e.g. the libtest harness's mpmc channels) produces
false-positive races whose stacks are entirely `/rustc/.../library/*`.
This filter splits the log into ThreadSanitizer report blocks and fails
only when a block contains a frame compiled from the workspace
(checkout-relative source paths like `engine/src/...`), i.e. a race we
can and must fix.

Usage: tsan-filter.py <log-file>
"""

import sys

def actionable(block: str) -> bool:
    for line in block.splitlines():
        line = line.strip()
        if not line.startswith("#"):
            continue
        # Frame lines look like: `#2 fn path (binary+0x..) (BuildId: ...)`.
        parts = line.split()
        if len(parts) < 3:
            continue
        path = parts[2]
        if path == "??:?":
            continue
        if path.startswith("/rustc/") or path.startswith("/library/"):
            continue  # std / compiler internals
        if path.startswith("/home/runner/work/") or "/src/" in path or path.startswith("./"):
            return True  # a workspace frame
    return False

def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    log = open(sys.argv[1], encoding="utf-8", errors="replace").read()
    blocks = [b for b in log.split("==================") if "WARNING: ThreadSanitizer" in b]
    bad = [b for b in blocks if actionable(b)]
    std_only = len(blocks) - len(bad)
    print(f"tsan-filter: {len(blocks)} race report(s); "
          f"{len(bad)} actionable (workspace frames), {std_only} std-internal.")
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
