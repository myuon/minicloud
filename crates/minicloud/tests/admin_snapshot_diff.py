"""Compares two snapshots of the admin app's pages (tests/admin_snapshot.rs).

    python3 -I crates/minicloud/tests/admin_snapshot_diff.py BEFORE AFTER

Each snapshot is a directory of `<name>.html` and a `MANIFEST` (name,
method, path and status, a line each). The two MANIFESTs must be the same,
and so must every page once the few things that differ between two runs of
the same tree are written as a placeholder. Prints a unified diff of
whatever is left and exits 1 if there is any; exits 0, saying how many
files it compared, if there is none.

What is normalised, and nothing else:

- the time a log line, a change, a secret or a metrics window was taken at:
  an ISO stamp `2026-10-15T01:57:49.235Z` (`text.isoTime`), and the time of
  day a log line shows beside it as `<span title="<stamp>">01:57:49.235</span>`
  (`atTime`). The run happens when it happens. The queue's stamps are fixed
  by the test and come out the same either way.
- the temporary directory the test's apps are copied to,
  `<tmp>/minicloud-test-<pid>-<n>`, which a refused app's reason and the
  history of a refused change name: it is made per process.

A run is otherwise fixed — the data, the order of every request, the metrics
rows — so a difference that is left is one the pages made.
"""

import difflib
import os
import re
import sys

# `text.isoTime`: always milliseconds, always UTC.
STAMP = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z"

PATTERNS = [
    # `atTime`: the clock beside a log line, its whole stamp as the title.
    # Matched as the pair so that a bare `hh:mm:ss.mmm` anywhere else is not.
    (
        re.compile(r'<span title="' + STAMP + r'">\d{2}:\d{2}:\d{2}\.\d{3}</span>'),
        '<span title="<STAMP>"><CLOCK></span>',
    ),
    (re.compile(STAMP), "<STAMP>"),
    # `std::env::temp_dir()/minicloud-test-<pid>-<n>` (tests/common).
    (
        re.compile(r"(?:/[A-Za-z0-9._-]+)*/minicloud-test-\d+-\d+"),
        "<TMP>/minicloud-test",
    ),
]


def normalise(text):
    for pattern, placeholder in PATTERNS:
        text = pattern.sub(placeholder, text)
    return text


def read(path):
    with open(path, "rb") as f:
        # Every page is UTF-8; a byte that is not is kept visible rather
        # than failing the comparison.
        return f.read().decode("utf-8", errors="backslashreplace")


def lines(text):
    # A page is mostly one long line; break it between tags so that a diff
    # points at the element that changed rather than at the whole page.
    return text.replace("><", ">\n<").splitlines(keepends=True)


def main(argv):
    if len(argv) != 3:
        sys.stderr.write("usage: admin_snapshot_diff.py BEFORE AFTER\n")
        return 2
    before, after = argv[1], argv[2]
    for side in (before, after):
        if not os.path.isfile(os.path.join(side, "MANIFEST")):
            sys.stderr.write(f"{side}: no MANIFEST, so not a snapshot\n")
            return 2
    differing = 0
    out = sys.stdout

    def show(name, a, b):
        nonlocal differing
        differing += 1
        out.writelines(
            difflib.unified_diff(
                lines(a), lines(b), fromfile=f"{before}/{name}", tofile=f"{after}/{name}"
            )
        )
        out.write("\n")

    a_manifest = read(os.path.join(before, "MANIFEST"))
    b_manifest = read(os.path.join(after, "MANIFEST"))
    if a_manifest != b_manifest:
        show("MANIFEST", a_manifest, b_manifest)

    a_pages = {n for n in os.listdir(before) if n.endswith(".html")}
    b_pages = {n for n in os.listdir(after) if n.endswith(".html")}
    for name in sorted(a_pages - b_pages):
        differing += 1
        out.write(f"only in {before}: {name}\n")
    for name in sorted(b_pages - a_pages):
        differing += 1
        out.write(f"only in {after}: {name}\n")
    both = sorted(a_pages & b_pages)
    for name in both:
        a = normalise(read(os.path.join(before, name)))
        b = normalise(read(os.path.join(after, name)))
        if a != b:
            show(name, a, b)

    if differing:
        out.write(f"{differing} of {len(both) + 1} files differ\n")
        return 1
    out.write(f"no differences: {len(both)} pages and the MANIFEST\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
