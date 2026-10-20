"""Compares two snapshots of the admin app's pages (tests/admin_snapshot.rs).

    python3 -I crates/minicloud/tests/admin_snapshot_diff.py [--admin-changed] BEFORE AFTER

Each snapshot is a directory of `<name>.html` and a `MANIFEST` (name,
method, path and status, a line each). The two MANIFESTs must be the same,
and so must every page once the few things that differ between two runs of
the same tree are written as a placeholder. Prints a unified diff of
whatever is left and exits 1 if there is any; exits 0, saying how many
files it compared, if there is none.

What is normalised, and nothing else:

- the time a log line, a change, a secret or a metrics window was taken at:
  an ISO stamp `2026-10-15T01:57:49.235Z` (`text.isoTime`) or, as the pages
  write it since they show JST, `2026-10-15 10:57:49` (`jst`), and the time of
  day a log line shows beside it as `<span title="<stamp>">01:57:49.235</span>`
  (`atTime`). The run happens when it happens. The queue's stamps are fixed
  by the test and come out the same either way.
- the temporary directory the test's apps are copied to,
  `<tmp>/minicloud-test-<pid>-<n>`, which a refused app's reason and the
  history of a refused change name: it is made per process.

A run is otherwise fixed — the data, the order of every request, the metrics
rows — so a difference that is left is one the pages made.

`--admin-changed` is for comparing across a change to the admin app itself,
which is what this harness exists for and which no normalisation above
covers. An app's version is a hash of its source, and the admin's is in the
asset URLs of every page, so any edit to the admin moves 160-odd pages. With
the flag, the admin's version on the AFTER side (read from its own code page,
"What <code>v…</code> is made of") is written as the BEFORE side's, and the
pages that show the admin's own files — `apps-admin-code.html` and
`apps-admin-file-*.html`, whose sizes and text are the change — are listed
and not compared. That loses no markup: the same code pages are compared for
every other app.
"""

import difflib
import os
import re
import sys

# `text.isoTime`: always milliseconds, always UTC.
STAMP = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z"
# `jst`: the same moment as the pages write it since they show JST,
# `2026-10-15 10:57:49`, and `atTime`'s title adds ` JST`.
JST = r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}"

PATTERNS = [
    # `atTime`: the clock beside a log line, its whole stamp as the title.
    # Matched as the pair so that a bare `hh:mm:ss.mmm` anywhere else is not.
    (
        re.compile(r'<span title="' + STAMP + r'">\d{2}:\d{2}:\d{2}\.\d{3}</span>'),
        '<span title="<STAMP>"><CLOCK></span>',
    ),
    (
        re.compile(r'<span title="' + JST + r' JST">\d{2}:\d{2}:\d{2}\.\d{3}</span>'),
        '<span title="<STAMP>"><CLOCK></span>',
    ),
    (re.compile(STAMP), "<STAMP>"),
    (re.compile(JST), "<STAMP>"),
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


ADMIN_VERSION = re.compile(r"What <code>(v\d+-[0-9a-f]{8})</code> is made of")


def admin_version(side):
    found = ADMIN_VERSION.search(read(os.path.join(side, "apps-admin-code.html")))
    if not found:
        raise SystemExit(f"{side}: no admin version on apps-admin-code.html")
    return found.group(1)


def shows_admin_files(name):
    return name == "apps-admin-code.html" or name.startswith("apps-admin-file-")


def main(argv):
    args = argv[1:]
    admin_changed = "--admin-changed" in args
    args = [a for a in args if a != "--admin-changed"]
    if len(args) != 2:
        sys.stderr.write("usage: admin_snapshot_diff.py [--admin-changed] BEFORE AFTER\n")
        return 2
    before, after = args
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
    if admin_changed:
        was, now = admin_version(before), admin_version(after)
        skipped = [n for n in both if shows_admin_files(n)]
        both = [n for n in both if not shows_admin_files(n)]
        out.write(f"admin version {now} read as {was}; not compared: {', '.join(skipped)}\n")
    for name in both:
        a = normalise(read(os.path.join(before, name)))
        b = read(os.path.join(after, name))
        if admin_changed:
            b = b.replace(now, was)
        b = normalise(b)
        if a != b:
            show(name, a, b)

    if differing:
        out.write(f"{differing} of {len(both) + 1} files differ\n")
        return 1
    out.write(f"no differences: {len(both)} pages and the MANIFEST\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
