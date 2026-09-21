#!/usr/bin/env bash
# Sanity-check a built .deb before it is published.
#
# Every check here corresponds to something that actually went wrong while the
# packaging was written, or to a promise the package makes to its users.
set -euo pipefail

DEB="${1:?usage: verify-deb.sh PACKAGE.deb}"
[[ -f "$DEB" ]] || { echo "no such file: $DEB" >&2; exit 1; }

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

echo "==> verifying $(basename "$DEB")"

NAME="$(dpkg-deb -f "$DEB" Package)"
ARCH="$(dpkg-deb -f "$DEB" Architecture)"
VERSION="$(dpkg-deb -f "$DEB" Version)"

# A blank line in the control stanza silently splits it, and the fields after
# the split are dropped — that is how a package ends up with no Description.
for field in Package Version Architecture Maintainer Depends Description; do
    dpkg-deb -f "$DEB" "$field" | grep -q . || fail "control field $field is empty or missing"
done
ok "control stanza intact ($NAME $VERSION $ARCH)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
dpkg-deb -x "$DEB" "$TMP"

# mktemp gives 0700 and dpkg-deb records that for "./", which is the filesystem
# root on unpack.
root_mode="$(dpkg-deb -c "$DEB" | awk '$NF == "./" {print $1}')"
[[ "$root_mode" == "drwxr-xr-x" ]] || fail "package root is $root_mode, expected drwxr-xr-x"
ok "package root mode is sane"

case "$ARCH" in
    amd64) want="x86-64" ;;
    arm64) want="aarch64" ;;
    *) fail "unknown architecture $ARCH" ;;
esac

expected=(doord door-greeter door-settings door-lock)
# doorstep is the fork's own greeter host; upstream uses cage instead (D-0022).
if [[ "$NAME" != "door" ]]; then
    expected+=(doorstep)
fi

for bin in "${expected[@]}"; do
    [[ -x "$TMP/usr/bin/$bin" ]] || fail "missing /usr/bin/$bin"
    got="$(file -b "$TMP/usr/bin/$bin")"
    [[ "$got" == *"$want"* ]] || fail "/usr/bin/$bin is not $want: $got"
done
ok "${#expected[@]} binaries present and built for $want"

if [[ "$NAME" == "door" ]]; then
    [[ -e "$TMP/usr/bin/doorstep" ]] && fail "upstream package must not ship doorstep"
    dpkg-deb -f "$DEB" Depends | grep -q '\bcage\b' \
        || fail "upstream package must depend on cage"
    ok "upstream package shape (no doorstep, depends on cage)"
else
    dpkg-deb -f "$DEB" Depends | grep -q '\blibinput10\b' \
        || fail "fork package must depend on libinput10 for doorstep"
    ok "fork package shape (ships doorstep, depends on the KMS/input stack)"
fi

# The central promise: installing door must never take over the running display
# manager. A shipped enablement symlink would do exactly that.
[[ -e "$TMP/etc/systemd/system/display-manager.service" ]] \
    && fail "package ships a display-manager symlink"
find "$TMP/etc/systemd/system" -name 'doord.service' 2>/dev/null | grep -q . \
    && fail "package ships doord.service already enabled"
[[ -f "$TMP/usr/lib/systemd/system/doord.service" ]] \
    || fail "missing the doord unit"
ok "unit shipped but not enabled"

for conf in etc/pam.d/doord etc/pam.d/door-greeter; do
    [[ -f "$TMP/$conf" ]] || fail "missing $conf"
    dpkg-deb -f "$DEB" Conffiles 2>/dev/null | grep -q "/$conf" \
        || dpkg-deb --ctrl-tarfile "$DEB" | tar -xO ./conffiles 2>/dev/null | grep -q "/$conf" \
        || fail "$conf is not registered as a conffile"
done
ok "PAM files registered as conffiles"

echo "==> $(basename "$DEB") OK"
