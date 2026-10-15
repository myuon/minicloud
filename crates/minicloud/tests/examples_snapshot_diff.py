"""Compares two snapshots of the example apps' pages (tests/examples_snapshot.rs).

    python3 -I crates/minicloud/tests/examples_snapshot_diff.py BEFORE AFTER

BEFORE and AFTER are either `$EXAMPLES_SNAPSHOT_DIR` as the test left it — a
directory per app, each a snapshot — or one app's directory. A snapshot is a
directory of `<name>.html` and a `MANIFEST` (name, method, path and status,
a line each). The apps on the two sides must be the same, their MANIFESTs
must be the same, and so must every page once the few things that differ
between two runs of the same tree are written as a placeholder. Prints a
unified diff of whatever is left and exits 1 if there is any; exits 0,
saying how many files it compared, if there is none.

`admin_snapshot_diff.py` is the same for the admin app; this follows it.

What is normalised is per app — a pattern one app needs is never applied to
another app's pages, so it cannot hide a difference there — and is nothing
else:

- `ledger`, `watch`: nothing. Their data is written at fixed times by the
  test and neither shows the clock.
- `algo`: the time a run took, which the app measures and shows
  (`showMicros`: `115 µs`, `76.7 ms`, `1.25 s` — the unit follows the
  number, so it moves too). Only where a page puts one: `(read in …)`,
  `… pairs in … ·`, `seed N · …</dd>` and `<dd>… · N decisions`. The same
  shapes elsewhere (the stylesheet's `1.45 s`…) are compared as they are.
- `webhooks`: what the app makes up or reads off the clock, and what the
  host it sends to answers with:
  - an event's id, `<16-digit microsecond stamp>-<8 hex>`, and an
    endpoint's id, 16 random hex digits — in the pages and in the
    MANIFEST's paths. Each is written as a placeholder numbered by where it
    first appears in its file, so that two different ids on one page stay
    two different placeholders;
  - an ISO stamp `2026-10-15T13:23:30.400Z` (created, received, resent);
  - the test host's port, `127.0.0.1:<port>`, in the resend target the
    test gives (port 1, the refused one, is fixed and kept);
  - how long a resend took, `<td>2 ms</td>`;
  - in a resend's answer as the app keeps it, the `date` header and the
    host's `x-cove-run-wall-us` and `x-cove-run-worker-us`.

No app shows a version of its own source (the admin's `?v=`), so there is no
`--admin-changed` here: a change to an app's source moves none of its pages
by itself.
"""

import difflib
import os
import re
import sys

# `showMicros` in examples/algo/pages.cove.
DURATION = r"(?:\d+ µs|\d+\.\d ms|\d+\.\d\d s)"

ALGO = [
    (re.compile(r"\(read in " + DURATION + r"\)"), "(read in <TIME>)"),
    (re.compile(r"pairs in " + DURATION + " ·"), "pairs in <TIME> ·"),
    (re.compile(r"(seed \d+ · )" + DURATION + "</dd>"), r"\1<TIME></dd>"),
    (re.compile(r"<dd>" + DURATION + r"( · [\d,]+ decision)"), r"<dd><TIME>\1"),
]

# Numbered per file by first appearance, rather than one placeholder for
# all: the event id first, since its stamp is sixteen digits too.
WEBHOOKS_IDS = [
    (re.compile(r"(?<![0-9a-f])\d{16}-[0-9a-f]{8}(?![0-9a-f])"), "EVENT"),
    (re.compile(r"(?<![0-9a-f])[0-9a-f]{16}(?![0-9a-f])"), "ENDPOINT"),
]

WEBHOOKS = [
    (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z"), "<STAMP>"),
    (re.compile(r"127\.0\.0\.1:(?!1/)\d+"), "127.0.0.1:<PORT>"),
    (re.compile(r"<td>\d+ ms</td>"), "<td><MS> ms</td>"),
    (
        re.compile(r"(<th>date</th><td><code>)[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT"),
        r"\1<HTTP-DATE>",
    ),
    (
        re.compile(r"(<th>x-cove-run-(?:wall|worker)-us</th><td><code>)\d+"),
        r"\1<US>",
    ),
]

PATTERNS = {"ledger": [], "watch": [], "algo": ALGO, "webhooks": WEBHOOKS}
NUMBERED = {"webhooks": WEBHOOKS_IDS}


def normalise(app, text):
    for pattern, kind in NUMBERED.get(app, []):
        seen = {}

        def number(found, seen=seen, kind=kind):
            return seen.setdefault(found.group(0), f"<{kind}-{len(seen) + 1}>")

        text = pattern.sub(number, text)
    for pattern, placeholder in PATTERNS[app]:
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


def is_snapshot(path):
    return os.path.isfile(os.path.join(path, "MANIFEST"))


def snapshots(side):
    """`{app: directory}` of one side: the directory itself if it is one
    app's snapshot, else every app directory in it."""
    if is_snapshot(side):
        return {os.path.basename(os.path.normpath(side)): side}
    return {
        name: os.path.join(side, name)
        for name in sorted(os.listdir(side))
        if is_snapshot(os.path.join(side, name))
    }


def main(argv):
    args = argv[1:]
    if len(args) != 2:
        sys.stderr.write("usage: examples_snapshot_diff.py BEFORE AFTER\n")
        return 2
    before, after = args
    sides = []
    for side in (before, after):
        found = snapshots(side) if os.path.isdir(side) else {}
        if not found:
            sys.stderr.write(f"{side}: no MANIFEST in it or in a directory of it, so not a snapshot\n")
            return 2
        unknown = sorted(set(found) - set(PATTERNS))
        if unknown:
            sys.stderr.write(f"{side}: not an app this compares: {', '.join(unknown)}\n")
            return 2
        sides.append(found)
    a_apps, b_apps = sides

    differing = 0
    compared = 0
    out = sys.stdout

    def show(where, name, a, b):
        nonlocal differing
        differing += 1
        out.writelines(
            difflib.unified_diff(
                lines(a),
                lines(b),
                fromfile=f"{a_apps.get(where, before)}/{name}",
                tofile=f"{b_apps.get(where, after)}/{name}",
            )
        )
        out.write("\n")

    for app in sorted(set(a_apps) - set(b_apps)):
        differing += 1
        out.write(f"only in {before}: {app}\n")
    for app in sorted(set(b_apps) - set(a_apps)):
        differing += 1
        out.write(f"only in {after}: {app}\n")

    for app in sorted(set(a_apps) & set(b_apps)):
        a_dir, b_dir = a_apps[app], b_apps[app]
        compared += 1
        a_manifest = normalise(app, read(os.path.join(a_dir, "MANIFEST")))
        b_manifest = normalise(app, read(os.path.join(b_dir, "MANIFEST")))
        if a_manifest != b_manifest:
            show(app, "MANIFEST", a_manifest, b_manifest)
        a_pages = {n for n in os.listdir(a_dir) if n.endswith(".html")}
        b_pages = {n for n in os.listdir(b_dir) if n.endswith(".html")}
        for name in sorted(a_pages - b_pages):
            differing += 1
            out.write(f"only in {a_dir}: {name}\n")
        for name in sorted(b_pages - a_pages):
            differing += 1
            out.write(f"only in {b_dir}: {name}\n")
        both = sorted(a_pages & b_pages)
        for name in both:
            compared += 1
            a = normalise(app, read(os.path.join(a_dir, name)))
            b = normalise(app, read(os.path.join(b_dir, name)))
            if a != b:
                show(app, name, a, b)
        out.write(f"{app}: {len(both)} pages and the MANIFEST\n")

    if differing:
        out.write(f"{differing} of {compared} files differ\n")
        return 1
    out.write(f"no differences: {compared} files\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
