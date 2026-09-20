//! doorstep — door's kiosk compositor.
//!
//! The pre-auth path used to end in `cage`, a C kiosk compositor built on
//! wlroots (D-0007). doorstep replaces it (D-0022): same job, same command-line
//! shape, written in Rust on Smithay, and scoped to what a login screen actually
//! needs. See `handlers/mod.rs` for the protocol surface and `state.rs` for the
//! three rules that define the kiosk.

// With a single backend feature enabled, the backend match arms collapse to one
// variant and every `let BackendData::X(..) = ..` becomes irrefutable.
#![allow(irrefutable_let_patterns)]

mod backend;
mod cli;
mod cursor;
mod handlers;
mod input;
mod render;
mod signals;
mod state;

use std::time::Duration;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use crate::{
    backend::BackendData,
    cli::{Backend, ParseOutcome},
    state::Doorstep,
};

fn main() -> std::process::ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        ParseOutcome::Run(args) => args,
        ParseOutcome::Help => {
            print!("{}", cli::USAGE);
            return std::process::ExitCode::SUCCESS;
        }
        ParseOutcome::Version => {
            println!("doorstep {}", env!("CARGO_PKG_VERSION"));
            return std::process::ExitCode::SUCCESS;
        }
        ParseOutcome::Error(err) => {
            eprintln!("doorstep: {err}\n\n{}", cli::USAGE);
            return std::process::ExitCode::from(2);
        }
    };

    tracing_subscriber::fmt()
        .with_max_level(if args.debug {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .with_writer(std::io::stderr)
        .init();

    match run(args) {
        Ok(code) => std::process::ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            tracing::error!("{err}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: cli::Args) -> Result<i32, Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<Doorstep> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let display: Display<Doorstep> = Display::new()?;

    let chosen = resolve_backend(args.backend);
    tracing::info!("starting on the {chosen} backend");

    // The nested backend opens its window during init and hands back an event
    // source that can only be inserted once the compositor state exists, so the
    // two are carried together.
    #[cfg(feature = "nested")]
    type PendingNested = Option<smithay::backend::winit::WinitEventLoop>;
    #[cfg(not(feature = "nested"))]
    type PendingNested = ();

    let (backend_data, _pending_nested): (BackendData, PendingNested) = match chosen {
        #[cfg(feature = "udev")]
        Backend::Udev => (
            BackendData::Udev(Box::new(backend::udev::init()?)),
            Default::default(),
        ),
        #[cfg(not(feature = "udev"))]
        Backend::Udev => {
            return Err("this doorstep was built without the udev backend".into());
        }
        #[cfg(feature = "nested")]
        Backend::Nested => {
            let (data, winit) = backend::nested::init()?;
            (BackendData::Nested(Box::new(data)), Some(winit))
        }
        #[cfg(not(feature = "nested"))]
        Backend::Nested => {
            return Err("this doorstep was built without the nested backend".into());
        }
        Backend::Auto => unreachable!("resolve_backend never returns Auto"),
    };

    let seat_name = backend_data.seat_name();
    let mut state = Doorstep::new(
        display,
        loop_handle.clone(),
        event_loop.get_signal(),
        backend_data,
        seat_name,
        args.vt_switch,
    )?;

    signals::install(&loop_handle)?;

    match chosen {
        #[cfg(feature = "udev")]
        Backend::Udev => backend::udev::start(&mut state, &loop_handle)?,
        #[cfg(feature = "nested")]
        Backend::Nested => {
            let winit = _pending_nested.expect("nested init produced an event loop");
            backend::nested::start(&mut state, &loop_handle, winit)?;
        }
        _ => {}
    }

    // Only now: the client finds an output the moment it connects.
    state.spawn_child(&args.command)?;

    event_loop.run(Some(Duration::from_millis(100)), &mut state, |state| {
        state.poll_child();
        state.space.refresh();
        state.popups.cleanup();
        let _ = state.display_handle.flush_clients();
    })?;

    // The child outlives the loop only if we were signalled; do not leave it
    // holding a socket that no longer has a compositor behind it.
    state.shutdown();
    state.reap();

    Ok(state.exit_code)
}

/// `auto` means "nested if there is a compositor to nest in, else the VT".
fn resolve_backend(requested: Backend) -> Backend {
    match requested {
        Backend::Auto => {
            let nested = std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("WAYLAND_SOCKET").is_some();
            if nested && cfg!(feature = "nested") {
                Backend::Nested
            } else {
                Backend::Udev
            }
        }
        explicit => explicit,
    }
}
