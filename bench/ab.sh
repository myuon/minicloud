#!/bin/sh
# Two builds of the host against each other, interleaved: each round starts
# a fresh host from A, loads it, stops it, then the same from B, so that
# what the machine is doing meanwhile falls on both alike.
#
#   cargo build --profile checked && cp target/checked/minicloud /tmp/a/   # on one commit
#   cargo build --profile checked && cp target/checked/minicloud /tmp/b/   # on the other
#   sh bench/ab.sh /tmp/a /tmp/b 5 > bench/results/ab-$(date +%F).txt
#
# The host as bench/perf.sh starts it (port 18180, four workers, a 2 ms
# slice, the samples' limits raised); the load is ./target/checked's
# minicloud-load unless LOAD names another. Each line of the output is one
# minicloud-load run under a `## <A|B> <arguments>` heading.
set -eu
A=$1
B=$2
ROUNDS=${3:-3}
LOAD=${LOAD:-./target/checked/minicloud-load}
PORT=18180
ADDR=127.0.0.1:$PORT
if curl -s -o /dev/null "http://$ADDR/"; then
  echo "something already answers on $ADDR" >&2
  exit 1
fi
APPS=$(mktemp -d)
trap 'rm -rf "$APPS"' EXIT
cp -R apps/* examples/* "$APPS"/
sed -i.bak "s|allow = \[.*\]|allow = [\"http://127.0.0.1:$PORT\"]|" "$APPS/proxy/app.toml"
for app in crunch slow proxy hello; do
  toml="$APPS/$app/app.toml"
  sed -i.bak -e '/^max_in_flight/d' -e '/^max_queued/d' "$toml"
  grep -q '^\[limits\]' "$toml" || printf '\n[limits]\n' >> "$toml"
  sed -i.bak 's|^\[limits\]$|[limits]\
max_in_flight = 10000\
max_queued = 10000|' "$toml"
done
run() {
  name=$1
  bin=$2
  data=$(mktemp -d)
  "$bin/minicloud" serve --apps "$APPS" --data "$data" --addr "$ADDR" --workers 4 \
    --slice 2 --no-admin --quiet 2> "$data/host.err" &
  host=$!
  until curl -s -o /dev/null "http://$ADDR/"; do sleep 0.2; done
  # Warm-up, not kept.
  "$LOAD" --addr "$ADDR" --mix hello --connections 64 --requests 20000 > /dev/null
  for args in "--mix hello --connections 64 --requests 200000" \
    "--mix hello --connections 256 --requests 200000" \
    "--mix cpu-io --connections 256 --requests 3000"; do
    echo "## $name $args"
    # shellcheck disable=SC2086 # the arguments are words
    "$LOAD" --addr "$ADDR" $args
  done
  kill "$host"
  wait "$host" 2> /dev/null || true
  rm -rf "$data"
}
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) $(uname -sm) load=$(uptime | sed 's/.*load averages*: //')"
for round in $(seq 1 "$ROUNDS"); do
  echo "# round $round"
  run A "$A"
  run B "$B"
done
