# DECISION-0022 — doorstep: door's own kiosk compositor replaces cage

**Status:** Binding
**Date:** 2026-09-20
**Ratified:** 2026-09-20
**Project:** door
**Supersedes:** the *host* clause of D-0007 (`cage`). D-0007's surface-type ruling
(a plain `iced` fullscreen `xdg-toplevel`, not layer-shell) still stands, and so
does its reasoning about why the host must be minimal.
**Graduates:** the Tier A half of `IDEAS/2026-07-04-door-compositor.md`
("doorstep"). Tier B left for **foyer** under D-0021 and is untouched here.

## Context

D-0007 chose `cage` as the greeter host on the correct metric — "fewest lines of
code that can watch the login" — and `cage` was the best *available* answer at the
time. Two things about it never sat right with D-0002 (Rust for the whole stack):

- `cage` and its `wlroots` dependency are **C, on the pre-auth path**, with input,
  video and seat access, running as the greeter user before any credential is
  typed. Every other component door ships is memory-safe Rust that door owns and
  audits; the one piece that can literally see the password was neither.
- **"Minimal" was minimal-for-a-kiosk, not minimal-for-a-login-screen.** `cage`
  is a general kiosk host: it will run any client, advertises protocols a login
  screen never uses, and accepts any connection on its socket.

The v0.1.7 black-strip bug was a symptom of the same gap: `cage` advertises no
`xdg-decoration`, so winit drew client-side decorations and shrank the greeter's
buffer. door worked around it in the client (`decorations: false`) because it did
not own the host.

`Smithay` (pure Rust, the library `cosmic-comp` is built on, actively maintained)
closed the "what would we build it on" question that made this impractical in June.

## Decision

The greeter is hosted by **`doorstep`**, an in-tree workspace crate: a
single-client, single-fullscreen-surface Wayland compositor built on Smithay,
with a `udev`/DRM backend for the greeter VT and a `nested` backend for
development. It replaces `cage`, and `cage` leaves `depends`.

Three rules define it, each enforced in code rather than by convention:

1. **One process, ours.** The socket admits a peer only if its kernel-attested
   `SO_PEERCRED` uid matches doorstep's own *and* its pid is the process doorstep
   launched. Anything else is closed before `wayland-server` ever sees it — it
   reaches no global.

   The unit is the *process*, not the connection. The first cut of this rule was
   "first client wins", and integration testing killed it immediately: the
   greeter's `iced`/`wgpu` stack opens three connections from one pid (the GPU
   surface gets its own). A connection count protects nothing a pid check does
   not, and the pid check is the stricter rule — `cage` admits any local
   connection at all. A cap of 8 connections bounds a wedged client.
2. **One window, fullscreen, undecorated.** Every toplevel is configured to the
   output size with the fullscreen state set, from the *initial* configure on.
   `xdg-decoration` is answered `ServerSide` and then nothing is drawn, which
   fixes the v0.1.7 black-strip class at the root instead of in the client.
3. **The client's lifetime is the compositor's.** doorstep exits with the hosted
   command's status, and `SIGTERM` is handled through the event loop so libseat
   tears down and the VT is released — the contract D-0008/D-0009 already rely on.

The protocol surface is the threat model, and it is enumerated in
`doorstep/src/handlers/mod.rs`. Present: `wl_compositor`, `wl_subcompositor`,
`wl_shm`, `wl_seat`, `wl_output`/`xdg_output`, `xdg_shell`, `xdg-decoration`,
`linux-dmabuf`, `viewporter`, `fractional-scale`, `presentation-time`,
`wl_data_device_manager`. **Absent on purpose:** XWayland, `wlr-layer-shell`,
`ext-session-lock`, screencopy, `foreign-toplevel`, gamma control, virtual
keyboard/pointer, input method, text input, tablet, primary selection,
`xdg-activation`, DRM lease, security contexts.

`Ctrl+Alt+F<n>` is intercepted and switches VTs. This is not a convenience: on a
VT in graphics mode the kernel stops handling those keys, and door's documented
recovery path ("switch to a TTY and revert") has to work *from the login screen*.
`--no-vt-switch` turns it off for sealed kiosks.

`DOORD_GREETER_CMD` keeps the `HOST -- CLIENT` shape, so
`cage -- /usr/bin/door-greeter` remains a supported fallback and `cage` stays in
`optdepends`.

## Rationale (on D-0007's own axes)

- **Security — better, and now ours.** wlroots + cage leave the pre-auth TCB.
  What remains below doorstep is the device layer any compositor needs
  (`libinput`, `libseat`, `libudev`, Mesa/GBM/DRM) — the kernel-facing C that
  `cage` also used, minus cage and wlroots on top of it. The Wayland *protocol*
  implementation is pure Rust: `use_system_lib` is off, so `libwayland-server` is
  not linked. The single-client and uid gates are stronger than cage's (which
  accepts any connection on its socket), and the absent-protocol list is shorter
  than any general-purpose kiosk's can be.
- **Performance — a wash, slightly ours.** One surface, direct scan-out where the
  hardware allows it, no window management. The greeter's own `wgpu` rendering
  still dominates the frame cost, exactly as D-0007 observed.
- **Ownership.** Bugs at the host/client seam (the black strip) are now fixable
  where they belong, and the greeter's per-output story (the parked D-0006/D-0007
  revisit) is reachable without a third-party dependency.

## Alternatives considered

- **Keep cage.** Zero work, and it is a good program. Rejected only on the C-on-
  the-pre-auth-path ground above; it stays as the documented fallback.
- **`wlroots` via FFI (`wlroots-rs` et al.).** Drags the same C surface back in,
  and the bindings have historically gone stale. Rejected (as in the idea note).
- **A general Rust compositor (`jay`, `niri`, `cosmic-comp`) in kiosk mode.**
  Rust, but each is a full window manager: strictly more pre-auth surface than
  cage, which is the thing being improved on. Rejected.
- **Raw `wayland-server`, no Smithay.** Smithay *is* the from-scratch path with
  the protocol grind already paid. Rejected.
- **Greeter renders straight to KMS/DRM, no host.** Still what D-0006/D-0007
  rejected: iced/winit cannot drive raw KMS. Rejected.

## Consequences

- New workspace member `doorstep`, and `/usr/bin/doorstep` in the package.
  `depends` loses `cage` and gains what the DRM backend links: `libinput`,
  `libseat`, `mesa`, `libxkbcommon` (`systemd` already provided `libudev`).
- `doord`'s default `greeter_cmd` becomes `doorstep -- /usr/bin/door-greeter`.
  Nothing else in `doord` changes: the fork/VT/seat/teardown machinery of
  D-0008/D-0009 is host-agnostic and was already driving a `HOST -- CLIENT`
  command.
- The greeter's `decorations: false` workaround stays, now as belt-and-braces for
  a non-doorstep host rather than as the fix.
- **Cursor:** doorstep draws a built-in arrow for `CursorImageStatus::Named`
  rather than loading an XCursor theme. Theme lookup would mean parsing files from
  `~/.icons`/`$XCURSOR_PATH` pre-auth; the arrow is compiled in.
- **EDID is not parsed.** Outputs are named by connector (`DP-1`) with make/model
  "Unknown", because `libdisplay-info` is a C parser fed by the attached display.
- **Single GPU.** doorstep drives the seat's primary card and lights every
  connected output on it; the greeter is mapped fullscreen on the first, the rest
  hold black. A second card's outputs stay dark at the login screen. This is a
  deliberate scope cut (anvil's multi-GPU copy paths are most of its size); revisit
  if it bites real hardware.
- **Verification status.** The nested backend is verified end to end with the
  **real `door-greeter`**: it connects (three connections, one pid), renders
  fullscreen and undecorated with its GPU sky and login card intact, a foreign
  process is refused, and the SIGTERM path exits with the child's status. The `udev`/DRM backend compiles and is
  modelled closely on Smithay's `anvil`, but has **not** been run on hardware yet
  — that is the acceptance gate before this ships as the default in a release, and
  the `cage` fallback exists for exactly that reason.
