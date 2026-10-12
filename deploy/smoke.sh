#!/usr/bin/env bash
# Installs a release tarball into a scratch home and runs the service's own
# command line against it, as CI and the release workflow do:
#
#   deploy/smoke.sh TARBALL
#
# It runs install.sh as on the server, over a current that points at a
# release from before the rename (the bundled admin app; it refuses an
# example asked for as a bundled app), deploys the examples the deployment
# runs (webhooks, ledger, algo, hello) from the repository's examples/ as any
# app is, checks a reinstall leaves an app that is not the release's alone,
# and refuses to switch while an installed app does not check, takes ExecStart from
# cove-tools.service with /home/ioijoi moved to the scratch home, starts
# it, checks the public and admin listeners answer as deployed (apps on
# 8790, /_host/ only on 8791, Cloudflare Access on), deploys an app onto it
# from an archive on stdin and rolls it back, stops it with SIGTERM as
# systemd would, starts it again with Access off to check the token way in,
# and runs backup.sh. Linux, x86-64; ports 8790 and 8791 free.
set -euo pipefail

tarball="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
here="$(cd "$(dirname "$0")" && pwd)"
examples="$(cd "$here/../examples" && pwd)"
scratch="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$scratch"' EXIT
pid=""
export HOME="$scratch/home"
mkdir -p "$HOME"
root="$HOME/cove-tools"

fail() { echo "smoke.sh: $*" >&2; exit 1; }

# The release bundles the admin app alone; an example is not one of its apps.
if bash "$here/install.sh" --with-bundled-apps "admin webhooks" "$tarball" > "$scratch/example.log" 2>&1; then
  fail "install.sh took an example app as a bundled one"
fi
grep -q 'examples/webhooks' "$scratch/example.log" \
  || fail "install.sh did not point at examples/ for a no-longer-bundled app: $(cat "$scratch/example.log")"
[ ! -e "$root/current" ] || fail "install.sh switched although it refused a bundled app"
[ ! -e "$root/env" ] || fail "install.sh wrote the env although it refused a bundled app"
if tar -tzf "$tarball" | grep -v '/apps/admin/\|/apps/$' | grep -q '/apps/.'; then
  fail "the release packages an app that is not admin"
fi

# The server's layout before the rename: current at a cove-host release.
mkdir -p "$root/releases/0.4.0/deploy"
printf '#!/bin/sh\necho "cove-host 0.4.0"\n' > "$root/releases/0.4.0/cove-host"
chmod 755 "$root/releases/0.4.0/cove-host"
cp "$here/cove-tools.service" "$root/releases/0.4.0/deploy/"
ln -sfn releases/0.4.0 "$root/current"

bash "$here/install.sh" "$tarball"
[ -L "$root/current" ] || fail "no current link"
[ "$(readlink "$root/current")" != releases/0.4.0 ] || fail "install.sh did not switch from the old cove-host release"
[ -x "$root/current/minicloud" ] || fail "the release has no minicloud binary"
[ ! -L "$root/current/minicloud" ] || fail "the release's minicloud is a link, not the binary"
[ "$(readlink "$root/current/cove-host")" = minicloud ] || fail "the release has no cove-host link to minicloud"
"$root/current/cove-host" --version | grep -q '^minicloud ' || fail "current/cove-host does not run minicloud"
grep -q '^ExecStart=/home/ioijoi/cove-tools/current/cove-host serve' "$root/current/deploy/cove-tools.service" \
  || fail "the unit no longer runs current/cove-host, which the installed unit does"
[ "$(stat -c %a "$root/env")" = 600 ] || fail "env is not mode 0600"
[ -f "$root/apps/admin/app.toml" ] || fail "the admin app is not installed by default"
[ -f "$root/apps/admin/assets/favicon.ico" ] || fail "the admin app's static files were not deployed"
for app in webhooks ledger algo hello; do
  [ ! -e "$root/apps/$app" ] || fail "the example $app was installed unasked"
done

# The examples the deployment runs, deployed into the apps directory as any
# app is (with the env, for their secrets): a release must not remove them,
# and must refuse to switch while one would not load.
deployed_examples="webhooks ledger algo hello"
for app in $deployed_examples; do
  (
    set -a
    # shellcheck source=/dev/null
    . "$root/env"
    set +a
    "$root/current/minicloud" deploy "$examples/$app" --into "$root/apps" --data "$root/data" > /dev/null
  ) || fail "minicloud deploy --into refused the example $app"
done
bash "$here/install.sh" --with-bundled-apps "" "$tarball" > /dev/null
for app in $deployed_examples; do
  [ -f "$root/apps/$app/app.toml" ] || fail "a reinstall removed $app, which is not the release's"
  [ ! -e "$root/apps/.previous/$app" ] || fail "a reinstall redeployed $app, which is not the release's"
done
[ ! -e "$root/apps/.previous/admin" ] || fail "a reinstall with --with-bundled-apps \"\" redeployed admin"
echo 'fn broken( {' >> "$root/apps/hello/hello.cove"
ln -sfn releases/before "$root/current"
if bash "$here/install.sh" "$tarball" > "$scratch/refused.log" 2>&1; then
  fail "install.sh switched with an installed app that does not check"
fi
grep -q 'nothing switched' "$scratch/refused.log" || fail "install.sh did not say why it refused: $(cat "$scratch/refused.log")"
[ "$(readlink "$root/current")" = releases/before ] || fail "install.sh switched current although an app does not check"
grep -q 'fn broken' "$root/apps/hello/hello.cove" || fail "install.sh touched the app it refused"
sed -i '/^fn broken( {$/d' "$root/apps/hello/hello.cove"
# Redeploying the bundled app (the default) keeps the version it replaces.
bash "$here/install.sh" "$tarball" > /dev/null
[ -f "$root/apps/.previous/admin/app.toml" ] || fail "a redeploy did not keep the admin app's previous version"
[ -f "$root/apps/hello/hello.cove" ] || fail "a redeploy of the bundled app removed hello"
[ ! -e "$root/apps/.previous/webhooks" ] || fail "a redeploy of the bundled app redeployed webhooks"
# Installing the same release again is allowed (a reinstall). A secret the
# env lacks is appended fresh, and the others are left as they were.
webhooks_before="$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")"
sed -i '/^ADMIN_UI_TOKEN=/d' "$root/env"
bash "$here/install.sh" "$tarball" > /dev/null
grep -q '^ADMIN_UI_TOKEN=.' "$root/env" || fail "the reinstall did not add ADMIN_UI_TOKEN back"
[ "$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")" = "$webhooks_before" ] \
  || fail "the reinstall changed WEBHOOKS_ADMIN_TOKEN"
[ "$(stat -c %a "$root/env")" = 600 ] || fail "env is not mode 0600 after the reinstall"
# A setting the env lacks is appended with the example's value; one it has
# is kept, even when it differs.
sed -i -e '/^ACCESS_TEAM_DOMAIN=/d' -e 's/^COVTOOLS_ACCESS_AUD=.*/COVTOOLS_ACCESS_AUD=kept/' "$root/env"
bash "$here/install.sh" "$tarball" > /dev/null
grep -q '^ACCESS_TEAM_DOMAIN=ioijoi.cloudflareaccess.com$' "$root/env" \
  || fail "the reinstall did not add ACCESS_TEAM_DOMAIN back"
grep -q '^COVTOOLS_ACCESS_AUD=kept$' "$root/env" || fail "the reinstall changed COVTOOLS_ACCESS_AUD"
sed -i "s/^COVTOOLS_ACCESS_AUD=kept$/$(grep '^COVTOOLS_ACCESS_AUD=' "$here/env.example")/" "$root/env"

# The unit's command line, at the scratch home.
command="$(sed -n '/^ExecStart=/,/[^\\]$/p' "$root/current/deploy/cove-tools.service" \
  | sed -e 's/^ExecStart=//' -e 's/\\$//' | tr '\n' ' ' | sed "s|/home/ioijoi|$HOME|g")"
echo "running: $command"

# Starts the host as systemd would, with the env and then `$*` (assignments
# that override it).
start_host() {
  (
    cd "$root"
    set -a
    # shellcheck source=/dev/null
    . "$root/env"
    for assignment in "$@"; do export "${assignment?}"; done
    set +a
    # shellcheck disable=SC2086
    exec $command
  ) &
  pid=$!
  for _ in $(seq 100); do
    curl -fsS -o /dev/null http://127.0.0.1:8790/ 2>/dev/null && break
    sleep 0.2
  done
}

stop_host() {
  kill -TERM "$pid"
  wait "$pid" || fail "the host did not exit cleanly on SIGTERM"
  pid=""
}

status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
header() { curl -s -o /dev/null -D - "$@" | tr -d '\r'; }
admin_token="$(sed -n 's/^ADMIN_UI_TOKEN=//p' "$root/env")"
token="$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")"
admin_host='Host: covtools-admin.ramda.io'

# 1. As deployed: Cloudflare Access on. Nothing here comes through Access,
# so the admin UI and the webhook lab's pages refuse it — the apps' own
# tokens too — and prompt for nothing. No request carries a well-formed
# token, so the host fetches no keys.
start_host
[ "$(status http://127.0.0.1:8790/algo/)" = 200 ] || fail "the algo app is not served"
[ "$(status http://127.0.0.1:8790/_host/stats)" = 404 ] || fail "/_host/ is on the public listener"
[ "$(status http://127.0.0.1:8791/_host/stats)" = 200 ] || fail "/_host/ is not on the admin listener"
[ "$(status -X POST http://127.0.0.1:8791/apps/algo/update)" = 401 ] || fail "the admin listener took an update without its token"
[ "$(status -H "$admin_host" http://127.0.0.1:8790/)" = 403 ] || fail "the admin UI answered without an Access token"
[ "$(status -H "$admin_host" -u "admin:$admin_token" http://127.0.0.1:8790/)" = 403 ] \
  || fail "the admin UI took its token with Access on"
[ "$(status -H "$admin_host" -H 'Cf-Access-Jwt-Assertion: not.a.token' http://127.0.0.1:8790/)" = 403 ] \
  || fail "the admin UI took a malformed Access token"
header -H "$admin_host" http://127.0.0.1:8790/ | grep -qi '^www-authenticate' \
  && fail "the admin UI prompts for a login behind Access"
[ "$(status -H "Authorization: Bearer $token" http://127.0.0.1:8790/webhooks/admin)" = 403 ] \
  || fail "the webhook lab's pages took the token with Access on"
[ "$(status -X POST --data hi http://127.0.0.1:8790/webhooks/in/0000000000000000)" = 404 ] \
  || fail "a receive URL asked for credentials"
[ "$(status http://127.0.0.1:8790/admin/)" = 404 ] || fail "the admin app is on the main hostname"
# Its static files are answered before the app runs, so they do not refuse a
# request the pages would: on the server, Access in front of the hostname is
# what covers them (README, "Static files").
[ "$(status -H "$admin_host" http://127.0.0.1:8790/favicon.ico)" = 200 ] \
  || fail "the admin app's icon is not served"
[ "$(status -H "$admin_host" http://127.0.0.1:8790/style.css)" = 200 ] \
  || fail "the admin app's stylesheet is not served"

# Deploying onto the running host, as from another repository over ssh: an
# archive on stdin, checked, written and updated to.
[ "$(status http://127.0.0.1:8790/hello/)" = 200 ] || fail "the deployed hello app is not served"
deployed="$(tar -C "$examples/hello" -c . \
  | "$root/current/minicloud" deploy - --name hello --admin 127.0.0.1:8791 --token-file "$root/data/admin.token")" \
  || fail "minicloud deploy - failed"
case "$deployed" in
  *'"version": "v2-'*) ;;
  *) fail "minicloud deploy - did not update the running host: $deployed" ;;
esac
[ -d "$root/apps/.previous/hello" ] || fail "the deploy kept no previous version"
"$root/current/minicloud" rollback hello --admin 127.0.0.1:8791 --token-file "$root/data/admin.token" > /dev/null \
  || fail "minicloud rollback failed"
[ "$(status http://127.0.0.1:8790/hello/)" = 200 ] || fail "hello is not served after the rollback"
# A secret set on the running host: kept in the data directory, mode 0600,
# listed by name and never by value, and deleted.
admin_flags=(--admin 127.0.0.1:8791 --token-file "$root/data/admin.token")
printf 'smoke-secret-value\n' | "$root/current/minicloud" secret set smoke "${admin_flags[@]}" > /dev/null \
  || fail "minicloud secret set failed"
[ "$(stat -c %a "$root/data/_host/secrets")" = 600 ] || fail "the secret store is not mode 0600"
listed="$("$root/current/minicloud" secret list "${admin_flags[@]}")" || fail "minicloud secret list failed"
case "$listed" in
  *smoke-secret-value*) fail "minicloud secret list printed a value" ;;
  *'"name": "smoke"'*) ;;
  *) fail "minicloud secret list does not list the secret: $listed" ;;
esac
"$root/current/minicloud" secret delete smoke "${admin_flags[@]}" > /dev/null \
  || fail "minicloud secret delete failed"
stop_host

# 2. Access off (no team): the apps' tokens are the way in, as before.
start_host ACCESS_TEAM_DOMAIN=
# The webhook lab writes its receive URLs at the public origin.
made="$(curl -fsS -H "Authorization: Bearer $token" -H 'Accept: application/json' \
  --data 'name=smoke' http://127.0.0.1:8790/webhooks/admin/endpoints)"
case "$made" in
  *'"https://covtools.ramda.io/webhooks/in/'*) ;;
  *) fail "the receive URL is not at the public origin: $made" ;;
esac
# The admin UI: reached by its hostname only, behind its own secret.
[ "$(status -H "$admin_host" http://127.0.0.1:8790/)" = 401 ] || fail "the admin UI answered without its token"
header -H "$admin_host" http://127.0.0.1:8790/ | grep -qi '^www-authenticate: basic' \
  || fail "the admin UI does not prompt for its token with Access off"
[ "$(status -H "$admin_host" -u "admin:$admin_token" http://127.0.0.1:8790/)" = 200 ] || fail "the admin UI refused its token"
[ "$(status http://127.0.0.1:8790/admin/)" = 404 ] || fail "the admin app is on the main hostname"
# Its static files are answered before the app runs, so they do not refuse a
# request the pages would: on the server, Access in front of the hostname is
# what covers them (README, "Static files").
[ "$(status -H "$admin_host" http://127.0.0.1:8790/favicon.ico)" = 200 ] \
  || fail "the admin app's icon is not served"
[ "$(status -H "$admin_host" http://127.0.0.1:8790/style.css)" = 200 ] \
  || fail "the admin app's stylesheet is not served"
[ "$(status -X POST -H "$admin_host" -u "admin:$admin_token" -H 'Origin: https://evil.example' \
  http://127.0.0.1:8790/apps/hello/disable)" = 403 ] || fail "the admin UI took a cross-site POST"

stop_host

bash "$root/current/deploy/backup.sh"
ls "$root"/backups/*/webhooks.kv.sqlite3.gz > /dev/null || fail "no backup of the webhook lab's store"
echo "smoke.sh: ok"
