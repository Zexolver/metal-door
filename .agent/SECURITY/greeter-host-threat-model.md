# Threat model — the greeter host (doorstep)

**Scope of this model:** `doorstep`, door's kiosk compositor — the Wayland
*server* that hosts `door-greeter` fullscreen on the greeter VT, per
**DECISION-0022** (M11). It covers the host process itself: its socket, its
protocol surface, its device access, and its teardown. It sits between
`auth-path-threat-model.md` (what happens to a credential once the greeter hands
it to doord) and `session-spawn-threat-model.md` (what happens after a successful
login); this model covers the process that can *see the credential being typed*.

**Why the host is its own model.** D-0007 named the metric correctly: the host
runs pre-authentication, as the greeter user, with **input + video + seat**
access. It observes every keystroke of the password before doord ever sees it. It
is not root and privilege separation still holds — a compromised host is not a
compromised system — but it is the single best position in door from which to
keylog or spoof a login. D-0022 replaced `cage` here not because cage is bad but
because this position should not be occupied by C that door does not own.

Serves Scope Principle 1 (security dominates), 2 (privilege separation),
7 (threat-model-first), and the D-0002 Rust-for-the-whole-stack constraint.

---

## 1. Assets

- **A1 — the password in flight.** Keystrokes travel libinput → doorstep → the
  greeter's `wl_keyboard`. doorstep handles them in cleartext by construction;
  this is inherent to being the compositor, not a defect to fix.
- **A2 — the screen.** Whatever is displayed *is* what the user believes they are
  logging into. Control of the framebuffer is control of the spoof.
- **A3 — the seat.** DRM master and input devices for the greeter VT. Holding
  them past the handoff wedges the machine on a blank VT (D-0008/D-0009).
- **A4 — the VT itself.** The path to a text console, which is door's documented
  recovery route out of a broken login screen.

## 2. Trust boundaries

| Boundary | Who is on the other side | Gate |
|---|---|---|
| B1 — the Wayland socket | any local process that can reach `$XDG_RUNTIME_DIR` | `SO_PEERCRED` uid **and pid** match against the hosted process (`state.rs::admit_client`) |
| B2 — the protocol surface | the admitted client (the greeter) | the enumerated handler list (`handlers/mod.rs`); everything else is not implemented |
| B3 — the seat | logind/libseat | the greeter user's own logind session, opened by doord (D-0008) |
| B4 — the command line | `doord` (`DOORD_GREETER_CMD`) | hard parse: unknown options are refused, not forwarded (`cli.rs`) |

doorstep trusts doord (it is launched by it) and trusts the kernel. It does **not**
trust other local processes, and it does not extend trust to its own client beyond
"draw on one surface and read one seat".

## 3. Threats and controls

- **T1 — a second local process connects and snoops or spoofs the login.**
  cage's socket accepts any connection. doorstep's does not: the peer's
  kernel-attested uid must equal doorstep's own **and** its pid must be the
  process doorstep launched. Anything else is closed before `wayland-server` sees
  it, so it reaches no global at all — verified: a foreign client fails with
  "xdg-shell support required" and never binds a seat.

  The gate is per-*process*, not per-connection. An earlier cut refused every
  connection after the first; integration testing showed the real greeter opens
  three from one pid (iced/wgpu takes its own for the GPU surface). Counting
  connections would have broken the greeter while adding no protection the pid
  check does not already give. A cap of 8 bounds fd exhaustion by a wedged
  client.

  **Residual:** a process already running *as the greeter user* could not connect
  (wrong pid), but it could ptrace the greeter — it is inside the greeter's own
  trust domain either way. This is a boundary, not a hole.

- **T2 — a malicious or buggy client escalates through the protocol surface.**
  The mitigation is subtraction: XWayland, `wlr-layer-shell`, `ext-session-lock`,
  screencopy, `foreign-toplevel`, gamma control, virtual keyboard/pointer, input
  method, text input, tablet, primary selection, `xdg-activation`, DRM lease and
  security contexts are **not implemented**. A protocol that does not exist
  cannot be misused, cannot be CVE'd, and costs nothing to audit. The full
  present/absent list is in `handlers/mod.rs` and D-0022.

- **T3 — memory-safety bugs in the host.** The compositor logic, the protocol
  state machines and the wire parsing are Rust. `use_system_lib` is off, so
  `libwayland-server` is not linked and the Wayland server implementation is pure
  Rust rather than C. **Residual:** the device layer below — `libinput`,
  `libseat`, `libudev`, Mesa/GBM/DRM — is still C, reached through `-sys`
  bindings. Every compositor on Linux has this floor; what D-0022 removed is
  `wlroots` and `cage` *on top of* it.

- **T4 — pre-auth parsing of attacker-influenceable files.** Two deliberate cuts:
  the pointer is a bitmap compiled into the binary instead of an XCursor theme
  loaded from `~/.icons`/`$XCURSOR_PATH`, and EDID is not parsed
  (`smithay-drm-extras` is built without `libdisplay-info`, so outputs are named
  by connector with make/model "Unknown"). Neither is worth a pre-auth parser.

- **T5 — the host outlives the greet and wedges the seat.** `SIGTERM` is
  delivered into the event loop through a self-pipe rather than taken on the
  default disposition, so libseat's teardown runs and DRM master is released
  before doord's session worker takes the VT. doorstep also exits when its client
  does, mirroring the child's exit status so doord's existing
  greeter-lifecycle logic (D-0008/D-0009) sees what it already expects.

- **T6 — the user cannot escape a broken login screen.** On a VT in graphics
  mode the kernel no longer handles `Ctrl+Alt+F<n>`, so a host that ignores those
  keys silently deletes door's documented recovery path. doorstep intercepts the
  chord (the client never sees it) and calls `session.change_vt`. `--no-vt-switch`
  disables this for deployments that want the machine sealed — with the cost
  stated in `--help`.

- **T7 — a spoofed login screen drawn by the host's own decorations.** doorstep
  answers `xdg-decoration` `ServerSide` and then draws nothing at all. There is no
  compositor chrome a user could mistake for, or that could obscure, the greeter's
  own card. (This also fixes the v0.1.7 black strip at the root: the greeter no
  longer needs its client-side workaround to be correct, only to be belt-and-braces
  under a non-doorstep host.)

## 4. Explicitly out of scope

- **Anything a compositor must be able to do.** doorstep reads the keyboard and
  owns the framebuffer. A compromised doorstep keylogs the login. The answer to
  that is privilege separation (it is not root and holds no credential store) and
  a small auditable surface — not a control inside doorstep.
- **The GPU stack.** Mesa runs in this process. Its failure modes are not
  modelled here.
- **Physical attacks on the machine**, which the login screen never defended
  against.

## 5. Verification status

| | |
|---|---|
| Nested backend, end to end | **Verified with the real `door-greeter`:** it connects, renders fullscreen and undecorated with the GPU sky and login card intact, and `SIGTERM` exits with the child's status. |
| uid + pid admission gate | **Verified** (nested; the gate is backend-independent). A foreign client is refused and binds nothing. |
| udev/DRM backend | **Not yet run on hardware.** Compiles and is modelled on Smithay's `anvil`. M11.T5 is the gate; the `cage` fallback exists for exactly this window. |
| VT switch (T6) | **Not yet verified** — needs a real VT, so it rides M11.T5. |
| Adversarial pass | **Not done.** The IPC surface got one (`2026-06-30-ipc-pentest.md`); this one has not. Worth scheduling once T5 passes. |
