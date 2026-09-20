# Project Decisions

Append-only register of architectural decisions. Each decision lives in
its own file at `.agent/DECISIONS/DECISION-XXXX-slug.md` and progresses
through a defined lifecycle.

`.agent/PROJECT-SCOPE.md` (`## Hard constraints`) governs binding force:
decisions marked **Binding** here apply until explicitly superseded by a
new dated decision.

## Lifecycle

Each decision has a **Status**:

- **Proposed** — under discussion, not yet ratified. Implementation should
  not depend on this decision until ratified.
- **Binding** — ratified, in effect. Cited by file:section in other
  artifacts. Changing direction requires a superseding decision, not an
  edit.
- **Superseded by D-YYYY** — was Binding, replaced by a later decision.
  The original file stays; the index row notes the supersession.

## Three-surface sync at ratification

When a decision moves Proposed → Binding, three surfaces update in
lockstep:

1. **The decision file** — `Status:` Binding, add `Ratified:` date.
2. **This index** — update the row; if it supersedes an earlier decision,
   mark the earlier row "Superseded by D-XXXX (date)".
3. **`PROJECT-STATE.md`** — clear any "deferred until D-XXXX ratified"
   notes referencing this decision.

Missing any of the three is drift. The `decision-log/` tool
automates this.

## Index

| ID | Date | Title |
|---|---|---|

(Add a row per decision in numeric order. Mark superseded decisions
inline in the title cell: `— **Superseded by 0NNN (YYYY-MM-DD)**`.)
| 0001 | 2026-06-25 | [Fully self-contained: bespoke IPC protocol, no greetd](DECISION-0001-bespoke-no-greetd.md) |
| 0002 | 2026-06-25 | [Rust for the whole stack (doord + door-greeter)](DECISION-0002-rust-stack.md) |
| 0003 | 2026-06-25 | [Hardened seam & TCB security defaults](DECISION-0003-hardened-seam-defaults.md) |
| 0004 | 2026-06-25 | [logind session registration via pam_systemd](DECISION-0004-logind-session-pam-systemd.md) — *leadership clause superseded by 0005 (2026-06-26)* |
| 0005 | 2026-06-26 | [Per-login session worker is the logind session leader](DECISION-0005-per-login-session-worker.md) |
| 0006 | 2026-06-26 | [Greeter UI toolkit: Iced + iced_layershell](DECISION-0006-greeter-toolkit-iced.md) — *surface clause superseded by 0007 (2026-06-26); Iced stands* |
| 0007 | 2026-06-26 | [Greeter host: cage + plain-iced fullscreen toplevel](DECISION-0007-greeter-host-cage-toplevel.md) — *host clause superseded by 0022 (2026-09-20): doorstep replaces cage; the plain-toplevel ruling stands* |
| 0008 | 2026-06-26 | [doord owns the greeter lifecycle (handoff + re-greet)](DECISION-0008-doord-owns-greeter-lifecycle.md) |
| 0009 | 2026-06-27 | [Session lifetime tied to doord; seat freed by killing the compositor process group](DECISION-0009-session-lifetime-tied-to-doord.md) |
| 0010 | 2026-06-27 | [Greeter config tooling: standalone Rust editor + shared theme crate (not a Qt/QML KCM)](DECISION-0010-greeter-config-tooling.md) |
| 0011 | 2026-06-27 | [Expand greeter control surface (3 tiers) with help + an --expert gate](DECISION-0011-greeter-control-surface.md) |
| 0012 | 2026-06-29 | [Pre-auth surface stance: caps-lock is TCB-neutral (built); user list / indicators / F1 / F2 stay parked](DECISION-0012-pre-auth-surface-stance.md) — *indicator-parking clause superseded in part by D-0018 (2026-07-03): keyboard-layout + battery graduate; caps-lock/F1/F2 rulings stand* |
| 0013 | 2026-06-30 | [Per-scene authoring controls + sky-mode completeness (M8)](DECISION-0013-per-scene-controls-and-modes.md) |
| 0014 | 2026-06-30 | [Global GPU-budget level (Lite/Moderate/High/Bonkers)](DECISION-0014-global-gpu-level.md) |
| 0015 | 2026-06-30 | [Pre-forked spawner so the supervisor can sandbox itself (M5 Tier 2)](DECISION-0015-pre-forked-spawner-for-sandboxing.md) |
| 0016 | 2026-07-01 | [Supervisor seccomp filter via seccompiler, log-before-enforce (M5 Tier 3)](DECISION-0016-supervisor-seccomp-seccompiler.md) |
| 0017 | 2026-07-02 | [Supervisor Landlock path sandbox via the landlock crate, enforce-only (M5 Tier 4)](DECISION-0017-supervisor-landlock-enforce.md) |
| 0018 | 2026-07-03 | [Pre-auth stance round 2: graduate keyboard-layout + battery (default-off); user-list parked; no-network reaffirmed absolute + a verification engagement](DECISION-0018-pre-auth-stance-round2.md) — *supersedes D-0012's indicator-parking clause in part* |
| 0019 | 2026-07-03 | [Session lock screen: door-lock as an ext-session-lock-v1 client + a doord Reauth verb (M10)](DECISION-0019-session-lock-door-lock.md) |
| 0020 | 2026-07-04 | [Module naming: the door-parts convention](DECISION-0020-module-naming-door-parts.md) — *umbrella-name-parking clause superseded in part by 0021 (2026-07-04): the environment is foyer; the convention + module reservations stand* |
| 0021 | 2026-07-04 | [DE work graduates to its own project: foyer (door stays the DM)](DECISION-0021-de-graduates-to-foyer.md) |
| 0022 | 2026-09-20 | [doorstep: door's own kiosk compositor replaces cage](DECISION-0022-doorstep-kiosk-compositor.md) — *supersedes D-0007's host clause (cage); its surface-type ruling stands* |
