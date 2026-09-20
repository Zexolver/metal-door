#!/usr/bin/env bash
# Verify the greeter's battery indicator HIDES on a machine with no battery (a
# desktop), while the keyboard-layout indicator still shows — a headless cage + grim
# capture with BOTH indicators enabled in the config.
#
# Battery presence is live hardware (`/sys/class/power_supply`), not a config knob, so
# a laptop can't test the "no battery" branch directly. This presents an empty
# power-supply directory to the greeter alone via an unprivileged user+mount namespace
# (only that dir is masked — the real xkb config stays visible, so kb-layout still
# reads). Requires unprivileged user namespaces (the default on most Arch/CachyOS
# kernels) and `cage` + `grim`.
#
# NOTE (D-0022): door's own greeter host is now `doorstep`, not `cage`. This harness
# deliberately still uses `cage` — it needs a *headless* host so it can run over SSH
# with no seat, and doorstep only offers a DRM backend (a VT) and a nested backend
# (an existing compositor). Neither is what a headless screenshot run wants, so the
# dependency here is dev-only and intentionally independent of what door ships. To
# check the same thing under the real host, run it from a graphical session with
# `doorstep --backend nested -- ./target/debug/door-greeter` in place of the `cage`
# line below.
#
# Expected result in the PNG: no "⚡ …%" row (battery hidden), a "⌨ US" row under the
# password (kb-layout shown). Compare a real laptop run (battery present) to see the
# "⚡ …%" row appear from the same config.
set +e
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 1

cargo build -p door-greeter 2>&1 | tail -2
out_dir="$REPO/scratch/indicator-shots"
mkdir -p "$out_dir"
out="$out_dir/indicators-desktop.png"
rm -f "$out"

cfg="$(mktemp --suffix=-door.toml)"
cat > "$cfg" <<EOF
wallpaper = "$REPO/dist/door/wallpaper.png"
show_kb_layout = true
show_battery = true
sky_mode = "auto"
EOF

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WLR_BACKENDS=headless
export WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1
export DOORD_GREETER_DEV=1
export DOORD_GREETER_CONFIG="$cfg"
export DOOR_GREETER_BIN="$REPO/target/debug/door-greeter"
export DOOR_OUT="$out"

for attempt in 1 2 3 4; do
  # New user+mount namespace: overmount an empty tmpfs on the power-supply dir so the
  # greeter (and only it) sees a machine with no battery. cage runs headless, so no
  # seat/DRM access is needed inside the namespace.
  unshare --map-root-user --mount --propagation private bash -c '
    mount -t tmpfs none /sys/class/power_supply
    timeout 18 cage -- bash -c "
      $DOOR_GREETER_BIN & gp=\$!
      sleep 7
      grim \"$DOOR_OUT\"
      kill \$gp 2>/dev/null
    "
  ' >/tmp/verify-indicators-desktop.log 2>&1
  if [ -s "$out" ]; then echo "OK attempt $attempt: $(ls -la "$out")"; break; fi
  echo "attempt $attempt produced no frame; retrying"; sleep 1
done
tail -5 /tmp/verify-indicators-desktop.log
rm -f "$cfg"
