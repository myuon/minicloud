#!/usr/bin/env python3
"""Summarise a TLC counterexample printed by `quint verify --backend=tlc`.

Reads the output on stdin and prints one line per step: the action, then
the variables that changed, with the module prefix taken off. Only the
standard library; run it with `python3 -I`.
"""
import re
import sys


def states(text):
    blocks = re.split(r"^State (\d+): ", text, flags=re.M)
    out = []
    for k in range(1, len(blocks), 2):
        body = blocks[k + 1]
        head, _, rest = body.partition("\n")
        # stop at TLC's footer
        rest = re.split(r"^\d+ states generated|^Finished|^Error|^Warning", rest, flags=re.M)[0]
        vals = {}
        for m in re.finditer(r"^/\\ (\w+) = (.*?)(?=^/\\ |\Z)", rest, flags=re.M | re.S):
            name = re.sub(r"^\w+?_queue_", "", m.group(1))
            vals[name] = " ".join(m.group(2).split())
        act = re.sub(r"<\w+?_queue_", "<", head.strip())
        act = re.sub(r" line \d+, col \d+ to line \d+, col \d+ of module \w+", "", act)
        out.append((int(blocks[k]), act, vals))
    return out


def main():
    text = sys.stdin.read()
    prev = {}
    for n, act, vals in states(text):
        changed = {k: v for k, v in vals.items() if prev.get(k) != v}
        print(f"{n:3d} {act}")
        if n > 1:
            for k, v in changed.items():
                print(f"      {k} = {v}")
        prev = vals


if __name__ == "__main__":
    main()
