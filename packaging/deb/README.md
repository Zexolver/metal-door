# Debian/Ubuntu packaging

door is packaged for Arch upstream (`PKGBUILD` at the repo root). This directory
adds a `.deb` build that mirrors that package's layout, so the same tree can be
installed on Debian/Ubuntu — and so this fork and upstream can be built side by
side under distinct package names.

`build-deb.sh` is deliberately not debhelper: it runs `cargo build --release`
and assembles the tree with `dpkg-deb`. One file, no `debian/` boilerplate, and
the whole install layout is readable in one place.

## CI builds them for you

`.github/workflows/release-deb.yml` is the normal path: push a `v*` tag and it
builds `metal-door` for amd64 and arm64, verifies each package, and attaches
both plus a `SHA256SUMS` to that tag's release (creating the release if it does
not exist). It can also be run by hand from the Actions tab against any tag,
with an `include_upstream` option that additionally builds upstream `door` from
`master` for side-by-side comparison.

**Both architectures build natively.** GitHub provides arm64 runners for public
repositories, so CI needs none of the cross-compilation setup below — that is
only for building an arm64 package on a local amd64 machine.

`verify-deb.sh` runs on every package before it is published and checks the
things that actually went wrong while this packaging was written: a split
control stanza, mode 0700 on the package root, binaries built for the wrong
architecture, the wrong variant shape (doorstep vs `Depends: cage`), a shipped
enablement symlink, and unregistered conffiles.

## Build it locally

```sh
packaging/deb/build-deb.sh --source . --name metal-door --arch amd64 --outdir dist-deb
```

`--name` is the package name, so the fork ships as `metal-door` and an upstream
checkout ships as `door`; both can sit in one repository without colliding.

Whether `doorstep` is included is detected from the workspace members, not
passed in — a tree that has it gets the binary and the KMS/input dependencies,
a tree that does not gets `Depends: cage` instead (D-0022).

## Cross-building arm64

The build needs the target's `libpam`, and for `doorstep` also `libinput`,
`libseat`, `libgbm`, `libdrm`, `libudev` and `libxkbcommon`. Installing those as
`:arm64` multiarch packages conflicts with the amd64 `libglib2.0-dev-bin`, so
extract them into a sysroot instead:

```sh
dpkg --add-architecture arm64
# point arm64 at ports.ubuntu.com, then:
apt-get install -y crossbuild-essential-arm64
mkdir -p /opt/sysroot-arm64
cd /tmp && apt-get download libpam0g-dev:arm64 libinput-dev:arm64 libseat-dev:arm64 \
    libgbm-dev:arm64 libdrm-dev:arm64 libudev-dev:arm64 libxkbcommon-dev:arm64 \
    # ...plus their library closure
for d in *.deb; do dpkg-deb -x "$d" /opt/sysroot-arm64; done

# Reproduce usrmerge. Ubuntu ships libm.so etc. as ld scripts naming the
# absolute path /lib/<triplet>/libm.so.6, and --sysroot makes the linker resolve
# that *inside* the sysroot — where extraction left a real ./lib holding only the
# few packages that install there. Without this the link fails with
# "cannot find /lib/aarch64-linux-gnu/libm.so.6 inside /opt/sysroot-arm64".
cd /opt/sysroot-arm64 && cp -an lib/. usr/lib/ && rm -rf lib && ln -s usr/lib lib
```

Then build as usual — the script picks the sysroot up from `$SYSROOT_ARM64`
(default `/opt/sysroot-arm64`) and sets the linker, `pkg-config` and bindgen
variables itself:

```sh
packaging/deb/build-deb.sh --source . --name metal-door --arch arm64 --outdir dist-deb
```

## What the package does on install

Nothing, deliberately. `postinst` creates the greeter system user and reloads
systemd; it does **not** enable `doord` and does not touch the active display
manager. That matches the Arch package and the README's reversibility promise —
installing door can never lock you out. Enabling it is a separate command, and
`prerm` stops it on removal so uninstalling cannot strand a machine on a dead VT.

The two PAM files are marked `conffiles`, so local edits survive upgrades.

## Known gaps

- **The Nerd Font is not in Debian.** The Arch package depends on
  `ttf-meslo-nerd`; there is no equivalent, so the package *recommends*
  `fonts-meslo-lg | fonts-firacode`. The greeter falls back cleanly, but the
  built-in theme is designed around MesloLGS Nerd Font — install it by hand for
  the intended look.
- **Not a policy-conformant Debian package.** No `debian/` source package, no
  lintian pass, no signed changelog. It installs and removes correctly; it is
  not ready for an apt repository.
