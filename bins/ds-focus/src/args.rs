//! Hand-rolled argument parsing for `ds-focus` (grammar: `docs/components/focus-sender.md`).
//!
//! Flags may appear anywhere and support the `--flag=value` form. `--interval`/`--settle`
//! take milliseconds, with an optional `ms` suffix (`250`, `250ms`).

use std::path::PathBuf;
use std::time::Duration;

/// The `--backend` vocabulary. `x11` is accepted so scripts can name it, but the alpha
/// build ships the two Wayland backends (KDE-first, `docs/40-platforms.md` §1); selecting
/// `x11` exits 3 with the list of backends that are actually available.
pub const BACKEND_NAMES: [&str; 4] = ["auto", "kwin", "wlr", "x11"];

/// Default poll interval of a live backend (ms).
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(250);
/// Default settle time: focus churn is coalesced for this long before emitting (ms)
/// — the `FOCUS_SETTLE = 0.3` precedent of `docs/components/focus-sender.md`.
pub const DEFAULT_SETTLE: Duration = Duration::from_millis(300);

/// A requested backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Kwin,
    Wlr,
    X11,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Self, UsageError> {
        match s {
            "auto" => Ok(Self::Auto),
            "kwin" => Ok(Self::Kwin),
            "wlr" => Ok(Self::Wlr),
            "x11" => Ok(Self::X11),
            other => Err(UsageError(format!(
                "unknown backend '{other}' (valid: {})",
                BACKEND_NAMES.join(", ")
            ))),
        }
    }
}

/// A parsed configuration. `interval`/`settle` are `None` until resolved (CLI flag >
/// `/settings/senders/focus` > default); see [`Config::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub backend: Backend,
    pub interval: Option<Duration>,
    pub settle: Option<Duration>,
    pub once: bool,
    /// Raw `--inject` argument: an inline JSON payload (object, or array of objects) or a
    /// path to an `.ndjson` file with one payload object per line.
    pub inject: Option<String>,
    pub socket: Option<PathBuf>,
    pub token_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub quiet: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            interval: None,
            settle: None,
            once: false,
            inject: None,
            socket: None,
            token_file: None,
            data_dir: None,
            quiet: false,
        }
    }
}

impl Config {
    /// Resolve the effective poll interval / settle time: CLI flag, else the daemon's
    /// `/settings/senders/focus` object (`interval_ms` / `settle_ms`), else the defaults.
    pub fn resolve(&self, store: Option<&serde_json::Value>) -> (Duration, Duration) {
        let from_store = |key: &str| {
            store
                .and_then(|s| s.get(key))
                .and_then(|v| v.as_u64())
                .and_then(u64_to_duration)
        };
        let interval = self
            .interval
            .or_else(|| from_store("interval_ms"))
            .unwrap_or(DEFAULT_INTERVAL);
        let settle = self
            .settle
            .or_else(|| from_store("settle_ms"))
            .unwrap_or(DEFAULT_SETTLE);
        (interval, settle)
    }
}

fn u64_to_duration(ms: u64) -> Option<Duration> {
    if ms > 600_000 {
        return None;
    }
    Some(Duration::from_millis(ms))
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Run(Config),
    Help,
    Version,
}

/// A usage error: printed as `ds-focus: <msg>  [exit 1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

/// Parse milliseconds with an optional `ms` suffix: `250`, `250ms`.
pub fn parse_ms(flag: &str, s: &str) -> Result<Duration, UsageError> {
    let digits = s.strip_suffix("ms").unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UsageError(format!(
            "{flag} needs milliseconds, e.g. 250 or 250ms (got {s:?})"
        )));
    }
    let ms: u64 = digits
        .parse()
        .map_err(|_| UsageError(format!("{flag} is too large: {s:?}")))?;
    if ms == 0 {
        return Err(UsageError(format!("{flag} must be > 0 (got {s:?})")));
    }
    if ms > 600_000 {
        return Err(UsageError(format!(
            "{flag} must be <= 600000 ms (got {s:?})"
        )));
    }
    Ok(Duration::from_millis(ms))
}

fn value_of(
    args: &[String],
    i: &mut usize,
    inline: &Option<String>,
    flag: &str,
) -> Result<String, UsageError> {
    if let Some(v) = inline {
        return Ok(v.clone());
    }
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| UsageError(format!("{flag} needs a value")))
}

/// Parse argv (without argv[0]).
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let mut cfg = Config::default();
    let mut help = false;
    let mut version = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        match flag {
            "--backend" => {
                cfg.backend = Backend::parse(&value_of(args, &mut i, &inline, flag)?)?;
            }
            "--interval" => {
                cfg.interval = Some(parse_ms(flag, &value_of(args, &mut i, &inline, flag)?)?);
            }
            "--settle" => {
                cfg.settle = Some(parse_ms(flag, &value_of(args, &mut i, &inline, flag)?)?);
            }
            "--once" => cfg.once = true,
            "--inject" => {
                cfg.inject = Some(value_of(args, &mut i, &inline, flag)?);
            }
            "--socket" => {
                cfg.socket = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?));
            }
            "--token-file" => {
                cfg.token_file = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?));
            }
            "--data-dir" => {
                cfg.data_dir = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?));
            }
            "--quiet" => cfg.quiet = true,
            "-h" | "--help" => help = true,
            "-V" | "--version" => version = true,
            _ if flag.starts_with('-') && flag.len() > 1 => {
                return Err(UsageError(format!(
                    "unknown argument '{arg}' (try `ds-focus --help`)"
                )));
            }
            _ => {
                return Err(UsageError(format!(
                    "unexpected argument '{arg}' — ds-focus takes no positional arguments (try `ds-focus --help`)"
                )));
            }
        }
        i += 1;
    }
    if help {
        return Ok(Command::Help);
    }
    if version {
        return Ok(Command::Version);
    }
    if cfg.once && cfg.inject.is_some() {
        return Err(UsageError(
            "--once and --inject cannot be combined (--inject is already one-shot)".to_string(),
        ));
    }
    Ok(Command::Run(cfg))
}

pub const HELP: &str = "\
ds-focus — watch the focused window and emit app/focus/changed to ds-daemon (DS-060)

Usage:
  ds-focus [--backend auto|kwin|wlr|x11] [--interval 250ms] [--settle 300ms]
           [--once] [--inject <json|file.ndjson>]
           [--socket PATH] [--token-file PATH] [--data-dir DIR] [--quiet]
  ds-focus --help | ds-focus --version

What it does:
  polls the session's active window and emits ONE event per settled focus change on
  the daemon's event bus (mutate scope needed):
      {\"op\":\"event\",\"topic\":\"app/focus/changed\",\"path\":\"/app/focus/changed\",
       \"ts_ms\":…,\"payload\":{\"app\":\"<wm-class>\",\"title\":\"<title>\",\"pid\":…}}
  `pid` is included when the backend knows it. ds-focus NEVER writes /focus directly —
  the daemon mirrors the payload into /focus in the same transaction that dispatches
  bindings (S7, DS-044).

Backends (autodetect order):
  kwin   KDE Plasma (Wayland or X11) — D-Bus org.kde.KWin /KWin org.kde.KWin
         queryWindowInfo (the active-window interface, verified on Bazzite Plasma 6)
  wlr    wlroots compositors (sway/Hyprland/…) — zwlr_foreign_toplevel_manager_v1
         (ext-foreign-toplevel-list-v1 carries no activation state, so it cannot drive
         focus on its own; zwlr is the wlr-foreign-toplevel protocol with `activated`)
  auto   kwin when a KDE session bus with org.kde.KWin is present, else wlr on Wayland
  x11    accepted but not built in the Linux alpha (Wayland/KDE-first); exits 3

Flags:
  --backend NAME     auto (default) | kwin | wlr | x11
  --interval MS      live-backend poll interval, default 250ms
  --settle MS        coalesce focus churn for this long before emitting, default 300ms
                     (FOCUS_SETTLE precedent); emit happens at the settle deadline, so a
                     settled change is emitted within the 500 ms latency budget
  --once             emit the current focus once and exit (no settle wait)
  --inject ARG       test hook: bypass the backend and emit ARG verbatim — an inline
                     JSON object, an array of objects, or a path to an .ndjson file with
                     one payload object per line. Emitted immediately (deduped), exit 0
  --socket PATH      control socket (default $XDG_RUNTIME_DIR/deckstack/daemon.sock)
  --token-file PATH  token file (default <socket dir>/token, 0600)
  --data-dir DIR     data dir (only used to resolve defaults)
  --quiet            suppress info logs (emits are still logged at info unless --quiet)
  -h, --help         this help
  -V, --version      print the version

Config: /settings/senders/focus in the daemon state may carry {\"backend\":…,
  \"interval_ms\":…,\"settle_ms\":…}; CLI flags win over it.

Exit codes: 0 ok · 1 usage · 3 no usable backend (message lists the available ones) ·
  4 E_AUTH/E_SCOPE/E_CAP · 6 E_UNREACHABLE (daemon not running; retried with backoff for
  60 s first) · 8 E_INTERNAL/E_VERSION.

Examples:
  ds-focus                                  # watch + emit on the default socket
  ds-focus --backend auto --settle 300ms
  ds-focus --once                           # one-shot snapshot of the focused window
  ds-focus --inject '{\"app\":\"code\",\"title\":\"x\"}'   # conformance test hook
  ds watch /app/focus/changed --ndjson      # observe the events
";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn parse_ok(v: &[&str]) -> Config {
        match parse(&args(v)) {
            Ok(Command::Run(c)) => c,
            other => panic!("{}: {other:?}", v.join(" ")),
        }
    }

    fn parse_err(v: &[&str]) -> String {
        parse(&args(v)).unwrap_err().0
    }

    #[test]
    fn defaults() {
        let c = parse_ok(&[]);
        assert_eq!(c, Config::default());
        assert_eq!(c.backend, Backend::Auto);
        assert!(!c.once);
        assert!(c.inject.is_none());
    }

    #[test]
    fn parses_all_flags_and_equals_form() {
        let c = parse_ok(&[
            "--backend=wlr",
            "--interval",
            "100ms",
            "--once",
            "--socket",
            "/tmp/s.sock",
            "--token-file=/tmp/t",
            "--data-dir",
            "/tmp/d",
            "--quiet",
        ]);
        assert_eq!(c.backend, Backend::Wlr);
        assert_eq!(c.interval, Some(Duration::from_millis(100)));
        assert_eq!(c.socket, Some(PathBuf::from("/tmp/s.sock")));
        assert_eq!(c.token_file, Some(PathBuf::from("/tmp/t")));
        assert_eq!(c.data_dir, Some(PathBuf::from("/tmp/d")));
        assert!(c.quiet);
        // --settle 0 is a usage error, so use 1ms here
        let c2 = parse_ok(&["--settle=1", "--once"]);
        assert_eq!(c2.settle, Some(Duration::from_millis(1)));
        assert!(c2.once);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(
            parse_ms("--interval", "250").unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_ms("--interval", "250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(parse_ms("--settle", "1").unwrap(), Duration::from_millis(1));
        for bad in ["", "ms", "-5", "1.5", "abc", "250s", "0"] {
            assert!(parse_ms("--interval", bad).is_err(), "{bad}");
        }
        assert!(parse_ms("--interval", "600001").is_err());
        assert_eq!(
            parse_ms("--interval", "600000").unwrap(),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn backend_vocabulary() {
        assert_eq!(Backend::parse("auto").unwrap(), Backend::Auto);
        assert_eq!(Backend::parse("kwin").unwrap(), Backend::Kwin);
        assert_eq!(Backend::parse("wlr").unwrap(), Backend::Wlr);
        assert_eq!(Backend::parse("x11").unwrap(), Backend::X11);
        let e = Backend::parse("win").unwrap_err().0;
        assert!(e.contains("unknown backend 'win'"), "{e}");
        assert!(e.contains("auto, kwin, wlr, x11"), "{e}");
    }

    #[test]
    fn usage_errors() {
        assert!(parse_err(&["--bogus"]).contains("unknown argument"));
        assert!(parse_err(&["positional"]).contains("no positional arguments"));
        assert!(parse_err(&["--backend"]).contains("needs a value"));
        assert!(parse_err(&["--backend", "nope"]).contains("unknown backend"));
        assert!(parse_err(&["--interval"]).contains("needs a value"));
        assert!(parse_err(&["--interval", "abc"]).contains("needs milliseconds"));
        assert!(parse_err(&["--once", "--inject", "{}"]).contains("cannot be combined"));
        assert!(matches!(parse(&args(&["--help"])).unwrap(), Command::Help));
        assert!(matches!(parse(&args(&["-h"])).unwrap(), Command::Help));
        assert!(matches!(
            parse(&args(&["--version"])).unwrap(),
            Command::Version
        ));
        assert!(matches!(parse(&args(&["-V"])).unwrap(), Command::Version));
    }

    #[test]
    fn inject_arg_is_kept_verbatim() {
        let c = parse_ok(&["--inject", "{\"app\":\"code\",\"title\":\"x\"}"]);
        assert_eq!(
            c.inject.as_deref(),
            Some("{\"app\":\"code\",\"title\":\"x\"}")
        );
        let c = parse_ok(&["--inject=/tmp/events.ndjson"]);
        assert_eq!(c.inject.as_deref(), Some("/tmp/events.ndjson"));
    }

    #[test]
    fn resolve_prefers_cli_then_store_then_default() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve(None), (DEFAULT_INTERVAL, DEFAULT_SETTLE));
        let store = json!({"interval_ms": 100, "settle_ms": 50, "backend": "wlr"});
        assert_eq!(
            cfg.resolve(Some(&store)),
            (Duration::from_millis(100), Duration::from_millis(50))
        );
        let cfg = Config {
            interval: Some(Duration::from_millis(10)),
            settle: Some(Duration::from_millis(20)),
            ..Config::default()
        };
        assert_eq!(
            cfg.resolve(Some(&store)),
            (Duration::from_millis(10), Duration::from_millis(20)),
            "CLI flags win over the store"
        );
        // junk in the store is ignored (default config, no CLI overrides)
        let junk = json!({"interval_ms": "fast", "settle_ms": -1});
        assert_eq!(
            Config::default().resolve(Some(&junk)),
            (DEFAULT_INTERVAL, DEFAULT_SETTLE)
        );
    }
}
