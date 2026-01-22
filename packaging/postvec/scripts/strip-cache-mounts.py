#!/usr/bin/env python3
"""Rewrite a builder Dockerfile so the classic (non-BuildKit) builder accepts it.

    strip-cache-mounts.py Dockerfile.deb /tmp/Dockerfile.nocache

The builders use `RUN --mount=type=cache,...` because a Rust release build
without a Cargo cache is unbearably slow. Those mounts need BuildKit, which
needs the `buildx` plugin. Releases always have it; a developer's machine may
not, and "you cannot build this at all" is a bad answer to a missing plugin.

Caches are explicitly not build *inputs* — they may only make a build faster —
so removing them changes what the build costs, never what it produces.
"""

import re
import sys

CACHE_MOUNT = re.compile(r"--mount=type=cache,\S+[ \t]*")
# A continuation line that held nothing but mounts is now blank; splice it out
# rather than leaving `RUN \` followed by an empty line, which is a parse error.
EMPTY_CONTINUATION = re.compile(r"\\\n[ \t]*(?=\\\n|\n)")


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    source, dest = sys.argv[1], sys.argv[2]

    text = open(source, encoding="utf-8").read()
    if "--mount=type=cache" not in text:
        open(dest, "w", encoding="utf-8").write(text)
        return 0

    text = text.replace("# syntax=docker/dockerfile:1.7\n", "")
    text = CACHE_MOUNT.sub("", text)
    while EMPTY_CONTINUATION.search(text):
        text = EMPTY_CONTINUATION.sub("", text)
    # `RUN \` immediately followed by the real command reads fine; `RUN` alone
    # on its own line does not.
    text = re.sub(r"^(RUN)[ \t]*\\\n", r"\1 ", text, flags=re.MULTILINE)

    if "--mount=type=cache" in text:
        print("cache mounts survived the rewrite", file=sys.stderr)
        return 1
    open(dest, "w", encoding="utf-8").write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
