#!/bin/sh
# Bounded TLC check of store.qnt (README, "Inductive invariants").
#
#   tools/store-tlc.sh <main> <invariant>
#
# <main> is a TLC instance of store.qnt (storeTlc, storeTlcBugDone, ...).
# The output is kept in out/tlc-<main>-<invariant>.txt; this prints the
# verdict, the state counts and the wall time. TLC's states go under
# $TLC_TMP (default ~/.cache/minicloud-tlc) and are removed after.
set -u
main=$1
inv=$2
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
name="tlc-$main-$inv"
tmp="${TLC_TMP:-$HOME/.cache/minicloud-tlc}/$name"
rm -rf "$tmp"; mkdir -p "$tmp"
start=$(date +%s)
(cd out && TMPDIR="$tmp" npx quint verify --backend=tlc --tlc-config=../tlc-all.json \
  --main="$main" --invariant="$inv" ../store.qnt) > "out/$name.txt" 2>&1
status=$?
end=$(date +%s)
rm -rf "$tmp" out/states out/_apalache-out
echo "== $name: exit $status, $((end - start)) s"
grep -E "distinct states found, [0-9]+ states left|depth of the complete|No violation|Found an issue|error:" "out/$name.txt"
exit $status
