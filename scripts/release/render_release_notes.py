#!/usr/bin/env python3
"""Render GitHub Release notes for a version.

Notes = that version's ``CHANGELOG.md`` section (unwrapped so GitHub's
hard-line-break rendering doesn't force a narrow column) followed by a
Docker-pull footer. CHANGELOG.md is the single source of truth, so a release
re-run regenerates identical notes and never clobbers curated text.

Usage: render_release_notes.py <version> <owner/repo>
Prints the rendered Markdown to stdout.
"""

import re
import sys


def changelog_section(version: str) -> str:
    """Return the unwrapped body of the ``## [version]`` CHANGELOG section."""
    try:
        text = open("CHANGELOG.md", encoding="utf-8").read()
    except FileNotFoundError:
        return ""
    m = re.search(
        r"^## \[" + re.escape(version) + r"\][^\n]*\n(.*?)(?=^## \[|\Z)",
        text,
        re.S | re.M,
    )
    if not m:
        return ""
    out: list[str] = []
    buf = ""
    for line in m.group(1).splitlines():
        if re.match(r"\[\d", line):  # drop link-reference definitions
            continue
        s = line.rstrip()
        if not s.strip():
            if buf:
                out.append(buf)
                buf = ""
            out.append("")
        elif s.startswith("#"):
            # a heading has no continuations — emit it standalone
            if buf:
                out.append(buf)
                buf = ""
            out.append(s.strip())
        elif re.match(r"\s*- ", s):
            # a bullet starts a new logical line that later wrapped lines join
            if buf:
                out.append(buf)
            buf = s.strip()
        else:
            # continuation of the current paragraph/bullet — join with a space
            buf = (buf + " " + s.strip()) if buf else s.strip()
    if buf:
        out.append(buf)
    # collapse runs of blank lines
    collapsed: list[str] = []
    for line in out:
        if line == "" and collapsed and collapsed[-1] == "":
            continue
        collapsed.append(line)
    return "\n".join(collapsed).strip()


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: render_release_notes.py <version> <owner/repo>", file=sys.stderr)
        return 2
    version, repo = sys.argv[1], sys.argv[2]
    body = changelog_section(version)
    minor = version.rsplit(".", 1)[0]
    footer = (
        "Multi-arch Docker image (`linux/amd64` + `linux/arm64`):\n\n"
        f"    docker pull ghcr.io/{repo}:{version}\n\n"
        f"Also tagged `{minor}` and `latest`."
    )
    print(f"{body}\n\n---\n\n{footer}" if body else footer)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
