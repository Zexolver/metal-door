//! The lock UI: the door-theme surface rendered as `ext-session-lock-v1` lock
//! surfaces on the *user's* running compositor, wired to the daemon's verify-only
//! reauth seam.
//!
//! The app itself holds no authority — same posture as the greeter. A background
//! worker owns the reauth [`Client`](crate::client::Client) (blocking socket I/O)
//! and talks to this UI over two channels: the UI sends [`Command`]s to the
//! worker, and the worker emits [`Message`]s back through an Iced subscription.
//! Because the daemon drops peers that idle between attempts, the worker dials a
//! fresh connection per unlock attempt instead of holding one across the locked
//! wait.
//!
//! The locked surface mirrors the greeter's disclosure posture exactly: sky,
//! clock, the password/second-factor prompt, and the default-off keyboard-layout
//! and battery indicators — no notifications, media, or session content, ever.
//! There is deliberately no session picker, no username field (the daemon binds
//! the reauth target to this process's peer-cred uid), and no power row (the
//! reauth seam has no power verb).
//!
//! Crash-safety comes from the protocol, not from this process: the compositor
//! owns the blanking, so if door-lock dies the screen stays covered — the
//! failure mode is "stays locked," never "reveals."

use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::SinkExt;
use iced::widget::{
    canvas, column, container, image, shader, stack, svg, text, text_input, Column, Space, Stack,
};
use iced::{
    keyboard, Alignment, Background, Border, ContentFit, Element, Font, Length, Subscription, Task,
};
use iced_sessionlock::actions::UnLockAction;

use protocol::Secret;

use crate::client::{Client, ReauthStep, DEFAULT_REAUTH_SOCKET};
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

/// Run the locker: `ext-session-lock-v1` lock surfaces on every output of the
/// user's running compositor (the compositor blanks anything left uncovered, so
/// the protocol's natural shape is all-outputs — no primary-only bound here).
///
/// Dev (`DOOR_LOCK_DEV` set): a normal window instead — **no lock protocol at
/// all** — so the surface can be exercised nested in any session without locking
/// it. "Unlock" then just exits.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Render in the themed font + weight, exactly like the greeter.
    let mut font = match theme().font.clone() {
        Some(name) => Font::with_name(Box::leak(name.into_boxed_str())),
        None => Font::DEFAULT,
    };
    font.weight = theme().font_weight.iced();

    if std::env::var_os("DOOR_LOCK_DEV").is_some() {
        iced::application(boot, update, dev_view)
            .title("door-lock (dev)")
            // No client-side decorations: under a host that draws none, winit's
            // CSD fallback would shrink the content buffer — same fix as the greeter.
            .window(iced::window::Settings {
                decorations: false,
                ..Default::default()
            })
            .style(app_style)
            .subscription(subscription)
            .default_font(font)
            .run()?;
        return Ok(());
    }

    // The honest-bound decline, made graceful: a compositor without
    // ext-session-lock-v1 (KWin and Mutter ship their own lockers and do not
    // expose the protocol to third-party clients) gets a clear refusal here —
    // never a half-locked screen, and not a panic out of the lock shell.
    if !compositor_supports_session_lock() {
        return Err(
            "this compositor does not offer ext-session-lock-v1, so door-lock \
                    cannot lock it. Supported: sway, Hyprland, river, niri, labwc, \
                    Wayfire, COSMIC, Weston 12+, ... KWin and GNOME ship their own \
                    lockers and do not accept third-party ones."
                .into(),
        );
    }

    iced_sessionlock::application(boot, update, view)
        .style(app_style)
        .subscription(subscription)
        .default_font(font)
        .run()?;
    Ok(())
}

/// Whether the compositor advertises `ext_session_lock_manager_v1`: one registry
/// round-trip on a throwaway connection, before the lock shell starts.
fn compositor_supports_session_lock() -> bool {
    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::wl_registry::WlRegistry,
        Connection, Dispatch, QueueHandle,
    };

    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(
            _state: &mut Self,
            _proxy: &WlRegistry,
            _event: <WlRegistry as wayland_client::Proxy>::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qhandle: &QueueHandle<Self>,
        ) {
        }
    }

    let Ok(conn) = Connection::connect_to_env() else {
        // No display at all — let the lock shell surface its own connect error.
        return true;
    };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&conn) else {
        return true;
    };
    globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == "ext_session_lock_manager_v1")
    })
}

/// Window-level appearance from the theme: the solid background (also shown at any
/// wallpaper letterbox edge) and the default text color.
fn app_style(state: &State, _theme: &iced::Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: state.theme.background.iced(),
        text_color: state.theme.foreground.iced(),
    }
}

/// The locker's subscriptions: the reauth worker event stream, a 1 Hz clock, the
/// one-shot launch fade, and the animation ticker.
fn subscription(state: &State) -> Subscription<Message> {
    let mut subs = vec![
        Subscription::run(reauth_worker),
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
    // Animation rides a fixed-rate ticker at the GPU level's cap (60 when the tier
    // is uncapped). The greeter's uncapped tier rides the compositor's vsync stream
    // instead, but that subscription is winit-shaped; under the session-lock shell
    // the fixed ticker is the reliable clock, and `anim` is recomputed from real
    // elapsed time so motion stays time-correct either way.
    if state.theme.animate {
        subs.push(Subscription::run(frame_ticker));
    }
    Subscription::batch(subs)
}

/// Emit a [`Message::Tick`] once a second so the card clock stays current. A small
/// thread sleeps and pushes through the Iced channel (same shape as the reauth
/// worker) — avoids pulling an async timer/runtime feature onto the auth surface.
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

/// Drive the launch fade-in: ~16 ms [`Message::Fade`] steps over the configured
/// fade, then the stream ends (bounded — no perpetual repaint while locked idle).
fn fade_ticker() -> impl futures::Stream<Item = Message> {
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

/// Emit [`Message::AnimTick`] at a fixed frame rate — the GPU level's cap, or 60
/// for the uncapped tier. `anim` is recomputed from real elapsed time each tick,
/// so motion stays time-correct regardless of the rate.
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

/// Where the locker is in the unlock flow.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Locked, ready for the user to enter credentials.
    Locked,
    /// A reauthentication is in flight (waiting on PAM prompts / verdict).
    Authenticating,
    /// The daemon said Allow; the unlock is being handed to the compositor.
    Unlocking,
}

/// What the UI asks the worker to do.
#[derive(Debug)]
pub enum Command {
    /// Dial the reauth socket and run one verify-only attempt for our own uid.
    Authenticate,
    /// Answer the most recent PAM prompt.
    Reply(Secret),
}

#[derive(Debug, Clone)]
pub enum Message {
    // From the worker:
    WorkerReady(mpsc::Sender<Command>),
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
    DaemonError(String),
    Fatal(String),
    /// 1 Hz clock tick — refreshes the card clock + date.
    Tick,
    /// One step of the launch fade-in.
    Fade,
    /// Continuous ambient animation tick (twinkle + breathing card edge).
    AnimTick,
    /// Tab / Shift-Tab focus traversal across the fields and unlock.
    FocusNext,
    FocusPrev,
    /// CapsLock was pressed/released — re-read the lock state.
    CapsLockChanged,
    // From the UI:
    PasswordChanged(String),
    UnlockPressed,
    /// Hand the unlock to the session-lock shell (it converts this to the
    /// protocol's unlock-and-exit). In dev mode it just exits.
    UnLock,
}

/// The session-lock shell intercepts task-emitted messages that convert into
/// [`UnLockAction`] and performs the protocol unlock + exit; everything else
/// flows to `update` as usual.
impl TryInto<UnLockAction> for Message {
    type Error = Self;

    fn try_into(self) -> Result<UnLockAction, Self::Error> {
        match self {
            Message::UnLock => Ok(UnLockAction),
            other => Err(other),
        }
    }
}

/// An interactive PAM prompt whose reply the locker must collect by typing —
/// a second factor the pre-typed password can't answer, e.g. a FIDO2 PIN in a
/// passwordless setup, or any prompt after the first. The password (the first
/// secret prompt, pre-typed into the unlock form) is still auto-answered and
/// never routes through here. `value` holds the in-progress input and is moved
/// into a zeroizing [`Secret`] the instant it is submitted.
struct PromptField {
    text: String,
    secret: bool,
    value: String,
}

struct State {
    phase: Phase,
    /// Whether we are running as a plain window (`DOOR_LOCK_DEV`) instead of
    /// session-lock surfaces — flips what [`Message::UnLock`] does.
    dev: bool,
    /// The user being reauthenticated — display only. The daemon derives the real
    /// target from this process's peer-cred uid; this string never crosses the seam.
    username: String,
    password: String,
    status: String,
    /// Whether `status` is an error (attempt failed, daemon unreachable) — colored
    /// with the theme's `error_color` rather than the muted tone.
    status_error: bool,
    cmd_tx: Option<mpsc::Sender<Command>>,
    /// The resolved look (a clone of the process-wide [`theme`]).
    theme: Theme,
    /// The theme's wallpaper/logo paths *after* the trust + size vetting
    /// (`vet_theme_assets`) — `None` where the configured asset was refused. Vetted
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
    /// When the locker started, the zero point for `anim`.
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
    /// mid-conversation asking for input the unlock form didn't already supply.
    prompt: Option<PromptField>,
    /// A one-way PAM cue shown mid-authentication — e.g. "Please touch the
    /// device" from `pam_u2f cue`. Rendered as a distinct waiting indicator, not
    /// a plain status line, so a hardware-key touch reads as an action to take.
    cue: Option<String>,
}

/// The display username for the card — `$USER`, else a passwd lookup of our own
/// uid. Display only; the daemon never sees it (the seam carries no identity).
fn display_username() -> String {
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            return user;
        }
    }
    // SAFETY: getpwuid(getuid()) reads the calling process's own passwd entry; the
    // returned struct is a static-lifetime C buffer we only copy the name out of.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_name.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned();
        }
    }
    String::new()
}

impl State {
    fn new() -> Self {
        // Pick the day or night variant by the local clock, exactly like the greeter.
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
            phase: Phase::Locked,
            dev: std::env::var_os("DOOR_LOCK_DEV").is_some(),
            username: display_username(),
            password: String::new(),
            status: String::new(),
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
                    self.status =
                        "The unlock worker is gone — switch to a TTY to recover.".to_string();
                    self.status_error = true;
                    self.phase = Phase::Locked;
                }
            }
            None => self.status = "Not ready yet.".to_string(),
        }
    }
}

/// Boot the app state and land keyboard focus in the password field, so a
/// walk-up user can just start typing.
fn boot() -> (State, Task<Message>) {
    (State::new(), iced::widget::operation::focus(PASSWORD_ID))
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    let mut task = Task::none();
    match message {
        Message::WorkerReady(tx) => {
            state.cmd_tx = Some(tx);
        }
        Message::Prompt { text, secret } => {
            // A fresh prompt supersedes any prior touch cue.
            state.cue = None;
            if secret && !state.password.is_empty() {
                // Common case: the user pre-typed their password into the unlock
                // form, so answer PAM's password prompt instantly — no extra
                // field, the smooth path stays smooth.
                state.status = text;
                let reply = Secret::new(std::mem::take(&mut state.password));
                state.send(Command::Reply(reply));
            } else {
                // A prompt the unlock form can't answer: a FIDO2 PIN, a second
                // factor, or the first prompt of a passwordless stack. Surface
                // PAM's own text and collect the typed reply interactively.
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
            state.phase = Phase::Unlocking;
            state.status = "Unlocked.".to_string();
            state.status_error = false;
            // Emit the unlock as a task so the session-lock shell intercepts it
            // (TryInto<UnLockAction>) and performs the protocol unlock + exit.
            task = Task::done(Message::UnLock);
        }
        Message::AuthFailed(reason) => {
            state.password.clear();
            state.prompt = None;
            state.cue = None;
            state.status = reason;
            state.status_error = true;
            state.phase = Phase::Locked;
            task = iced::widget::operation::focus(PASSWORD_ID);
        }
        Message::DaemonError(message) | Message::Fatal(message) => {
            // Both are per-attempt failures here: the worker dials fresh each try,
            // so an unreachable daemon is retryable (and the TTY path is the
            // documented recovery if it stays unreachable).
            state.password.clear();
            state.prompt = None;
            state.cue = None;
            state.status = message;
            state.status_error = true;
            state.phase = Phase::Locked;
        }
        Message::PasswordChanged(value) => state.password = value,
        Message::UnlockPressed => {
            if matches!(state.phase, Phase::Locked) {
                state.status_error = false;
                state.phase = Phase::Authenticating;
                state.status = "Authenticating…".to_string();
                // A fresh conversation: drop any stale prompt/cue from a prior try.
                state.prompt = None;
                state.cue = None;
                state.send(Command::Authenticate);
            }
        }
        Message::UnLock => {
            // Reached only in dev mode (the shell intercepts it in production —
            // and in production a stray UnLock must NOT unlock from `update`, so
            // re-emitting it as a task is the only correct handling either way).
            if state.dev {
                task = iced::exit();
            } else {
                task = Task::done(Message::UnLock);
            }
        }
        Message::Tick => {
            // Live config hot-reload, exactly like the greeter: re-resolve the theme
            // each second and swap it only if it changed.
            let fresh = Theme::load_at(now_minutes(), now_month());
            if fresh != state.theme {
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
            // locker lacked focus (the key-event path catches the focused case).
            if let Some(on) = caps_lock_on() {
                state.caps_lock = on;
            }
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

/// Stable id for the password field, so boot and a failed attempt can land
/// keyboard focus where the user will type.
const PASSWORD_ID: &str = "unlock-password";

/// Stable id for the interactive second-factor field, so `update` can focus it
/// the moment PAM issues a prompt the unlock form didn't pre-answer.
const PROMPT_ID: &str = "pam-prompt";

/// The per-output view: every lock surface renders the same card (state is
/// shared), so a multi-monitor lock shows the themed surface everywhere —
/// uncovered outputs would be blanked by the compositor anyway.
fn view(state: &State, _window: iced::window::Id) -> Element<'_, Message> {
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
    // Optional battery indicator (off by default), carried over from the greeter
    // per the shared disclosure posture. `⚡` marks charging/full.
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

    let busy = matches!(state.phase, Phase::Authenticating | Phase::Unlocking);

    let password = text_input("password", &state.password)
        .id(PASSWORD_ID)
        .on_input(Message::PasswordChanged)
        .on_submit(Message::UnlockPressed)
        .secure(true)
        .padding(11)
        .size(15.0 * t.font_scale)
        .style(field_style(t, f));

    let mut unlock = iced::widget::button(
        text("Unlock")
            .width(Length::Fill)
            .center()
            .size(15.0 * t.font_scale),
    )
    .width(Length::Fill)
    .padding(11)
    .style(button_style(t, f));
    if !busy {
        unlock = unlock.on_press(Message::UnlockPressed);
    }

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

    let mut items: Vec<Element<Message>> = vec![header];
    if let Some(logo) = logo {
        items.push(logo);
    }
    // Who is locked — display only (the daemon binds the real identity to this
    // process's uid; there is deliberately no way to switch users here).
    if !state.username.is_empty() {
        items.push(
            text(state.username.clone())
                .size(13.0 * t.font_scale)
                .color(muted)
                .into(),
        );
    }
    if let Some(p) = &state.prompt {
        // An interactive second factor (e.g. a FIDO2 PIN): PAM's own label above a
        // focused field, replacing the unlock form for this round of the exchange.
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
        let submit = iced::widget::button(
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
        items.push(password.into());
        if state.caps_lock {
            items.push(
                text("⇪  Caps Lock is on")
                    .size(12.0 * t.font_scale)
                    .color(t.error_color.iced_alpha(f))
                    .into(),
            );
        }
        // Optional keyboard-layout indicator (off by default), carried over from
        // the greeter: an unlock on an unexpected layout is visible, not a silent
        // auth failure.
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
        items.push(unlock.into());
    }
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

    // No power row here: the reauth seam has no power verb, and a locked session
    // with unsaved work is not the place to offer one-click poweroff.

    // The animated sky over the wallpaper — only when animation is enabled;
    // otherwise the still wallpaper shows through.
    let scene: Element<Message> = if t.animate {
        let sky = shader(SkyShader::from_theme(t, state.anim, f))
            .width(Length::Fill)
            .height(Length::Fill);
        stack![sky, centered].into()
    } else {
        centered.into()
    };

    // Wallpaper behind everything, if configured and it passed the asset vetting
    // (a missing/refused file just leaves the solid window background).
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

/// Dev-mode view: the same surface in a plain window (no lock protocol).
fn dev_view(state: &State) -> Element<'_, Message> {
    view(state, iced::window::Id::unique())
}

/// The subscription body: spawn the worker thread and stream its events. Created
/// once by Iced; `Subscription::run` keys it by this function so it is not
/// restarted on every frame.
fn reauth_worker() -> impl futures::Stream<Item = Message> {
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

/// The blocking worker: dials the reauth socket fresh for each attempt (the
/// daemon drops peers that idle between attempts), drives the verify-only
/// conversation, and emits the verdict back as [`Message`]s.
fn worker_loop(cmd_rx: mpsc::Receiver<Command>, mut out: futures::channel::mpsc::Sender<Message>) {
    let mut emit = |message: Message| {
        // Block the worker thread until Iced accepts the event (a tiny, bounded
        // wait); losing a prompt would hang the UI, so we never drop.
        let _ = futures::executor::block_on(out.send(message));
    };

    let socket =
        std::env::var("DOORD_REAUTH_SOCKET").unwrap_or_else(|_| DEFAULT_REAUTH_SOCKET.to_string());

    while let Ok(command) = cmd_rx.recv() {
        match command {
            Command::Authenticate => {
                // Fresh connection per attempt; dropped (closed) after the verdict.
                let mut client = match Client::connect(&socket) {
                    Ok(client) => client,
                    Err(e) => {
                        emit(Message::Fatal(format!("Cannot reach doord: {e}")));
                        continue;
                    }
                };
                if let Err(e) = client.begin() {
                    emit(Message::DaemonError(e.to_string()));
                    continue;
                }
                run_conversation(&mut client, &cmd_rx, &mut emit);
            }
            // A stray reply outside a conversation: ignore.
            Command::Reply(_) => {}
        }
    }
}

/// Drive one reauthentication conversation: relay each PAM prompt as a
/// [`Message`], then block for the matching reply command, until a terminal
/// verdict (Allow / Deny) or a transport error.
fn run_conversation(
    client: &mut Client,
    cmd_rx: &mpsc::Receiver<Command>,
    emit: &mut impl FnMut(Message),
) {
    loop {
        match client.recv_step() {
            Ok(ReauthStep::Question { text, secret }) => {
                emit(Message::Prompt { text, secret });
                // Wait for the UI to send the reply (or give up).
                match cmd_rx.recv() {
                    Ok(Command::Reply(response)) => {
                        if let Err(e) = client.reply(response) {
                            emit(Message::DaemonError(e.to_string()));
                            return;
                        }
                    }
                    // The channel closed, or an out-of-band command: abort.
                    _ => return,
                }
            }
            Ok(ReauthStep::Info(text)) | Ok(ReauthStep::Error(text)) => emit(Message::Notice(text)),
            Ok(ReauthStep::Allow) => {
                emit(Message::AuthSucceeded);
                return;
            }
            Ok(ReauthStep::Deny(reason)) => {
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
