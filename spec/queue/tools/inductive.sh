#!/bin/sh
# Inductive checks of store.qnt with Apalache (README, "Inductive invariants").
#
#   tools/inductive.sh <main> <phase> [<invariant>] [<init>]
#
# <main> is a module of store.qnt (storeInd3, ...). <phase>:
#   init   Init => <invariant>                 (default IndInv; any number of steps is not asked)
#   step   <invariant> /\ step => <invariant>' (from <init>, default indInit: any state
#          of at most K rows satisfying IndInv)
#   props  <init> => <invariant>               (default Props)
# Quint's own --inductive-invariant cannot be used: it takes the invariant as
# the initial predicate, which needs it to give every variable a finite
# shape (README). So each phase is a plain `quint verify` with its own init.
#
# The output is kept in out/ind-<main>-<phase>-<invariant>.txt; this prints
# the verdict and the wall time. Apalache's working files go under out/ and
# are removed after.
set -u
main=$1
phase=$2
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
case $phase in
  init) inv=${3:-IndInv}; init=${4:-init}; steps=0 ;;
  step) inv=${3:-IndInv}; init=${4:-indInit}; steps=1 ;;
  props) inv=${3:-Props}; init=${4:-indInit}; steps=0 ;;
  *) echo "unknown phase $phase" >&2; exit 2 ;;
esac
name="ind-$main-$phase-$inv"
start=$(date +%s)
(cd out && npx quint verify --main="$main" --init="$init" --invariant="$inv" \
  --max-steps="$steps" ../store.qnt) > "out/$name.txt" 2>&1
status=$?
end=$(date +%s)
rm -rf out/_apalache-out _apalache-out
verdict=$(grep -E "No violation found|Found an issue|error:" "out/$name.txt" | head -1)
echo "== $name: exit $status, $((end - start)) s: $verdict"
exit $status
