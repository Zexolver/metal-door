//! door-greeter — the unprivileged greeter UI.
//!
//! This is the beautiful half and the untrusted half. It runs as a system user
//! with no human home directory, owns no privilege, and holds no authority
//! beyond connecting to the daemon's socket and forwarding what the user types.
//! It collects credentials and hands them straight to the daemon for the PAM
//! conversation; it never validates them, never starts a session, never touches
//! a seat. If this process is compromised, the attacker is still not root —
//! that separation is the whole point.
//!
//! Because it runs pre-session, every asset it renders (theme, cursor, font,
//! shaders, wallpaper) must be installed system-wide and world-readable, or it
//! silently falls back to stock. The toolkit is **Iced + iced_layershell**
//! (D-0006); this binary wires the protocol [`client`] to that layer-shell UI.

mod app;
mod client;

fn main() -> iced::Result {
    // The greeter is the Wayland UI built on `client` (the protocol-facing half):
    // a plain iced fullscreen toplevel hosted by doorstep on the greeter VT (D-0007).
    // It connects to the daemon, lists sessions, runs the PAM conversation, and
    // asks the daemon to start the chosen session.
    app::run()
}
