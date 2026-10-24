#!/bin/sh
# Runs one check with TLC and summarises it.
#
#   tools/check.sh <name> <main module> <quint verify options...>
#
# The whole TLC output goes to out/<name>.txt; this prints the verdict,
# the state counts, the wall time and, for a counterexample, its steps.
set -u
name=$1
main=$2
shift 2
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
start=$(date +%s)
npx quint verify --backend=tlc --main="$main" "$@" checks.qnt > "out/$name.txt" 2>&1
status=$?
end=$(date +%s)
echo "== $name ($main $*): exit $status, $((end - start)) s"
grep -E "distinct states found, [0-9]+ states left|depth of the complete|No violation|Found an issue|Model checking completed|error:" "out/$name.txt" | grep -v "^Progress"
if grep -q "^State 1:" "out/$name.txt"; then
  python3 -I tools/trace.py < "out/$name.txt" > "out/$name.trace"
  grep -cE "^ +[0-9]+ <" "out/$name.trace" | sed 's/^/states in trace: /'
fi
exit $status
