//! Command line, deliberately tiny and hand-rolled (no `clap` in the pre-auth TCB).
//!
//! The shape is `cage`-compatible on purpose: `doorstep [OPTIONS] -- COMMAND [ARGS]`.
//! Swapping the binary name in `DOORD_GREETER_CMD` is the whole migration, and
//! `cage`'s `-d`/`-s` are accepted so an existing invocation keeps working.

use std::fmt;

/// Which display backend to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Pick `Nested` when a parent compositor is visible, `Udev` otherwise.
    Auto,
    /// DRM/KMS on a VT — the production path.
    Udev,
    /// A window inside an existing compositor — development only.
    Nested,
}

/// Parsed invocation.
#[derive(Debug, Clone)]
pub struct Args {
    /// The client to host, argv[0] first. Never empty.
    pub command: Vec<String>,
    pub backend: Backend,
    /// Honour `Ctrl+Alt+F<n>` by switching VTs (default true — door's documented
    /// recovery path tells users to reach a TTY *from the login screen*).
    pub vt_switch: bool,
    /// Verbose logging on stderr.
    pub debug: bool,
}

/// Why a command line was rejected, or that it asked for help/version.
pub enum ParseOutcome {
    Run(Args),
    Help,
    Version,
    Error(String),
}

pub const USAGE: &str = "\
doorstep — door's kiosk compositor: hosts exactly one fullscreen Wayland client.

Usage:
  doorstep [OPTIONS] -- COMMAND [ARGS...]
  doorstep [OPTIONS] COMMAND [ARGS...]

Options:
  -d, --debug            Verbose logging on stderr.
  -s, --allow-vt-switch  Accepted for cage compatibility (VT switching is on by
                         default; use --no-vt-switch to turn it off).
      --no-vt-switch     Ignore Ctrl+Alt+F<n>. Note that this removes the
                         documented TTY recovery path from the login screen.
      --backend BACKEND  auto (default), udev, or nested.
  -h, --help             Show this help.
  -V, --version          Show the version.

Everything after the first non-option argument (or after `--`) is the command.
";

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Backend::Auto => "auto",
            Backend::Udev => "udev",
            Backend::Nested => "nested",
        })
    }
}

/// Parse an argv tail (everything after the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> ParseOutcome {
    let mut backend = Backend::Auto;
    let mut vt_switch = true;
    let mut debug = false;

    let mut it = args.into_iter().peekable();
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                command.extend(it);
                break;
            }
            "-h" | "--help" => return ParseOutcome::Help,
            "-V" | "--version" => return ParseOutcome::Version,
            "-d" | "--debug" => debug = true,
            // cage spells "allow VT switching" `-s`; doorstep already does, so the
            // flag is accepted and does nothing rather than being an error.
            "-s" | "--allow-vt-switch" => vt_switch = true,
            "--no-vt-switch" => vt_switch = false,
            "--backend" => match it.next().as_deref() {
                Some("auto") => backend = Backend::Auto,
                Some("udev") => backend = Backend::Udev,
                Some("nested") => backend = Backend::Nested,
                Some(other) => {
                    return ParseOutcome::Error(format!("unknown backend `{other}`"));
                }
                None => return ParseOutcome::Error("--backend needs a value".into()),
            },
            // A bare `-` or anything else starting with `-` is a flag we do not
            // know. Refusing beats silently handing it to the client: this process
            // runs before authentication and should have no ambiguous inputs.
            other if other.starts_with('-') && other.len() > 1 => {
                return ParseOutcome::Error(format!("unknown option `{other}`"));
            }
            other => {
                command.push(other.to_string());
                command.extend(it);
                break;
            }
        }
    }

    if command.is_empty() {
        return ParseOutcome::Error("no command given".into());
    }

    ParseOutcome::Run(Args {
        command,
        backend,
        vt_switch,
        debug,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(argv: &[&str]) -> Args {
        match parse(argv.iter().map(|s| s.to_string())) {
            ParseOutcome::Run(args) => args,
            ParseOutcome::Error(err) => panic!("unexpected parse error: {err}"),
            _ => panic!("unexpected help/version"),
        }
    }

    fn err(argv: &[&str]) -> String {
        match parse(argv.iter().map(|s| s.to_string())) {
            ParseOutcome::Error(err) => err,
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn cage_style_invocation_parses() {
        // The exact shape of the default DOORD_GREETER_CMD.
        let args = run(&["--", "/usr/bin/door-greeter"]);
        assert_eq!(args.command, vec!["/usr/bin/door-greeter".to_string()]);
        assert_eq!(args.backend, Backend::Auto);
        assert!(args.vt_switch);
    }

    #[test]
    fn cage_flags_are_accepted() {
        let args = run(&["-d", "-s", "--", "greeter", "--flag"]);
        assert!(args.debug);
        assert!(args.vt_switch);
        assert_eq!(
            args.command,
            vec!["greeter".to_string(), "--flag".to_string()]
        );
    }

    #[test]
    fn command_flags_after_the_command_are_not_ours() {
        // Everything from the command onward belongs to the client, including
        // things that look like doorstep's own options.
        let args = run(&["greeter", "--backend", "udev"]);
        assert_eq!(args.backend, Backend::Auto);
        assert_eq!(
            args.command,
            vec![
                "greeter".to_string(),
                "--backend".to_string(),
                "udev".to_string()
            ]
        );
    }

    #[test]
    fn unknown_options_and_empty_commands_are_refused() {
        assert!(err(&["--wat", "greeter"]).contains("--wat"));
        assert!(err(&[]).contains("no command"));
        assert!(err(&["-d", "--"]).contains("no command"));
        assert!(err(&["--backend", "wayland", "--", "g"]).contains("wayland"));
    }

    #[test]
    fn vt_switching_can_be_turned_off() {
        assert!(!run(&["--no-vt-switch", "--", "greeter"]).vt_switch);
    }
}
