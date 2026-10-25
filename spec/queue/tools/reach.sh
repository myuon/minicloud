#!/bin/sh
# Reachability sanity for the model of fix/queue-contract (IMPL2): each
# "Never..." invariant must FAIL, which shows that the behaviour it names
# is reachable, so a property that holds is not holding vacuously.
cd "$(dirname "$0")/.." || exit 2
fail=0
for inv in NeverFail NeverOutlived NeverLowered NeverUndeclared NeverHeld NeverSecondAttempt NeverDone NeverStall; do
  tools/check.sh "reach_$inv" I2_SAFE_STALL --tlc-config=tlc-bfs1.json --invariant="$inv" > /dev/null 2>&1
  if grep -q "Found an issue" "out/reach_$inv.txt"; then
    echo "ok   $inv is reachable ($(grep -cE '^ +[0-9]+ <' "out/reach_$inv.trace") states)"
  else
    echo "BAD  $inv: not violated"; fail=1
  fi
done
exit $fail
