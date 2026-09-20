# door

A beautiful, security-first **login manager** (display manager) for Linux — a
privilege-separated daemon paired with a genuinely gorgeous Wayland greeter.

door owns the screen before any user session exists and decides which session
begins: it owns authentication, the seat/VT, and the session handoff. The headline
is the security model; the wallpaper is just a bonus.

> ⚠️ **Status: v0.1.1 — early alpha.** Proven on hardware (Arch/CachyOS + KDE Plasma
> Wayland), but only on the author's machine so far. It installs **disabled by
> default** and ships a tested two-command revert, so trying it can't lock you out.
> See [Compatibility](#compatibility) before you enable it.

## Architecture

door is **privilege-separated**, deliberately, in the SDDM/greetd lineage:

- **`doord`** — a small *privileged* daemon: PAM auth, `logind` seat/VT management,
  session discovery + spawn, env sanitization, and a peer-cred-checked local IPC
  server. This is the trusted computing base; it stays minimal and is the only thing
  that touches credentials or root.
- **`doorstep`** — the *unprivileged* kiosk compositor that hosts the greeter on
  the greeter VT. It is a single-client, single-fullscreen-surface Wayland server
  built on [Smithay](https://github.com/Smithay/smithay): the socket admits only
  the process it launched (uid- and pid-checked against `SO_PEERCRED`), every
  toplevel is configured fullscreen and undecorated,
  and the protocol surface stops at what a login screen needs — no XWayland, no
  layer-shell, no screencopy, no foreign-toplevel, no security contexts. It
  replaces [`cage`](https://github.com/cage-kiosk/cage) so that nothing on the
  pre-auth path is C that door does not own.
- **`door-greeter`** — an *unprivileged* Wayland UI. It renders the beautiful part
  (GPU-shaded animated sky, themed login card) and holds no authority beyond "ask
  `doord` to try these credentials." A compromised greeter is not root.
- **`door-settings`** — an unprivileged editor for the greeter theme (live preview,
  day/night variants, presets).
- **`door-lock`** — an unprivileged session **lock screen**: an
  `ext-session-lock-v1` client of *your running compositor* that renders the same
  themed surface and unlocks by asking `doord` to verify your own credentials
  (never another user's — the target identity is the kernel-attested socket peer,
  not anything the locker sends). Requires a compositor with
  `ext-session-lock-v1` (sway, Hyprland, river, niri, labwc, Wayfire, COSMIC,
  Weston 12+, …). **KWin/Plasma and GNOME are not supported** — both ship their
  own built-in lockers and do not expose the protocol to third-party lockers
  (verified against KWin 6.7); door-lock declines cleanly rather than pretending.
  If the locker ever wedges, the compositor keeps the screen blanked — recover
  from a TTY (`Ctrl+Alt+F3`, log in, `pkill door-lock` and relaunch it).

The login flow: `doord` launches the greeter under `doorstep` → greeter
authenticates via `doord` → on success `doord` registers the logind session, hands
off the seat/VT, and starts the chosen session.

The host is configurable (`DOORD_GREETER_CMD`), and `cage` takes the same
`HOST -- CLIENT` shape, so `DOORD_GREETER_CMD="cage -- /usr/bin/door-greeter"`
remains a supported fallback if doorstep does not come up on your hardware.

## Compatibility

| | Status |
|---|---|
| **Wayland sessions** (Plasma, GNOME, sway, Hyprland, …) | ✅ supported — only Plasma is hardware-proven so far |
| **X11 sessions** (i3, XFCE, …) | ⛔ not yet — door starts no X server, so X11 entries are hidden by default (`DOORD_ALLOW_X11=1` lists them at your own risk) |
| **Distro** | Arch / CachyOS (needs **systemd/logind**); packaged for Arch only |
| **Greeter host** | `doorstep`, in-tree — needs KMS/DRM, libinput and libseat (`cage` still works as a fallback) |

## Install

From the [AUR](https://aur.archlinux.org/packages/door):

```sh
yay -S door     # or: paru -S door
```

Or build the release yourself:

```sh
git clone https://github.com/satorisage/door && cd door
makepkg -si
```

Installing **does not** change your active display manager — door is inert until you
enable it.

## Enable it (reversible)

```sh
sudo systemctl enable --now doord     # door takes over the login screen
```

If anything goes wrong, switch to a TTY (`Ctrl+Alt+F2`), log in, and revert — your
previous DM is untouched and still installed:

```sh
sudo systemctl disable --now doord
sudo systemctl enable --now sddm       # or gdm / your previous DM
```

## Two-factor with a YubiKey

door can require a **hardware security key** (YubiKey or any FIDO2/U2F key) as a
second factor — password **and** a touch. door already relays multi-round PAM
conversations, so this is a `pam_u2f` line you add to your login stack plus
`pam-u2f` installed; door needs no reconfiguration and the key exchange stays
inside the privileged daemon, never the greeter. Setup, and how to avoid locking
yourself out, are in **[docs/yubikey.md](docs/yubikey.md)**.

## Theming

Run **door-settings** (also under *Settings* in the app menu) to edit the look live:
colors, the animated sky, the comet spinner, card styling, day/night palettes, and
**presets**. Three are built in — *Tokyo Night* (default), *Supernova*, *Nebula* —
and you can save your own to `~/.config/door/presets/`. Launch with `--expert` for
the advanced controls.

The config is `/etc/door/greeter.toml` (copy from `/usr/share/door/greeter.toml`);
every key is documented there.

## Lock screen

`door-lock` locks your **current session** behind the same themed surface as the
login screen — same sky, same card, same config. door ships the lock *surface*
only; wire the trigger into your existing idle stack or a keybinding, e.g.:

```
# Hyprland (hypridle.conf)
listener {
    timeout = 300
    on-timeout = door-lock
}

# sway / river (swayidle)
swayidle -w timeout 300 'door-lock' before-sleep 'door-lock'
```

Unlocking verifies **your own** credentials (password, or your full PAM stack —
a YubiKey touch works here too) through the same audited daemon that logged you
in. Honest bounds and the TTY recovery path are in the Architecture section
above and `door-lock --help`.

## Reporting issues

- **Bugs in door** (greeter, daemon, login, theming) → [GitHub Issues](https://github.com/satorisage/door/issues).
- **Packaging problems** (AUR build fails, bad checksum, stale version) → the
  [AUR package comments](https://aur.archlinux.org/packages/door), or *Flag package out-of-date*.
- **Security vulnerabilities** → **privately**, via the [security policy](SECURITY.md) —
  please don't open a public issue.

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

door was designed and built by **Satori** in partnership with
[Claude Code](https://claude.com/claude-code) (Anthropic's Claude Opus 4.8) — the
architecture, the WGSL shaders, the config-driven theme engine, the preset library,
and this README were paired on end to end. The decisions are mine; the leverage was
real. The git history reflects it (`Co-Authored-By` on the commits).

## License

[MPL-2.0](LICENSE).

> "The door to your system."
