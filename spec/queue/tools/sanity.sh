#!/bin/sh
# The backend's sanity checks: each must end the way it says.
cd "$(dirname "$0")/.." || exit 2
fail=0
expect() { # holds|fails description command...
  want=$1; what=$2; shift 2
  "$@" > /dev/null 2>&1
  got=$?
  if [ "$want" = holds ] && [ $got -eq 0 ]; then echo "ok   $what: holds"
  elif [ "$want" = fails ] && [ $got -ne 0 ]; then echo "ok   $what: fails"
  else echo "BAD  $what: expected it to $want (exit $got)"; fail=1; fi
}
tlc="npx quint verify --backend=tlc"
expect fails "toy: false invariant x <= 2" $tlc --invariant=inv_false sanity/toy.qnt
expect holds "toy: true invariant x <= 3" $tlc --invariant=inv_true sanity/toy.qnt
expect fails "toy: eventually x == 3 without fairness" $tlc --temporal=reach3 sanity/toy.qnt
expect holds "toy: eventually x == 3 with weak fairness" $tlc --temporal=reach3Fair sanity/toy.qnt
expect fails "toy: eventually x == 4 with weak fairness" $tlc --temporal=reach4Fair sanity/toy.qnt
expect fails "model: S9 with lease taking every queue (before #167)" \
  tools/check.sh sanity_pre167 S9_PRE167 --tlc-config=tlc-bfs1.json --invariant=S9,NoOverflow
expect fails "model: L1 without fairness" \
  tools/check.sh sanity_l1_nofair LIVE --tlc-config=tlc-all.json --invariant=NoOverflow --temporal=L1_nofair
expect holds "model: L1 with fairness" \
  tools/check.sh sanity_l1_fair LIVE --tlc-config=tlc-all.json --invariant=NoOverflow --temporal=L1_live
exit $fail
