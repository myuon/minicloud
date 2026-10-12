#!/usr/bin/env bash
# Installs a minicloud release (the platform: the binary and deploy/) under
# ~/cove-tools (the server's root keeps its old name), as the user that runs
# it. No sudo: the one root step (the systemd unit) is printed, not run.
#
#   install.sh [--with-bundled-apps "admin"] [--root DIR] VERSION|TARBALL
#
#   VERSION   a release tag (v0.5.0 or 0.5.0): downloaded from GitHub
#   TARBALL   a local minicloud-<version>-x86_64-linux.tar.gz, with its
#             .sha256 beside it
#
# A release before 0.5.0 (cove-host-<version>-x86_64-linux.tar.gz, binary
# `cove-host`) installs too, so going back to one is the same command. A
# release from 0.5.0 on ships `cove-host` as a symbolic link to `minicloud`,
# for the unit's `ExecStart=.../current/cove-host serve`.
#
# The apps are not the release's: they are whatever was deployed into
# <root>/apps (`minicloud deploy`, from any repository; README "Deploying an
# app"). Installing a release never removes or replaces one of them, but for
# the one app the release bundles, the admin app, which it deploys by default
# (`--with-bundled-apps ""` deploys none). The example apps (webhooks, ledger,
# algo, ...) are not in the release: deploy them from the repository's
# examples/ with `minicloud deploy` (README "Examples").
#
# What it does:
#   1. downloads (or takes) the tarball and verifies its sha256;
#   2. unpacks it into <root>/releases/<version>/;
#   3. creates <root>/env from deploy/env.example with fresh random secrets
#      if there is none (mode 0600); if there is one, appends each key of the
#      release's env.example that it lacks — a `KEY=change-me` as a fresh
#      secret, any other `KEY=value` as written there — and changes nothing
#      else in it;
#   4. checks every app installed in <root>/apps with the release's binary,
#      against that env and the secret store in <root>/data — with the
#      bundled apps named by --with-bundled-apps in place of their installed
#      versions — and stops, switching nothing, if any would be refused
#      (`--deployed`: a key an installed app.toml may no longer say is a
#      warning, as it is when the host starts, not a refusal);
#   5. deploys the bundled apps --with-bundled-apps names, from the release,
#      with `minicloud deploy --into <root>/apps` (each checked again; the
#      version it replaces is kept in <root>/apps/.previous/<app>); `admin`,
#      the only one, by default;
#   6. points <root>/current at the release; <root>/data is never touched;
#   7. keeps the three newest releases, and prints what to run with sudo.
set -euo pipefail

REPO=myuon/minicloud
ROOT="${MINICLOUD_ROOT:-${COVE_TOOLS_ROOT:-$HOME/cove-tools}}"
BUNDLED="admin"
SOURCE=""

die() { echo "install.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --with-bundled-apps) BUNDLED="$2"; shift 2 ;;
    --apps) die "--apps is gone: a release no longer replaces the apps; --with-bundled-apps \"${2:-}\" deploys those of the release" ;;
    --root) ROOT="$2"; shift 2 ;;
    -h|--help) sed -n '2,44p' "$0"; exit 0 ;;
    -*) die "unknown option $1" ;;
    *) [ -z "$SOURCE" ] || die "one VERSION or TARBALL"; SOURCE="$1"; shift ;;
  esac
done
[ -n "$SOURCE" ] || die "usage: install.sh [--with-bundled-apps \"admin\"] [--root DIR] VERSION|TARBALL"
[ "$(id -u)" -ne 0 ] || die "run it as the user the service runs as, not root"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) die "the release is for x86_64 Linux, not $(uname -s)-$(uname -m)" ;;
esac
for tool in curl sha256sum tar; do
  command -v "$tool" >/dev/null || die "needs $tool"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 1. The tarball and its checksum.
if [ -f "$SOURCE" ]; then
  tarball="$(cd "$(dirname "$SOURCE")" && pwd)/$(basename "$SOURCE")"
  [ -f "$tarball.sha256" ] || die "no $tarball.sha256 beside the tarball"
  cp "$tarball" "$tarball.sha256" "$work/"
  name="$(basename "$tarball")"
else
  version="${SOURCE#v}"
  url="https://github.com/$REPO/releases/download/v$version"
  # minicloud-<v> from 0.5.0 on; cove-host-<v> before.
  for product in minicloud cove-host; do
    name="$product-$version-x86_64-linux.tar.gz"
    echo "downloading $url/$name"
    if curl -fsSL -o "$work/$name" "$url/$name"; then
      break
    fi
    rm -f "$work/$name"
  done
  [ -f "$work/$name" ] || die "no release asset for v$version at $url"
  curl -fsSL -o "$work/$name.sha256" "$url/$name.sha256"
fi
(cd "$work" && sha256sum -c "$name.sha256") || die "checksum mismatch for $name"
case "$name" in
  minicloud-*) product=minicloud ;;
  cove-host-*) product=cove-host ;;
  *) die "not a release tarball: $name" ;;
esac
version="${name#"$product"-}"
version="${version%-x86_64-linux.tar.gz}"
if [ -z "$version" ] || [ "$version" = "$name" ]; then
  die "cannot read a version from $name"
fi

# 2. Unpack into releases/<version>, and point current at it.
mkdir -p "$ROOT/releases"
chmod 700 "$ROOT"
tar -xzf "$work/$name" -C "$work"
top="$work/$product-$version-x86_64-linux"
[ -x "$top/$product" ] || die "$name has no $product binary"
release="$ROOT/releases/$version"
rm -rf "$release.tmp"
mv "$top" "$release.tmp"
rm -rf "$release"
mv "$release.tmp" "$release"
# The binary, under either name.
bin="$release/$product"
"$bin" --version || die "the binary does not run on this machine"

# The bundled apps asked for are the release's, before anything is changed.
for app in $BUNDLED; do
  if [ ! -f "$release/apps/$app/app.toml" ]; then
    bundled="$(for dir in "$release"/apps/*/; do if [ -f "$dir/app.toml" ]; then basename "$dir"; fi; done | tr '\n' ' ')"
    die "the release bundles only: ${bundled% }; \`$app\` is not one of them. The example apps (webhooks, ledger, algo, ...) live in the repository's examples/ and are deployed like any app: minicloud deploy examples/$app --admin 127.0.0.1:8791 --token-file $ROOT/data/admin.token (README \"Examples\"); nothing switched"
  fi
done

# 3. The secrets.
fresh_secret() {
  if command -v openssl >/dev/null; then
    openssl rand -hex 32
  else
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
  fi
}
if [ ! -f "$ROOT/env" ]; then
  (
    umask 077
    while IFS= read -r line; do
      case "$line" in
        *=change-me) echo "${line%=change-me}=$(fresh_secret)" ;;
        *) echo "$line" ;;
      esac
    done < "$release/deploy/env.example" > "$ROOT/env"
  )
  chmod 600 "$ROOT/env"
  echo "created $ROOT/env with fresh secrets (mode 0600); read them there"
else
  # A key this release added: append it, leave every existing line alone.
  # A secret is fresh; a setting is the example's value.
  while IFS= read -r line; do
    case "$line" in
      [A-Za-z_]*=*)
        key="${line%%=*}"
        if ! grep -q "^$key=" "$ROOT/env"; then
          # A file whose last line has no newline would swallow the key.
          [ -z "$(tail -c 1 "$ROOT/env")" ] || echo >> "$ROOT/env"
          if [ "$line" = "$key=change-me" ]; then
            (umask 077; echo "$key=$(fresh_secret)" >> "$ROOT/env")
            echo "added $key to $ROOT/env (fresh secret)"
          else
            (umask 077; echo "$line" >> "$ROOT/env")
            echo "added $line to $ROOT/env"
          fi
        fi ;;
    esac
  done < "$release/deploy/env.example"
  chmod 600 "$ROOT/env"
fi

# Runs the release's binary with the env loaded.
with_env() {
  (
    set -a
    # shellcheck source=/dev/null
    . "$ROOT/env"
    set +a
    "$@"
  )
}

# 4. The installed apps, with the bundled ones about to be deployed in their
# place, checked against the new binary before anything is switched.
mkdir -p "$ROOT/apps" "$ROOT/data" "$ROOT/backups"
chmod 700 "$ROOT/data" "$ROOT/backups"
check="$work/check"
mkdir -p "$check"
for dir in "$ROOT"/apps/*/; do
  [ -f "$dir/app.toml" ] || continue
  app="$(basename "$dir")"
  case " $BUNDLED " in *" $app "*) continue ;; esac
  cp -R "${dir%/}" "$check/$app"
done
for app in $BUNDLED; do
  cp -R "$release/apps/$app" "$check/$app"
done
with_env "$bin" check --deployed --apps "$check" --data "$ROOT/data" \
  || die "an app does not check against this release, $ROOT/env and the secret store (see above; a new secret?); nothing switched"

# 5. The bundled apps asked for, deployed as any app is.
for app in $BUNDLED; do
  with_env "$bin" deploy "$release/apps/$app" --name "$app" --into "$ROOT/apps" --data "$ROOT/data" >/dev/null \
    || die "deploying the bundled app \`$app\` failed (see above); nothing switched"
done

# 6. The switch.
ln -sfn "releases/$version" "$ROOT/current.tmp"
mv -T "$ROOT/current.tmp" "$ROOT/current"

# 7. Keep the three newest releases (and always the current one).
(
  cd "$ROOT/releases"
  # shellcheck disable=SC2012
  ls -1t | tail -n +4 | while IFS= read -r old; do
    [ "$old" = "$version" ] || rm -rf -- "$old"
  done
)

unit=/etc/systemd/system/cove-tools.service
echo
installed="$(for dir in "$ROOT"/apps/*/; do if [ -f "$dir/app.toml" ]; then basename "$dir"; fi; done | tr '\n' ' ')"
installed="${installed% }"
echo "installed minicloud $version in $ROOT (apps: ${installed:-none})"

# A release that carries a newer copy of an app that is installed says so,
# and does not deploy it: `--with-bundled-apps` is how an operator asks for
# that, and an install that quietly replaced an app would be an install
# that could not be rolled back app by app (issue #37).
behind=""
for dir in "$release"/apps/*/; do
  [ -f "$dir/app.toml" ] || continue
  app="$(basename "$dir")"
  [ -f "$ROOT/apps/$app/app.toml" ] || continue
  # The app's files, as `minicloud deploy` chooses them: app.toml, the
  # .cove files, and everything under assets/.
  files() {
    (
      cd "$1" || exit 0
      find . \( -name '*.cove' -o -name app.toml -o -path './assets/*' \) -type f \
        ! -name '.*' | LC_ALL=C sort | while IFS= read -r file; do
        printf '%s ' "$file"
        cksum < "$file"
      done
    )
  }
  if [ "$(files "$dir")" != "$(files "$ROOT/apps/$app")" ]; then
    behind="$behind $app"
  fi
done
if [ -n "$behind" ]; then
  echo
  echo "this release carries a newer copy of:${behind}"
  echo "the installed one is still what serves; deploy the release's with:"
  echo
  echo "  bash install.sh --with-bundled-apps \"${behind# }\" $version"
fi
if [ -z "$installed" ]; then
  echo "no apps are installed: run again without --with-bundled-apps \"\" for the admin app,"
  echo "or deploy one with: $ROOT/current/$product deploy <app-dir> --admin 127.0.0.1:8791 --token-file $ROOT/data/admin.token"
fi
if [ ! -f "$unit" ]; then
  echo "first time: install the unit and start the service (needs sudo, once):"
  echo
  echo "  sudo install -m644 $ROOT/current/deploy/cove-tools.service $unit && sudo systemctl daemon-reload && sudo systemctl enable --now cove-tools"
elif ! cmp -s "$ROOT/current/deploy/cove-tools.service" "$unit"; then
  echo "the unit changed in this release; install it and restart:"
  echo
  echo "  sudo install -m644 $ROOT/current/deploy/cove-tools.service $unit && sudo systemctl daemon-reload && sudo systemctl restart cove-tools"
else
  echo "restart onto it:"
  echo
  echo "  sudo systemctl restart cove-tools"
fi
echo
echo "then: curl -s http://127.0.0.1:8790/ && curl -s http://127.0.0.1:8791/_host/stats | head"
echo "logs: journalctl -u cove-tools -f   (per app: $ROOT/data/<app>/log.txt)"
echo "admin UI: https://covtools-admin.ramda.io/ (the Cloudflare Access login; ACCESS_* in $ROOT/env)"
echo "roll back the platform: ln -sfn releases/<old> $ROOT/current && sudo systemctl restart cove-tools"
echo "roll back an app: $ROOT/current/$product rollback <app> --admin 127.0.0.1:8791 --token-file $ROOT/data/admin.token"
