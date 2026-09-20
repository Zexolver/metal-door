# DECISION-0007 — Greeter host: cage + plain-iced fullscreen toplevel

**Status:** Binding — *host clause superseded*
**Date:** 2026-06-26
**Ratified:** 2026-06-26
**Project:** door
**Superseded in part by:** D-0022 (2026-09-20). The **host** is now `doorstep`,
door's own Smithay kiosk compositor, not `cage`; `cage` remains a supported
fallback via `DOORD_GREETER_CMD`. Everything else below — the surface-type ruling
(a plain `iced` fullscreen `xdg-toplevel`, not layer-shell), the metric that
picked it, and the `DOORD_GREETER_DEV` behaviour — still stands.
**Supersedes:** the *surface-type* clause of D-0006 (`iced_layershell` /
`wlr-layer-shell`). D-0006's toolkit choice (**Iced**) still stands.

## Context

M6 (packaging + reversible install) must decide what hosts the greeter on its VT.
The first live run surfaced that the obvious minimal kiosk host, `cage` (this Arch
build), advertises **no `wlr-layer-shell`** — so a layer-shell greeter cannot run
under it. The decision was weighed on two primary axes the user named:
**performance / GPU** and **security**.

Key framing established in the analysis:

- **GPU is nearly a wash across hosts.** The visual/animation richness (M4) comes
  from the *greeter's own* `wgpu` rendering, not the host; every wlroots host
  presents one fullscreen surface with full hardware acceleration. The host only
  needs efficient passthrough, which all options do.
- **Security is the differentiator.** The host runs **pre-authentication**, as the
  greeter user, with **input + video + seat** access — it can observe every
  keystroke of the password before it reaches doord (still unprivileged, not root,
  per privsep, but a pre-auth keylog/spoof surface). So the security metric is
  "fewest lines of code that can watch the login" → the **smallest** host wins.
- **Layer-shell has no v1 use.** Its unique benefit is overlaying a *running*
  session (a lock screen); PROJECT-SCOPE §Out-of-scope makes session locking
  out of scope for v1. A sole-client login greeter needs nothing layer-shell offers.

## Decision

The greeter is a **plain `iced` fullscreen `xdg-toplevel`**, hosted by **`cage`**
(one minimal kiosk compositor) on the greeter VT. `iced_layershell` is dropped;
the rest of the greeter — State / update / view and the worker bridge — is
unchanged. The toolkit stays **Iced** (D-0006).

Production: `cage` runs `door-greeter` fullscreen. Dev (`DOORD_GREETER_DEV`): a
normal window (no fullscreen, no grab) — trivially safe to run nested in any
session for testing.

## Rationale (on the chosen axes)

- **Security — best.** `cage` is a purpose-built single-app kiosk: smallest
  pre-auth surface, no IPC socket, no window management. It is the greetd-standard
  greeter host for exactly this reason.
- **Performance / GPU — best.** Thinnest wlroots compositor: one surface, no chrome,
  least compositing overhead, full hardware accel, a clean `wgpu` surface for the
  greeter's own M4 effects.
- **Portability / packaging.** A plain toplevel runs under *any* compositor; `cage`
  is a tiny package dependency.

## Alternatives considered

- **labwc + keep layer-shell.** The lightest *layer-shell* host (no greeter
  change), but a full stacking WM — strictly more pre-auth surface than `cage` for
  features a login never uses. Best fallback if "no greeter change" were weighted
  highest; it was not.
- **sway.** Full i3-style WM with a live IPC socket — the largest pre-auth surface
  and an extra attack vector. Rejected.
- **weston (kiosk-shell).** Does **not** implement `wlr-layer-shell` (a wlroots
  protocol), so it *also* forces a plain toplevel — with more surface than `cage`.
  Strictly dominated.
- **gamescope.** Strongest raw GPU story (upscaling, frame pacing) but large,
  complex, no layer-shell, and a login needs none of it. Rejected.
- **No host (greeter renders直 to KMS/DRM).** Iced/winit cannot drive raw KMS;
  this is the bespoke-`wgpu`/smithay path D-0006 already rejected. Rejected.

## Consequences

- `door-greeter` reworks `run()` to `iced::application(...).window(fullscreen)`,
  drops the `iced_layershell` dependency, the `#[to_layer_message]` macro, and the
  `LayerShellSettings`/`Anchor`/`Layer`/`KeyboardInteractivity` use. `Message`
  becomes a plain `derive(Debug, Clone)` enum; State/update/view/worker unchanged.
- The greeter session unit (M6) launches `cage -- door-greeter` on the dedicated
  VT; `cage` becomes a package dependency.
- `DOORD_GREETER_DEV` now toggles windowed vs fullscreen (no keyboard-grab concern
  exists for a plain toplevel).
- If a future version adds a lock screen (out of v1 scope), layer-shell would be
  reconsidered then via a new decision.
