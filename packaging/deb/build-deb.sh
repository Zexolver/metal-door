#!/usr/bin/env bash
# Build a Debian package of door from a source tree.
#
# door is packaged for Arch upstream (see PKGBUILD, which this mirrors). This
# script produces the same layout as a .deb so the project can be installed on
# Debian/Ubuntu, and so the fork and upstream can be built side by side under
# distinct package names.
#
# Usage:
#   build-deb.sh --source DIR --name PKGNAME --arch amd64|arm64 [--outdir DIR]
#
# The build is a plain `cargo build --release`; cross builds expect an arm64
# sysroot at $SYSROOT_ARM64 (see packaging/deb/README.md).
set -euo pipefail

SOURCE="" NAME="" ARCH="" OUTDIR="$PWD/dist-deb"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --source)  SOURCE="$2"; shift 2 ;;
        --name)    NAME="$2";   shift 2 ;;
        --arch)    ARCH="$2";   shift 2 ;;
        --outdir)  OUTDIR="$2"; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
[[ -n "$SOURCE" && -n "$NAME" && -n "$ARCH" ]] || {
    echo "usage: $0 --source DIR --name PKGNAME --arch amd64|arm64 [--outdir DIR]" >&2
    exit 2
}
SOURCE="$(cd "$SOURCE" && pwd)"
mkdir -p "$OUTDIR"; OUTDIR="$(cd "$OUTDIR" && pwd)"

case "$ARCH" in
    amd64) RUST_TARGET="x86_64-unknown-linux-gnu" ;;
    arm64) RUST_TARGET="aarch64-unknown-linux-gnu" ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 2 ;;
esac

VERSION="$(grep -m1 '^version' "$SOURCE/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
# doorstep only exists in the fork; upstream's greeter host is cage.
HAS_DOORSTEP=no
grep -q '"doorstep"' "$SOURCE/Cargo.toml" && HAS_DOORSTEP=yes

echo "==> building $NAME $VERSION ($ARCH, doorstep=$HAS_DOORSTEP) from $SOURCE"

# ── Build ────────────────────────────────────────────────────────────────────
cd "$SOURCE"
if [[ "$ARCH" == "arm64" ]]; then
    SYSROOT="${SYSROOT_ARM64:-/opt/sysroot-arm64}"
    [[ -d "$SYSROOT" ]] || { echo "missing arm64 sysroot at $SYSROOT" >&2; exit 1; }
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$SYSROOT -L $SYSROOT/usr/lib/aarch64-linux-gnu -L $SYSROOT/lib/aarch64-linux-gnu"
    # pkg-config must answer for the target, from the sysroot only.
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
    export PKG_CONFIG_LIBDIR="$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig:$SYSROOT/usr/share/pkgconfig"
    # bindgen (pam-sys) runs host libclang; point it at the target headers.
    export BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-linux-gnu --sysroot=$SYSROOT -I$SYSROOT/usr/include -I$SYSROOT/usr/include/aarch64-linux-gnu"
fi
cargo build --release --locked --workspace --target "$RUST_TARGET"
BIN="$SOURCE/target/$RUST_TARGET/release"

# ── Stage ────────────────────────────────────────────────────────────────────
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install -d "$STAGE/usr/bin"
for b in doord door-greeter door-settings door-lock; do
    install -m755 "$BIN/$b" "$STAGE/usr/bin/$b"
    strip --strip-unneeded "$STAGE/usr/bin/$b" 2>/dev/null || true
done
if [[ "$HAS_DOORSTEP" == yes ]]; then
    install -m755 "$BIN/doorstep" "$STAGE/usr/bin/doorstep"
    strip --strip-unneeded "$STAGE/usr/bin/doorstep" 2>/dev/null || true
fi

# PAM services for the login stack and the passwordless greeter session.
install -Dm644 "$SOURCE/dist/pam.d/doord"        "$STAGE/etc/pam.d/doord"
install -Dm644 "$SOURCE/dist/pam.d/door-greeter" "$STAGE/etc/pam.d/door-greeter"

# The unit ships INSTALLED BUT NOT ENABLED, exactly as the Arch package does:
# installing door must never take over the running display manager.
install -Dm644 "$SOURCE/dist/systemd/doord.service" \
    "$STAGE/usr/lib/systemd/system/doord.service"
install -Dm644 "$SOURCE/dist/sysusers.d/door.conf" \
    "$STAGE/usr/lib/sysusers.d/door.conf"

install -Dm644 "$SOURCE/dist/door/greeter.toml"  "$STAGE/usr/share/door/greeter.toml"
install -Dm644 "$SOURCE/dist/door/wallpaper.png" "$STAGE/usr/share/door/wallpaper.png"
for preset in "$SOURCE"/dist/door/presets/*.toml; do
    install -Dm644 "$preset" "$STAGE/usr/share/door/presets/$(basename "$preset")"
done
install -Dm644 "$SOURCE/dist/door/door-settings.desktop" \
    "$STAGE/usr/share/applications/door-settings.desktop"
install -Dm644 "$SOURCE/dist/door/door-settings-expert.desktop" \
    "$STAGE/usr/share/applications/door-settings-expert.desktop"
install -Dm644 "$SOURCE/LICENSE" "$STAGE/usr/share/doc/$NAME/copyright"

# ── Control ──────────────────────────────────────────────────────────────────
install -d "$STAGE/DEBIAN"

if [[ "$HAS_DOORSTEP" == yes ]]; then
    # doorstep drives KMS and input itself, so the host compositor's libraries
    # are door's dependencies now. cage is the documented fallback host.
    DEPENDS="libc6, libpam0g, systemd, libinput10, libseat1, libgbm1, libdrm2, libxkbcommon0, libudev1"
    SUGGESTS="cage"
    DESC_HOST="Greeter host: doorstep, door's own Rust kiosk compositor."
else
    DEPENDS="libc6, libpam0g, systemd, cage"
    SUGGESTS=""
    DESC_HOST="Greeter host: cage."
fi

cat > "$STAGE/DEBIAN/control" <<EOF
Package: $NAME
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Maintainer: door packaging <noreply@example.invalid>
Depends: $DEPENDS
Recommends: fonts-meslo-lg | fonts-firacode
${SUGGESTS:+Suggests: $SUGGESTS}
Description: Security-first Wayland login manager with an animated GPU greeter
 door is a privilege-separated Wayland display manager: a small privileged
 daemon (doord) owning PAM, logind seat/VT management and session spawn, paired
 with an unprivileged themed greeter, a settings editor and a session locker.
 .
 $DESC_HOST
 .
 This package installs door DISABLED. It never enables a unit and never touches
 the active display manager; enabling it is a separate, reversible step
 (systemctl enable --now doord).
EOF

# The PAM files are configuration: never clobber local edits on upgrade.
cat > "$STAGE/DEBIAN/conffiles" <<'EOF'
/etc/pam.d/doord
/etc/pam.d/door-greeter
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # Create the greeter system user and register the unit. NOTHING is enabled:
    # door stays inert until the admin explicitly turns it on.
    if command -v systemd-sysusers >/dev/null 2>&1; then
        systemd-sysusers >/dev/null 2>&1 || true
    fi
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload >/dev/null 2>&1 || true
    fi
    cat <<'NOTE'
door is installed but NOT enabled; your current display manager is untouched.
To switch to it:   sudo systemctl enable --now doord
To revert:         sudo systemctl disable --now doord && sudo systemctl enable --now <your previous DM>
NOTE
fi
exit 0
EOF
chmod 755 "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
# Removing the login manager while it owns the screen would strand the machine
# on a dead VT, so stop it first if it happens to be running.
if [ "$1" = "remove" ] && [ -d /run/systemd/system ]; then
    systemctl disable --now doord >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 755 "$STAGE/DEBIAN/prerm"

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 755 "$STAGE/DEBIAN/postrm"

DEB="$OUTDIR/${NAME}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$DEB" >/dev/null
echo "==> $DEB"
dpkg-deb --info "$DEB" | sed -n '1,12p'
