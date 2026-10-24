#!/bin/sh
# Prints, for each check named, its verdict, state counts, time, the loop
# point of a liveness counterexample and the steps of its trace.
cd "$(dirname "$0")/.." || exit 2
for name in "$@"; do
  echo "## $name"
  grep -E "No violation|Found an issue|TLC encountered|distinct states found, 0 states left|depth of the complete|Back to state|Stuttering|Finished in" "out/$name.txt" | sed 's/ line [0-9].*//' | tail -6
  if [ -f "out/$name.trace" ] && [ "out/$name.trace" -nt "out/$name.qnt" ]; then
    grep -E "^ +[0-9]+ <" "out/$name.trace" | sed 's/ line [0-9].*//'
  fi
done
