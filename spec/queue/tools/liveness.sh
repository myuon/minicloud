#!/bin/sh
# Runs every liveness check in turn and appends each summary to
# out/liveness.log. Each run checks NoOverflow too, and is cut off after
# 20 minutes.
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
log=out/liveness.log
only=${1:-.}
[ "$only" = . ] && : > "$log"
run() { # name config temporal
  echo "$1" | grep -qE "$only" || return 0
  timeout 1200 tools/check.sh "$1" "$2" --tlc-config=tlc-all.json --invariant=NoOverflow --temporal="$3" >> "$log" 2>&1
  echo "exit $?" >> "$log"
}
run l1_impl            LIVE                    L1_live
run l1_nofair          LIVE                    L1_nofair
run l1_noE1            LIVE                    L1_noE1
run l1_fix_s6          LIVE_FIX_S6             L1_live
run l1_fix_s6_e7       LIVE_FIX_S6             L1_E7
run l1_fixed           LIVE_FIXED              L1_live
run l1_fixed_e7        LIVE_FIXED              L1_E7
run l1_2q_impl         LIVE2                   L1_live
run l1_2q_fixed_e7     LIVE2_FIXED             L1_E7
run l2_impl            LIVE2                   L2_live
run l2_fix_l2          LIVE2_FIX_L2            L2_live
run l2_fixed           LIVE2_FIXED             L2_live
run l3b_impl           LIVE_L3B                L3b_live
run l3b_fixed_e7       LIVE_L3B_FIXED          L3b_E7
run l3b_fixed_e7_nocrash LIVE_L3B_FIXED_NOCRASH L3b_E7
