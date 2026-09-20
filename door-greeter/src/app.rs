//! The greeter UI: an Iced `wlr-layer-shell` overlay wired to the daemon.
//!
//! The app itself holds no authority. A background worker owns the protocol
//! [`Client`](crate::client::Client) (blocking socket I/O) and talks to this UI
//! over two channels: the UI sends [`Command`]s to the worker, and the worker
//! emits [`Message`]s back through an Iced subscription. The worker hands the UI
//! its command channel via [`Message::WorkerReady`] (the standard Iced
//! external-worker handshake), so nothing global is needed.
//!
//! Credentials never leave this process except as the daemon's PAM reply: the
//! typed password lives in the UI state only until the worker is told to send it,
//! then the field is cleared.

use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::SinkExt;
use iced::widget::{
    button, canvas, column, container, image, pick_list, row, shader, stack, svg, text, text_input,
    Column, Space, Stack,
};
use iced::{
    keyboard, window, Alignment, Background, Border, ContentFit, Element, Font, Length,
    Subscription, Task,
};

use protocol::{PowerAction, Secret, Session};

use crate::client::{AuthStep, Client, StartOutcome, DEFAULT_SOCKET};
use door_theme::clock::AnalogClock;
use door_theme::host::{
    battery, caps_lock_on, clock_angles, is_svg, kb_layout, now_date, now_hm, now_minutes,
    now_month, vet_theme_assets, Battery,
};
use door_theme::skyshader::{FrostShader, SkyShader, SpinnerShader};
use door_theme::styles::{button_style, card_style, field_style};
use door_theme::{CardPos, ClockStyle, Theme};

/// The resolved theme, loaded once. `run` needs it for the default font before the
/// app state exists, and the UI reads it every frame — so it lives here, not in
/// `State`.
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(Theme::load)
}

/// Run the greeter (D-0007): a plain `iced` fullscreen toplevel, hosted by
/// `doorstep` on the greeter VT (D-0022 replaced `cage` with door's own kiosk
/// compositor). As the sole client on its own compositor it needs nothing
/// `wlr-layer-shell` offers (v1 has no lock screen), and the host — the smallest
/// pre-auth surface — does not advertise it.
///
/// Production: fullscreen. Dev (`DOORD_GREETER_DEV` set): a normal window, so the
/// greeter can be run nested in any session for testing — no keyboard grab, just a
/// window (Ctrl-C the launching terminal, or close it, to quit).
pub fn run() -> iced::Result {
    let fullscreen = std::env::var_os("DOORD_GREETER_DEV").is_none();

    let mut app = iced::application(State::new, update, view)
        .title("door")
        .window(window::Settings {
            fullscreen,
            // A host that advertises no server-side-decoration protocol (cage
            // did not) makes winit fall back to client-side decorations even
            // fullscreen: a ~35px titlebar subsurface placed above the origin,
            // shrinking our content buffer to 1045 and leaving the bottom 35px of
            // the 1080 panel uncovered — the black strip. doorstep answers
            // xdg-decoration server-side and draws none, so this is now belt and
            // braces; keep it for hosts that do not (and because we draw our own
            // chrome regardless).
            decorations: false,
            ..Default::default()
        })
        .style(app_style)
        .subscription(subscription);

    // Render in the themed font + weight. A configured family must be installed
    // system-wide; iced wants a 'static name, so leak the single, process-lifetime
    // string. Weight applies whether or not a custom family is set.
    let mut font = match theme().font.clone() {
        Some(name) => Font::with_name(Box::leak(name.into_boxed_str())),
        None => Font::DEFAULT,
    };
    font.weight = theme().font_weight.iced();
    app = app.default_font(font);
    app.run()
}

/// Window-level appearance from the theme: the solid background (also shown at any
/// wallpaper letterbox edge) and the default text color.
fn app_style(state: &State, _theme: &iced::Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: state.theme.background.iced(),
        text_color: state.theme.foreground.iced(),
    }
}

/// The greeter's subscriptions: the daemon worker event stream, a 1 Hz clock, and
/// the one-shot launch fade.
fn subscription(state: &State) -> Subscription<Message> {
    let mut subs = vec![
        Subscription::run(daemon_worker),
        Subscription::run(clock_ticker),
        Subscription::run(fade_ticker),
        // Tab / Shift-Tab cycle focus; a CapsLock press re-reads the lock state.
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                modifiers,
                ..
            }) => Some(if modifiers.shift() {
                Message::FocusPrev
            } else {
                Message::FocusNext
            }),
            iced::Event::Keyboard(
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::CapsLock),
                    ..
                }
                | keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(keyboard::key::Named::CapsLock),
                    ..
                },
            ) => Some(Message::CapsLockChanged),
            _ => None,
        }),
    ];
    // Drive the animation off the compositor's frame clock (vsync — 60/120/144 Hz),
    // not a fixed-rate thread, so motion is buttery smooth. Only when animated. The GPU
    // level can cap the frame rate: capped tiers use a fixed-rate ticker,
    // `bonkers` (uncapped) rides vsync.
    if state.theme.animate {
        match state.theme.gpu_level.fps_cap() {
            Some(_) => subs.push(Subscription::run(frame_ticker)),
            None => subs.push(iced::window::frames().map(|_| Message::AnimTick)),
        }
    }
    Subscription::batch(subs)
}

/// Emit a [`Message::Tick`] once a second so the card clock stays current. A small
/// thread sleeps and pushes through the Iced channel (same shape as the daemon
/// worker) — avoids pulling an async timer/runtime feature onto the greeter.
fn clock_ticker() -> impl futures::Stream<Item = Message> {
    iced_futures::stream::channel(
        4,
        |output: futures::channel::mpsc::Sender<Message>| async move {
            std::thread::spawn(move || {
                let mut output = output;
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if futures::executor::block_on(output.send(Message::Tick)).is_err() {
                        break;
                    }
                }
            });
            std::future::pending::<()>().await;
        },
    )
}

/// Drive the launch fade-in: ~24 [`Message::Fade`] ticks over ~380 ms, then the
/// stream ends (bounded — no perpetual repaint on the idle greeter).
fn fade_ticker() -> impl futures::Stream<Item = Message> {
    // ~16 ms steps over the configured fade duration (one extra step lands fade at 1.0).
    let ticks = (theme().fade_ms / 16.0).ceil() as usize + 1;
    iced_futures::stream::channel(
        4,
        move |output: futures::channel::mpsc::Sender<Message>| async move {
            std::thread::spawn(move || {
                let mut output = output;
                for _ in 0..ticks {
                    std::thread::sleep(Duration::from_millis(16));
                    if futures::executor::block_on(output.send(Message::Fade)).is_err() {
                        return;
                    }
                }
            });
            std::future::pending::<()>().await;
        },
    )
}

/// Emit [`Message::AnimTick`] at a fixed frame cap (the global GPU level) instead of the
/// uncapped vsync `window::frames()`. `anim` is recomputed from real elapsed time each
/// tick, so motion stays time-correct — we just repaint (and re-run the sky shader)
/// fewer times per second. A small sleeping thread, same shape as `clock_ticker`.
fn frame_ticker() -> impl futures::Stream<Item = Message> {
    let fps = theme().gpu_level.fps_cap().unwrap_or(60);
    let dt = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    iced_futures::stream::channel(
        4,
        move |output: futures::channel::mpsc::Sender<Message>| async move {
            std::thread::spawn(move || {
                let mut output = output;
                loop {
                    std::thread::sleep(dt);
                    if futures::executor::block_on(output.send(Message::AnimTick)).is_err() {
                        break;
                    }
                }
            });
            std::future::pending::<()>().await;
        },
    )
}

/// A startable session, rendered by name in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChoice {
    id: String,
    name: String,
}

impl std::fmt::Display for SessionChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Where the greeter is in the login flow.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Connecting / waiting for the worker and the session list.
    Connecting,
    /// Idle, ready for the user to enter credentials.
    Ready,
    /// Authentication is in flight (waiting on PAM prompts / verdict).
    Authenticating,
    /// The session was started; the greeter is stepping aside.
    Started,
}

/// What the UI asks the worker to do.
#[derive(Debug)]
pub enum Command {
    Authenticate { username: String },
    Reply(Secret),
    Start { session_id: String },
    Power(PowerAction),
}

#[derive(Debug, Clone)]
pub enum Message {
    // From the worker:
    WorkerReady(mpsc::Sender<Command>),
    Sessions(Vec<Session>),
    /// PAM wants input; `secret` ⇒ it is the password (auto-answered if typed).
    Prompt {
        text: String,
        secret: bool,
    },
    Notice(String),
    /// The user edited the interactive second-factor prompt field (FIDO2 PIN, …).
    PromptChanged(String),
    /// The user submitted the interactive second-factor prompt — send the reply.
    PromptSubmitted,
    AuthSucceeded,
    AuthFailed(String),
    SessionStarted,
    DaemonError(String),
    Fatal(String),
    /// 1 Hz clock tick — refreshes the card clock + date.
    Tick,
    /// One step of the launch fade-in.
    Fade,
    /// Continuous ambient animation tick (twinkle + breathing card edge).
    AnimTick,
    /// Tab / Shift-Tab focus traversal across the fields and sign-in.
    FocusNext,
    FocusPrev,
    /// CapsLock was pressed/released — re-read the lock state.
    CapsLockChanged,
    // From the UI:
    UsernameChanged(String),
    PasswordChanged(String),
    SessionPicked(SessionChoice),
    LoginPressed,
    PowerPressed(PowerAction),
}

/// An interactive PAM prompt whose reply the greeter must collect by typing —
/// a second factor the pre-typed password can't answer, e.g. a FIDO2 PIN in a
/// passwordless setup, or any prompt after the first. The password (the first
/// secret prompt, pre-typed into the login form) is still auto-answered and
/// never routes through here. `value` holds the in-progress input and is moved
/// into a zeroizing [`Secret`] the instant it is submitted.
struct PromptField {
    text: String,
    secret: bool,
    value: String,
}

struct State {
    phase: Phase,
    sessions: Vec<SessionChoice>,
    selected: Option<SessionChoice>,
    username: String,
    password: String,
    status: String,
    /// Whether `status` is an error (login failed, daemon unreachable) — colored with
    /// the theme's `error_color` rather than the muted tone.
    status_error: bool,
    cmd_tx: Option<mpsc::Sender<Command>>,
    /// The resolved look (a clone of the process-wide [`theme`]).
    theme: Theme,
    /// The theme's wallpaper/logo paths *after* the pre-auth trust + size vetting
    /// ([`vet_theme_assets`]) — `None` where the configured asset was refused. Vetted
    /// once when the theme is set, so `view` never re-stats the filesystem per frame.
    wallpaper: Option<std::path::PathBuf>,
    logo: Option<std::path::PathBuf>,
    /// Current local time, `HH:MM`, refreshed by [`Message::Tick`].
    clock: String,
    /// Current local date, e.g. `Friday, June 27`.
    date: String,
    /// Launch fade-in progress, 0.0 → 1.0 (driven by [`Message::Fade`]).
    fade: f32,
    /// Animation clock in seconds — recomputed from `started` every frame so motion
    /// is time-accurate and smooth (not a fixed per-tick increment).
    anim: f32,
    /// When the greeter started, the zero point for `anim`.
    started: Instant,
    /// Whether Caps Lock is currently on — read from the keyboard LED (local; no
    /// daemon). Drives the warning shown by the password field.
    caps_lock: bool,
    /// The active keyboard layout code (e.g. `us`, `de`) from a local xkb read, shown
    /// under the password field when `theme.show_kb_layout` is on. `None` when the
    /// indicator is off or no layout is determinable. Refreshed on the 1 Hz Tick.
    kb_layout: Option<String>,
    /// Battery charge for the optional indicator, from a local sysfs read. `None` when
    /// the indicator is off or the machine has no battery. Refreshed on the 1 Hz Tick.
    battery: Option<Battery>,
    /// An interactive second-factor prompt awaiting a typed reply (a FIDO2 PIN,
    /// or any prompt the pre-typed password can't answer). `None` unless PAM is
    /// mid-conversation asking for input the login form didn't already supply.
    prompt: Option<PromptField>,
    /// A one-way PAM cue shown mid-authentication — e.g. "Please touch the
    /// device" from `pam_u2f cue`. Rendered as a distinct waiting indicator, not
    /// a plain status line, so a hardware-key touch reads as an action to take.
    cue: Option<String>,
}

impl State {
    fn new() -> Self {
        // Pick the day or night variant by the local clock — the greeter runs
        // pre-login, so it can't read the user's color scheme; time is the trigger.
        let theme = Theme::load_at(now_minutes(), now_month());
        let clock = now_hm(
            theme.clock_24h,
            theme.clock_seconds,
            theme.clock_format.as_deref(),
        );
        let (wallpaper, logo) = vet_theme_assets(&theme);
        // Read the optional indicators before `theme` is moved into the struct.
        let kb_layout = theme.show_kb_layout.then(kb_layout).flatten();
        let battery = theme.show_battery.then(battery).flatten();
        State {
            phase: Phase::Connecting,
            sessions: Vec::new(),
            selected: None,
            username: String::new(),
            password: String::new(),
            status: "Connecting to doord…".to_string(),
            status_error: false,
            cmd_tx: None,
            theme,
            wallpaper,
            logo,
            clock,
            date: now_date(),
            fade: 0.0,
            anim: 0.0,
            started: Instant::now(),
            caps_lock: caps_lock_on().unwrap_or(false),
            kb_layout,
            battery,
            prompt: None,
            cue: None,
        }
    }

    /// Send a command to the worker, noting if the worker is gone.
    fn send(&mut self, command: Command) {
        match &self.cmd_tx {
            Some(tx) => {
                if tx.send(command).is_err() {
                    self.status = "Lost the connection to doord.".to_string();
                    self.phase = Phase::Connecting;
                }
            }
            None => self.status = "Not connected to doord yet.".to_string(),
        }
    }
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    let mut task = Task::none();
    match message {
        Message::WorkerReady(tx) => {
            state.cmd_tx = Some(tx);
            state.status = "Select a session and sign in.".to_string();
            if state.phase == Phase::Connecting {
                state.phase = Phase::Ready;
            }
        }
        Message::Sessions(sessions) => {
            state.sessions = sessions
                .into_iter()
                .map(|s| SessionChoice {
                    id: s.id,
                    name: s.name,
                })
                .collect();
            if state.selected.is_none() {
                state.selected = state.sessions.first().cloned();
            }
        }
        Message::Prompt { text, secret } => {
            // A fresh prompt supersedes any prior touch cue.
            state.cue = None;
            if secret && !state.password.is_empty() {
                // Common case: the user pre-typed their password into the login
                // form, so answer PAM's password prompt instantly — no extra
                // field, the smooth path stays smooth.
                state.status = text;
                let reply = Secret::new(std::mem::take(&mut state.password));
                state.send(Command::Reply(reply));
            } else {
                // A prompt the login form can't answer: a FIDO2 PIN, a second
                // factor, or the first prompt of a passwordless stack. Surface
                // PAM's own text and collect the typed reply interactively. The
                // field's label carries the prompt; clear the status line so the
                // two don't echo each other.
                state.status = String::new();
                state.status_error = false;
                state.prompt = Some(PromptField {
                    text,
                    secret,
                    value: String::new(),
                });
                task = iced::widget::operation::focus(PROMPT_ID);
            }
        }
        Message::Notice(text) => {
            // Mid-authentication with no input field open, a one-way PAM message
            // is a cue to act (touch your key) — show it as a distinct waiting
            // indicator. Anywhere else it is ordinary status text.
            if matches!(state.phase, Phase::Authenticating) && state.prompt.is_none() {
                state.cue = Some(text);
                state.status = String::new();
            } else {
                state.status = text;
            }
        }
        Message::PromptChanged(value) => {
            if let Some(p) = &mut state.prompt {
                p.value = value;
            }
        }
        Message::PromptSubmitted => {
            if let Some(p) = state.prompt.take() {
                // Move the typed value straight into a zeroizing Secret — no
                // lingering copy — and send it as this round's PAM reply.
                let reply = Secret::new(p.value);
                state.send(Command::Reply(reply));
                state.status = "Authenticating…".to_string();
            }
        }
        Message::AuthSucceeded => {
            state.password.clear();
            state.prompt = None;
            state.cue = None;
            match &state.selected {
                Some(choice) => {
                    state.status = format!("Starting {}…", choice.name);
                    let id = choice.id.clone();
                    state.send(Command::Start { session_id: id });
                }
                None => {
                    state.status = "Authenticated, but no session selected.".to_string();
                    state.phase = Phase::Ready;
                }
            }
        }
        Message::AuthFailed(reason) => {
            state.password.clear();
            state.prompt = None;
            state.cue = None;
            state.status = reason;
            state.status_error = true;
            state.phase = Phase::Ready;
        }
        Message::SessionStarted => {
            state.phase = Phase::Started;
            state.status = "Session started.".to_string();
            // The greeter's job is done: step aside so the host compositor
            // (doorstep) exits and frees the VT for the session the daemon just
            // launched.
            task = iced::exit();
        }
        Message::DaemonError(message) => {
            state.password.clear();
            state.prompt = None;
            state.cue = None;
            state.status = message;
            state.status_error = true;
            state.phase = Phase::Ready;
        }
        Message::Fatal(message) => {
            state.prompt = None;
            state.cue = None;
            state.status = format!("Cannot reach doord: {message}");
            state.status_error = true;
            state.phase = Phase::Connecting;
        }
        Message::UsernameChanged(value) => state.username = value,
        Message::PasswordChanged(value) => state.password = value,
        Message::SessionPicked(choice) => state.selected = Some(choice),
        Message::LoginPressed => {
            state.status_error = false;
            if state.username.is_empty() {
                state.status = "Enter a username.".to_string();
            } else if state.selected.is_none() {
                state.status = "Select a session.".to_string();
            } else {
                state.phase = Phase::Authenticating;
                state.status = "Authenticating…".to_string();
                // A fresh conversation: drop any stale prompt/cue from a prior try.
                state.prompt = None;
                state.cue = None;
                let username = state.username.clone();
                state.send(Command::Authenticate { username });
            }
        }
        Message::PowerPressed(action) => state.send(Command::Power(action)),
        Message::Tick => {
            // Live config hot-reload: re-resolve the theme each second and swap it only
            // if it changed — so an edit to greeter.toml (or crossing the day/night
            // boundary) takes effect without restarting the greeter. Cheap: one small
            // TOML parse, and the PartialEq guard avoids needless repaint churn.
            let fresh = Theme::load_at(now_minutes(), now_month());
            if fresh != state.theme {
                // Re-vet assets only when the theme actually changed (a config edit or a
                // day/night flip) — not every tick.
                let (wallpaper, logo) = vet_theme_assets(&fresh);
                state.wallpaper = wallpaper;
                state.logo = logo;
                state.theme = fresh;
            }
            state.clock = now_hm(
                state.theme.clock_24h,
                state.theme.clock_seconds,
                state.theme.clock_format.as_deref(),
            );
            state.date = now_date();
            // Backstop poll of the Caps Lock LED, in case a press arrived while the
            // greeter lacked focus (the key-event path catches the focused case).
            if let Some(on) = caps_lock_on() {
                state.caps_lock = on;
            }
            // Refresh the optional indicators (local reads; skipped when disabled, and
            // re-evaluated here so a hot-reload that flips the flag takes effect).
            state.kb_layout = if state.theme.show_kb_layout {
                kb_layout()
            } else {
                None
            };
            state.battery = if state.theme.show_battery {
                battery()
            } else {
                None
            };
        }
        Message::Fade => state.fade = (state.fade + 16.0 / state.theme.fade_ms.max(16.0)).min(1.0),
        // Recompute the clock from real elapsed time each frame — smooth, jitter-free.
        Message::AnimTick => state.anim = state.started.elapsed().as_secs_f32() % 10_000.0,
        Message::FocusNext => task = iced::widget::operation::focus_next(),
        Message::FocusPrev => task = iced::widget::operation::focus_previous(),
        Message::CapsLockChanged => {
            if let Some(on) = caps_lock_on() {
                state.caps_lock = on;
            }
        }
    }
    task
}

// The pre-auth asset vetting, the local time reads, and the indicator probes
// (Caps Lock, keyboard layout, battery) live in `door_theme::host` — shared with
// door-lock so both auth surfaces vet and read identically.

/// Stable id for the interactive second-factor field, so `update` can focus it
/// the moment PAM issues a prompt the login form didn't pre-answer.
const PROMPT_ID: &str = "pam-prompt";

fn view(state: &State) -> Element<'_, Message> {
    let t = &state.theme;
    let f = state.fade.clamp(0.0, 1.0);
    let fg = t.foreground.iced_alpha(f);
    let muted = t.muted.iced_alpha(f);

    // Clock + date — the minimal focal point at the top of the card.
    let mut header_items: Vec<Element<Message>> = Vec::new();
    if t.show_clock {
        let time_widget: Element<Message> = match t.clock_style {
            ClockStyle::Digital => text(state.clock.clone())
                .size(t.clock_size * t.font_scale)
                .color(fg)
                .into(),
            ClockStyle::Analog => {
                let d = t.clock_size * t.font_scale * 2.3;
                canvas(AnalogClock::new(t, f, clock_angles()))
                    .width(Length::Fixed(d))
                    .height(Length::Fixed(d))
                    .into()
            }
        };
        header_items.push(time_widget);
        header_items.push(
            text(state.date.clone())
                .size(13.0 * t.font_scale)
                .color(muted)
                .into(),
        );
    }
    // Optional battery indicator (off by default): a subtle line near the clock,
    // hidden when the machine has no battery. `⚡` marks charging/full.
    if t.show_battery {
        if let Some(b) = &state.battery {
            let label = if b.charging {
                format!("⚡ {}%", b.percent)
            } else {
                format!("{}%", b.percent)
            };
            header_items.push(text(label).size(12.0 * t.font_scale).color(muted).into());
        }
    }
    let header: Element<Message> = if header_items.is_empty() {
        Space::new().into()
    } else {
        Column::with_children(header_items)
            .spacing(2)
            .align_x(Alignment::Center)
            .into()
    };

    // Emblem: a user-set image (SVG crisp, else raster) overrides; otherwise the
    // animated spinner — unless the style is `none`, which shows no emblem at all.
    let emblem: Option<Element<Message>> = match &state.logo {
        Some(path) if is_svg(path) => Some(
            svg(svg::Handle::from_path(path.clone()))
                .height(Length::Fixed(56.0))
                .into(),
        ),
        Some(path) => Some(
            image(image::Handle::from_path(path))
                .height(Length::Fixed(56.0))
                .into(),
        ),
        None if t.spinner_style.is_hidden() => None,
        None => Some(
            shader(SpinnerShader::from_theme(t, state.anim, f))
                .width(Length::Fixed(t.spinner_size))
                .height(Length::Fixed(t.spinner_size))
                .into(),
        ),
    };
    // Wrap in the optional backdrop tile (transparent by default); `None` → no emblem.
    let logo_bg = t.logo_box.iced_alpha(f);
    let logo_radius = t.logo_box_radius;
    let logo: Option<Element<Message>> = emblem.map(|e| {
        container(e)
            .padding(6)
            .style(move |_theme| container::Style {
                background: Some(Background::Color(logo_bg)),
                border: Border {
                    radius: logo_radius.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    });

    let username = text_input("user", &state.username)
        .on_input(Message::UsernameChanged)
        .on_submit(Message::LoginPressed)
        .padding(11)
        .size(15.0 * t.font_scale)
        .style(field_style(t, f));

    let password = text_input("password", &state.password)
        .on_input(Message::PasswordChanged)
        .on_submit(Message::LoginPressed)
        .secure(true)
        .padding(11)
        .size(15.0 * t.font_scale)
        .style(field_style(t, f));

    let busy = matches!(state.phase, Phase::Authenticating | Phase::Started);
    let mut login = button(
        text("Sign in")
            .width(Length::Fill)
            .center()
            .size(15.0 * t.font_scale),
    )
    .width(Length::Fill)
    .padding(11)
    .style(button_style(t, f));
    if !busy {
        login = login.on_press(Message::LoginPressed);
    }

    // Session: a slim, subtle selector under the button.
    let picker = pick_list(
        state.sessions.clone(),
        state.selected.clone(),
        Message::SessionPicked,
    )
    .placeholder("session")
    .text_size(13.0 * t.font_scale)
    .padding(8)
    .width(Length::Fill)
    .style(picker_style(t, f));

    // Status only takes space when there is something to say.
    let status: Element<Message> = if state.status.is_empty() {
        Space::new().into()
    } else {
        let tone = if state.status_error {
            t.error_color.iced_alpha(f)
        } else {
            muted
        };
        text(state.status.clone())
            .size(12.0 * t.font_scale)
            .color(tone)
            .into()
    };

    // A Caps Lock warning slips in under the password field only while it's on, so
    // there's no empty gap otherwise. Standard login-screen courtesy.
    let mut items: Vec<Element<Message>> = vec![header];
    if let Some(logo) = logo {
        items.push(logo);
    }
    if let Some(p) = &state.prompt {
        // An interactive second factor (e.g. a FIDO2 PIN): PAM's own label above a
        // focused field, replacing the login form for this round of the exchange.
        items.push(
            text(p.text.clone())
                .size(13.0 * t.font_scale)
                .color(fg)
                .into(),
        );
        let field = text_input("", &p.value)
            .id(PROMPT_ID)
            .on_input(Message::PromptChanged)
            .on_submit(Message::PromptSubmitted)
            .secure(p.secret)
            .padding(11)
            .size(15.0 * t.font_scale)
            .style(field_style(t, f));
        items.push(field.into());
        let submit = button(
            text("Submit")
                .width(Length::Fill)
                .center()
                .size(15.0 * t.font_scale),
        )
        .width(Length::Fill)
        .padding(11)
        .on_press(Message::PromptSubmitted)
        .style(button_style(t, f));
        items.push(submit.into());
    } else if let Some(cue) = &state.cue {
        // A one-way waiting cue (e.g. "Please touch the device"): a distinct
        // pulsing indicator so the moment reads as an action to take, not a
        // passive status line. The pulse rides the shared animation clock.
        let pulse = 0.55 + 0.45 * (state.anim * 2.4).sin().abs();
        items.push(
            column![
                text("◉")
                    .size(30.0 * t.font_scale)
                    .color(t.accent.iced_alpha(f * pulse)),
                text(cue.clone()).size(14.0 * t.font_scale).color(fg),
            ]
            .spacing(10)
            .align_x(Alignment::Center)
            .into(),
        );
    } else {
        items.push(username.into());
        items.push(password.into());
        if state.caps_lock {
            items.push(
                text("⇪  Caps Lock is on")
                    .size(12.0 * t.font_scale)
                    .color(t.error_color.iced_alpha(f))
                    .into(),
            );
        }
        // Optional keyboard-layout indicator (off by default): a muted hint so a
        // login on an unexpected layout is visible, not a silent auth failure.
        if t.show_kb_layout {
            if let Some(layout) = &state.kb_layout {
                items.push(
                    text(format!("⌨  {}", layout.to_uppercase()))
                        .size(12.0 * t.font_scale)
                        .color(muted)
                        .into(),
                );
            }
        }
        items.push(login.into());
    }
    items.push(picker.into());
    items.push(status);
    let form = Column::with_children(items)
        .spacing(12)
        .align_x(Alignment::Center);

    let card = container(form)
        .padding(26)
        .width(Length::Fixed(t.card_width))
        .style(card_style(t, f, state.anim));

    // Optional true backdrop blur: a frosted sample of the sky behind the card, drawn
    // *under* the translucent card (push_under keeps the card the size-defining base)
    // and masked to its rounded rect inside the shader. The GPU level can veto
    // this second full-screen pass on lower tiers, even if the user enabled it.
    let card: Element<Message> = if t.card_blur && t.gpu_level.allows_blur() {
        Stack::new()
            .push(card)
            .push_under(
                shader(FrostShader::from_theme(t, state.anim, f))
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .into()
    } else {
        card.into()
    };

    // Place the card per the theme; padding keeps edge placements off the bezel.
    let card_area = container(card).padding(48);
    let centered = match t.card_pos {
        CardPos::Center => card_area.center_x(Length::Fill).center_y(Length::Fill),
        CardPos::Left => card_area.align_left(Length::Fill).center_y(Length::Fill),
        CardPos::Right => card_area.align_right(Length::Fill).center_y(Length::Fill),
        CardPos::Top => card_area.center_x(Length::Fill).align_top(Length::Fill),
        CardPos::Bottom => card_area.center_x(Length::Fill).align_bottom(Length::Fill),
    };

    // Power controls: subtle, outside the card, bottom-right of the screen.
    let power = container(
        row![
            power_button("Suspend", PowerAction::Suspend, t, f),
            power_button("Restart", PowerAction::Reboot, t, f),
            power_button("Shut down", PowerAction::PowerOff, t, f),
        ]
        .spacing(4),
    )
    .align_right(Length::Fill)
    .align_bottom(Length::Fill)
    .padding(18);

    let overlay = stack![centered, power];

    // The animated sky (twinkling starfield + drifting comet) over the wallpaper —
    // only when animation is enabled; otherwise the still wallpaper shows through.
    let scene: Element<Message> = if t.animate {
        let sky = shader(SkyShader::from_theme(t, state.anim, f))
            .width(Length::Fill)
            .height(Length::Fill);
        stack![sky, overlay].into()
    } else {
        overlay.into()
    };

    // Wallpaper behind everything, if configured and it passed the pre-auth asset
    // vetting (a missing/refused file just leaves the solid window background).
    match &state.wallpaper {
        Some(path) => {
            let background = image(image::Handle::from_path(path))
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Cover);
            stack![background, scene].into()
        }
        None => scene,
    }
}

/// The slim session selector, matched to the field styling.
fn picker_style(
    t: &Theme,
    fade: f32,
) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style {
    let field = t.field.iced_alpha(fade);
    let fg = t.foreground.iced_alpha(fade);
    let muted = t.muted.iced_alpha(fade);
    let accent = t.accent.iced_alpha(fade);
    let radius = t.field_radius;
    move |_theme, status| {
        let focused = matches!(
            status,
            pick_list::Status::Hovered | pick_list::Status::Opened { .. }
        );
        pick_list::Style {
            text_color: fg,
            placeholder_color: muted,
            handle_color: muted,
            background: Background::Color(field),
            border: Border {
                radius: radius.into(),
                width: 1.0,
                color: if focused {
                    accent
                } else {
                    iced::Color::TRANSPARENT
                },
            },
        }
    }
}

/// A subtle, text-only power control that picks up the accent on hover.
fn power_button<'a>(
    label: &'a str,
    action: PowerAction,
    t: &Theme,
    fade: f32,
) -> Element<'a, Message> {
    let muted = t.muted.iced_alpha(fade);
    let accent = t.accent.iced_alpha(fade);
    button(text(label).size(12))
        .padding(8)
        .on_press(Message::PowerPressed(action))
        .style(move |_theme, status| button::Style {
            background: Some(Background::Color(iced::Color::TRANSPARENT)),
            text_color: if matches!(status, button::Status::Hovered) {
                accent
            } else {
                muted
            },
            border: Border {
                radius: 8.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// The subscription body: spawn the worker thread and stream its events. Created
/// once by Iced; `Subscription::run` keys it by this function so it is not
/// restarted on every frame.
fn daemon_worker() -> impl futures::Stream<Item = Message> {
    iced_futures::stream::channel(
        64,
        |mut output: futures::channel::mpsc::Sender<Message>| async move {
            let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
            let thread_output = output.clone();
            std::thread::spawn(move || worker_loop(cmd_rx, thread_output));
            // Hand the UI its command channel; then idle forever while the worker
            // pushes events through its own clone of `output`.
            let _ = output.send(Message::WorkerReady(cmd_tx)).await;
            std::future::pending::<()>().await;
        },
    )
}

/// The blocking worker: owns the protocol client and turns UI commands into
/// requests, emitting the daemon's responses back as [`Message`]s.
fn worker_loop(cmd_rx: mpsc::Receiver<Command>, mut out: futures::channel::mpsc::Sender<Message>) {
    let mut emit = |message: Message| {
        // Block the worker thread until Iced accepts the event (a tiny, bounded
        // wait); losing a prompt would hang the UI, so we never drop.
        let _ = futures::executor::block_on(out.send(message));
    };

    let socket = std::env::var("DOORD_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let mut client = match Client::connect(&socket) {
        Ok(client) => client,
        Err(e) => {
            emit(Message::Fatal(e.to_string()));
            return;
        }
    };

    match client.list_sessions() {
        Ok(sessions) => emit(Message::Sessions(sessions)),
        Err(e) => emit(Message::DaemonError(format!(
            "Could not list sessions: {e}"
        ))),
    }

    while let Ok(command) = cmd_rx.recv() {
        match command {
            Command::Authenticate { username } => {
                if let Err(e) = client.begin_auth(&username) {
                    emit(Message::DaemonError(e.to_string()));
                    continue;
                }
                run_conversation(&mut client, &cmd_rx, &mut emit);
            }
            Command::Start { session_id } => match client.start(&session_id) {
                Ok(StartOutcome::Started) => emit(Message::SessionStarted),
                Ok(StartOutcome::Refused(message)) => emit(Message::DaemonError(message)),
                Err(e) => emit(Message::Fatal(e.to_string())),
            },
            Command::Power(action) => match client.power(action) {
                // The daemon refused (e.g. not yet available); surface it.
                Ok(Some(message)) => emit(Message::DaemonError(message)),
                Ok(None) => {}
                Err(e) => emit(Message::DaemonError(e.to_string())),
            },
            // A stray reply outside a conversation: ignore.
            Command::Reply(_) => {}
        }
    }
}

/// Drive one authentication conversation: relay each PAM prompt as a [`Message`],
/// then block for the matching reply command, until a terminal verdict.
fn run_conversation(
    client: &mut Client,
    cmd_rx: &mpsc::Receiver<Command>,
    emit: &mut impl FnMut(Message),
) {
    loop {
        match client.recv_auth() {
            Ok(AuthStep::Question { text, secret }) => {
                emit(Message::Prompt { text, secret });
                // Wait for the UI to send the reply (or cancel).
                match cmd_rx.recv() {
                    Ok(Command::Reply(response)) => {
                        if let Err(e) = client.reply(response) {
                            emit(Message::DaemonError(e.to_string()));
                            return;
                        }
                    }
                    // The connection closed, or an out-of-band command: abort.
                    _ => return,
                }
            }
            Ok(AuthStep::Info(text)) | Ok(AuthStep::Error(text)) => emit(Message::Notice(text)),
            Ok(AuthStep::Success) => {
                emit(Message::AuthSucceeded);
                return;
            }
            Ok(AuthStep::Failure(reason)) => {
                emit(Message::AuthFailed(reason));
                return;
            }
            Err(e) => {
                emit(Message::Fatal(e.to_string()));
                return;
            }
        }
    }
}
