# IDEA — a door compositor ("we have to make a window manager too, then")

**Captured:** 2026-07-04 · **Status:** raw, unratified · **Owner reaction to:** the
D-0019 correction (KWin does not expose `ext-session-lock-v1`, so door-lock —
M10's deliverable — cannot lock the owner's own Plasma desktop).

## The want, decomposed

The trigger sentence was "well we have to make a window manager too then." Three
distinct goals hide in it; the idea is the third, the first two are its cheaper
siblings and were offered as alternatives (2026-07-04):

1. **door look on the owner's daily lock screen** — a door-themed kscreenlocker
   look-and-feel package (QML port of card/wallpaper/clock; WGSL sky needs a
   GLSL/QSB port or a still fallback). Days-scale. Does not need a compositor.
2. **door-lock protecting a real machine** — run Hyprland/sway as the session on
   one box; door-lock already works there. Zero new code.
3. **door grows its own compositor** — this idea.

## Why it is *not* absurd (the steelman)

- **door already ships a compositor dependency in the pre-auth path.** The
  greeter is hosted by `cage` (C, ~unaudited by us, D-0007) on the greeter VT.
  A minimal in-house compositor would remove a third-party C component from the
  most security-sensitive surface door has — squarely in the spirit of the
  Rust-for-the-whole-stack constraint (D-0002).
- **Rust has a serious foundation:** Smithay (the library cosmic-comp grew from)
  is mature, actively maintained, pure Rust, and implements the server side of
  the protocols door cares about — including `ext-session-lock-v1` (COSMIC
  ships it; wayland.app lists COSMIC ✓). The `anvil`/`smallvil` examples are
  honest starting skeletons.
- It is in character for this project: door exists because "from-scratch, but
  defensible" was the point.

## The tiers (record the fork, don't resolve it)

- **Tier A — "door-stage": a kiosk compositor replacing cage.** Single client,
  fullscreen, one output (or per-output fullscreen), no decorations, no
  xdg-shell zoo, no XWayland. Smithay's smallvil is most of the skeleton.
  **Weeks-scale.** Independent product value NOW: drops the cage dep from the
  pre-auth TCB, kills the CSD/black-strip class of bugs at the root, and gives
  the greeter per-output wallpaper (the parked D-0006/D-0007 revisit) for free.
  Does NOT help door-lock (the *session* compositor is still KWin).
- **Tier B — a lock-capable daily-driver session compositor.** What the trigger
  sentence literally asks for: replace KWin for the owner's sessions, implement
  `ext-session-lock-v1`, so door-lock is first-class on door's own stack.
  Everything Tier A skips becomes mandatory: xdg-shell in full, XWayland,
  output/input management, screen sharing/portals, clipboard/dnd, layer-shell,
  fractional scaling, a decade of client quirks. **Many-months-to-years scale,
  plus a permanent maintenance tail.** This is a new product with its own
  vision, threat model, and roadmap — not a door milestone.
- **Tier C — a full DE** (panels, shell, session settings). Out of any sane
  scope; named only to fence it off.

## Capability notes (for the eventual decision)

- **Smithay** — pure Rust, no C compositor lib; used by cosmic-comp in
  production; server-side ext-session-lock, layer-shell, xdg-shell, XWayland
  support. The fit for both tiers. License MIT.
- **wlroots via FFI (wlroots-rs et al.)** — bindings have historically gone
  stale; drags a large C surface back into the TCB — against D-0002's point.
- **From scratch (raw wayland-server)** — rejected-by-default; Smithay *is* the
  from-scratch path with the protocol grind already paid.

## Forks to resolve at graduation

1. Tier A only / A then B / B directly / decline all.
2. In-repo workspace member vs. new repo+scope (Tier B is clearly a new
   project; Tier A could be a door crate, e.g. `door-stage`).
3. Tier A's threat model: it *enters* the pre-auth TCB (replaces cage there) —
   it must be smaller and better-audited than what it replaces, or it loses.
4. Relationship to M10: Tier B would let the M10 Done-when demo run on door's
   own stack; until then the demo rides nested sway (M10.T5, parked/queued).

## Graduation path

DECISION + scope amendment (Tier A: a door milestone; Tier B: arguably its own
`.agent`-tracked project with door as a sibling), then ROADMAP tasks. Per
D-0028, this file moves to `ARCHIVED/` with a pointer when that happens.

## Pointer (2026-07-04, later)

Owner asked for the serious version of "door as the first piece of a desktop
environment." The full anatomy map (what makes Plasma Plasma, the door module
map, honest costs, the shippable sequence) is in
`.agent/REPORTS/2026-07-04-desktop-environment-anatomy.md`. Tier B here is that
map's module 2 (door-comp).

## Naming (2026-07-04, D-0020)

The door-parts naming convention is Binding: Tier A's working name
"door-stage" is reserved as **doorstep**; Tier B as **doorframe**. See
DECISION-0020.

## Graduation (2026-07-04, D-0021) — Tier B fork RESOLVED → foyer

The DE work (Tier B+: doorframe, sill, doorbell, threshold, latch) graduated
to its own sibling project **foyer** (`~/Projects/foyer`), per DECISION-0021.
Of the forks above: 1–2 are answered (Tier B → its own repo/scope; Tier A
remains a door candidate), 3–4 transfer to doorstep's eventual door DECISION.
**This file stays in the inbox for the doorstep (Tier A) half only** — the
cage-replacement question is door's, unratified, and unaffected by foyer.

## Graduation (2026-09-20, D-0022) — Tier A fork RESOLVED → doorstep, in door

The cage-replacement half is built and ratified: **doorstep** is a door workspace
crate hosting the greeter on the greeter VT, and `cage` has left `depends`. Of the
forks above: 1 is answered (Tier A built, Tier B is foyer's, Tier C declined),
2 is answered (in-repo workspace member), and 3–4 are settled in
[DECISION-0022](../../DECISIONS/DECISION-0022-doorstep-kiosk-compositor.md) — its
threat model is the enumerated protocol surface plus the single-client and uid
gates, and M10's demo is unaffected (the *session* compositor is still not door's).

With both halves resolved, this file is archived.
