#!/bin/sh
# Every inductive check of store.qnt (README, "Inductive invariants").
#   tools/inductive-all.sh [proof|bugs|iters|drops|sizes]   (default: proof and bugs)
cd "$(dirname "$0")" || exit 2
what=${1:-default}
if [ "$what" = default ] || [ "$what" = proof ]; then
  ./inductive.sh storeInd3 init
  ./inductive.sh storeInd3 step
  ./inductive.sh storeInd3 props
fi
if [ "$what" = default ] || [ "$what" = bugs ]; then
  # planted bugs: each must give a counterexample to induction
  ./inductive.sh storeIndBugDone step
  ./inductive.sh storeIndBugLease step
fi
if [ "$what" = sizes ]; then
  for k in 2 3 4 6; do ./inductive.sh "storeInd$k" step; done
fi
if [ "$what" = iters ]; then
  for n in 0 1 2 3 4 5 6; do ./inductive.sh storeInd2 step "Iter$n" "iter${n}Init"; done
fi
if [ "$what" = drops ]; then
  for n in 0 1 2 3 4 5; do ./inductive.sh storeInd2 step "Drop$n" "drop${n}Init"; done
fi
