#!/bin/sh
# Runs every safety check in turn and appends each summary to out/safety.log.
# `bfs1` runs (one TLC worker) give shortest counterexamples; `all` runs use
# every core. Each run is cut off after 20 minutes (`timeout 1200`).
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
log=out/safety.log
only=${1:-.}
[ "$only" = . ] && : > "$log"
run() { # name config tlc invariants
  echo "$1" | grep -qE "$only" || return 0
  timeout 1200 tools/check.sh "$1" "$2" --tlc-config="tlc-$3.json" --invariant="$4,NoOverflow" >> "$log" 2>&1
  echo "exit $?" >> "$log"
}
run s1_impl         S1_IMPL          all  S1
run s1_fixed        S1_FIXED         all  S1
run s2_impl         STORE_IMPL       all  S2
run s2_fixed        STORE_FIXED      all  S2
run s3_impl         S3_IMPL          all  S3
run s3_fixed        S3_FIXED         all  S3
run s4_impl         STORE_IMPL       all  S4
run s4_fixed        STORE_FIXED      all  S4
run s5_impl         S5_IMPL          bfs1 S5
run s5_race         S5_RACE          bfs1 S5
run s5_f1           S5_F1            bfs1 S5
run s5_fixed        S5_FIXED         all  S5
run s5strict_fixed  S5_FIXED         bfs1 S5strict
run s6_impl         S6_IMPL          bfs1 S6
run s6_1_impl       S6_IMPL          all  S6_1
run s6_2_impl       S6_IMPL          all  S6_2
run s6_3_nowf       S6_NOWF          bfs1 S6_3
run s6_3_fix        S6_FIX           bfs1 S6_3
run s6_3_fixed      S6_FIXED         bfs1 S6_3
run s6_3_fixed_norace S6_FIXED_NORACE bfs1 S6_3
run s6_12_fixed     S6_FIXED         all  S6_1,S6_2
run s7_impl         S7_IMPL          bfs1 S7
run s7_f3           S7_F3            bfs1 S7
run s7_fix_f3       S7_FIX_F3        bfs1 S7
run s7_fix_f3_atomic S7_FIX_F3_ATOMIC bfs1 S7
run s7_fix_f3g      S7_FIX_F3G       all  S7
run s7_fixed        S7_FIXED         all  S7
run s8_impl         S8_IMPL          all  S8
run s8_fixed        S8_FIXED         all  S8
run s9_impl         S9_IMPL          all  S9
run s9_fixed        S9_FIXED         all  S9
run s9_pre167       S9_PRE167        bfs1 S9
run s10_impl        S10_IMPL         bfs1 S10
run s10_norevive    S10_NOREVIVE     bfs1 S10
run s10_fix         S10_FIX          bfs1 S10
run s10_fix_noold   S10_FIX_NOOLD    all  S10
run l3a_impl        L3A_IMPL         all  L3a
run l3a_fixed       L3A_FIXED        all  L3a
