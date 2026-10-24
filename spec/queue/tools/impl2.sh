#!/bin/sh
# Every check of the model of fix/queue-contract (IMPL2), without and with
# a host stall, appended to out/impl2.log (`tools/impl2.sh <regex>` runs
# only matching names). Each run may take up to 60 minutes. A failing
# safety check is rerun with one worker (`<name>_bfs1`) for the shortest
# trace.
cd "$(dirname "$0")/.." || exit 2
mkdir -p out
log=out/impl2.log
only=${1:-.}
[ "$only" = . ] && : > "$log"
safety() { # name config invariants
  echo "$1" | grep -qE "$only" || return 0
  timeout 3600 tools/check.sh "$1" "$2" --tlc-config=tlc-all.json --invariant="$3,NoOverflow" >> "$log" 2>&1
  status=$?
  echo "exit $status" >> "$log"
  if grep -q "Found an issue" "out/$1.txt"; then
    timeout 3600 tools/check.sh "$1_bfs1" "$2" --tlc-config=tlc-bfs1.json --invariant="$3,NoOverflow" >> "$log" 2>&1
    echo "exit $?" >> "$log"
  fi
}
liveness() { # name config temporal
  echo "$1" | grep -qE "$only" || return 0
  timeout 3600 tools/check.sh "$1" "$2" --tlc-config=tlc-all.json --invariant=NoOverflow --temporal="$3" >> "$log" 2>&1
  echo "exit $?" >> "$log"
}
for st in "" _STALL; do
  sfx=$(echo "$st" | tr 'A-Z' 'a-z')
  safety i2_s5$sfx    I2_S5$st    S5
  safety i2_safe$sfx  I2_SAFE$st  S1,S2,S4,S8
  safety i2_s3$sfx    I2_S3$st    S3
  safety i2_s6$sfx    I2_S6$st    S6
  safety i2_s7$sfx    I2_S7$st    S7
  safety i2_s9$sfx    I2_S9$st    S9
  safety i2_s10$sfx   I2_S10$st   S10
  safety i2_s10u$sfx  I2_S10$st   S10undecl
  safety i2_l3a$sfx   I2_L3A$st   L3a
done
liveness i2_l1             I2_LIVE              L1_fixed
liveness i2_l1_nofair      I2_LIVE              L1_fixed_nofair
liveness i2_l1_noE7        I2_LIVE              L1_fixed_noE7
liveness i2_l1_stall       I2_LIVE_STALL        L1_fixed
liveness i2_l1_2q          I2_LIVE2             L1_fixed
liveness i2_l1_2q_stall    I2_LIVE2_STALL       L1_fixed
liveness i2_l2             I2_LIVE              L2_fixed
liveness i2_l2_stall       I2_LIVE_STALL        L2_fixed
liveness i2_l2_2q          I2_LIVE2             L2_fixed
liveness i2_l2_2q_stall    I2_LIVE2_STALL       L2_fixed
liveness i2_l3b            I2_LIVE_L3B          L3b_fixed
liveness i2_l3b_nocrash    I2_LIVE_L3B_NOCRASH  L3b_fixed
liveness i2_l3b_nocrash_stall I2_LIVE_L3B_NOCRASH_STALL L3b_fixed
liveness i2_l3b_keep          I2_LIVE_L3B_KEEP              L3b_fixed
liveness i2_l3b_keep_nocrash  I2_LIVE_L3B_KEEP_NOCRASH      L3b_fixed
liveness i2_l3b_keep_nocrash_stall I2_LIVE_L3B_KEEP_NOCRASH_STALL L3b_fixed
# larger than the first round could finish: two ids, every fault
safety i2_x_s5_2id   I2_S5_2ID   S5
safety i2_x_safe_2id I2_SAFE_2ID S1,S2,S4,S8
safety i2_x_s6_2id   I2_S6_2ID   S6
safety i2_x_s7_2id   I2_S7_2ID   S7
safety i2_x_l3a_2id  I2_L3A_2ID  L3a
# S10 with a stall on B10 did not finish; on B2
safety i2_s10b2       I2_S10_B2       S10
safety i2_s10b2_stall I2_S10_B2_STALL S10
# liveness with every fault at most once
liveness i2_l1_full          I2_LIVE_FULL                    L1_fixed
liveness i2_l2_full          I2_LIVE_FULL                    L2_fixed
liveness i2_l3b_full_keep_nocrash I2_LIVE_FULL_L3B_KEEP_NOCRASH L3b_fixed
