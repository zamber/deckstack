//! Hand-rolled argument parsing for `ds` (grammar: `docs/contracts/cli.md` §1).
//!
//! Global flags (`--socket`, `--token-file`, `--data-dir`, `--json`, `--ndjson`,
//! `--compact`) may appear anywhere; the verb is the first positional. `--help` at
//! every level (top, verb, group, group+verb) is resolved from the positionals.
//! There is deliberately no `migrate` verb: `ds migrate …` is a usage error (exit 1).

use serde_json::Value;
use std::path::PathBuf;

/// The output mode selected by `--json` / `--ndjson`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    #[default]
    Human,
    Json,
    Ndjson,
}

/// Global (verb-independent) options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Global {
    pub socket: Option<PathBuf>,
    pub token_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub mode: OutputMode,
    pub compact: bool,
}

/// The value source of `ds set`.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    /// Inline JSON value, or a scalar that failed JSON parsing (taken as a string).
    Inline(Value),
    /// `--file <path>`: an image file, sent as a `{"kind":"file",…}` visual.
    File(PathBuf),
}

/// A parsed command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Get {
        path: String,
        fields: Option<Vec<String>>,
        rev: Option<u64>,
    },
    Set {
        path: String,
        value: SetValue,
        if_rev: Option<u64>,
    },
    Watch {
        path: String,
        from_rev: Option<u64>,
        max_events: Option<usize>,
    },
    Schema {
        name: String,
    },
    Describe {
        path: String,
    },
    DaemonStart {
        device: Option<String>,
        foreground: bool,
    },
    DaemonStop,
    DaemonStatus,
    Status,
    /// `ds bind <topic> --do '<step>' [--do …] [--when '<cond>']` (DS-041).
    Bind {
        topic: String,
        dos: Vec<String>,
        when: Option<String>,
    },
    /// `ds unbind <id> | --topic <topic>` (DS-041).
    Unbind {
        id: Option<String>,
        topic: Option<String>,
    },
    /// `--help` at some level; the topic selects the text.
    Help(&'static str),
    Version,
}

/// A usage error: message printed as `ds: <msg>  [exit 1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

/// The schema names `ds schema <name>` accepts (`schemas/`).
pub const SCHEMA_NAMES: [&str; 5] = ["state", "ipc", "stream-frame", "plugin-manifest", "binding"];

/// Verbs (top-level words).
pub const VERBS: [&str; 9] = [
    "get", "set", "watch", "schema", "describe", "daemon", "status", "bind", "unbind",
];

/// Verbs of later phases — recognized, but refused with exit 1 in phase 1.
const LATER_PHASE: [&str; 5] = ["emit", "events", "stream", "plugin", "token"];

const DAEMON_SUBS: [&str; 3] = ["start", "stop", "status"];

/// Parse one inline `set` value: JSON when it parses, else the bare scalar as a string
/// (`'#3cc86e'` → `"#3cc86e"`, `80` → `80`, `'"Salon"'` → `"Salon"`).
pub fn parse_inline_value(s: &str) -> Value {
    match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => Value::String(s.to_string()),
    }
}

fn parse_u64(flag: &str, s: &str) -> Result<u64, UsageError> {
    s.parse::<u64>()
        .map_err(|_| UsageError(format!("{flag} needs a non-negative integer, got {s:?}")))
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

/// Parse argv (without argv[0]) into a command plus globals.
pub fn parse(args: &[String]) -> Result<(Command, Global), UsageError> {
    let mut g = Global::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut fields: Option<String> = None;
    let mut rev: Option<u64> = None;
    let mut if_rev: Option<u64> = None;
    let mut file: Option<PathBuf> = None;
    let mut from_rev: Option<u64> = None;
    let mut max_events: Option<usize> = None;
    let mut device: Option<String> = None;
    let mut do_steps: Vec<String> = Vec::new();
    let mut when: Option<String> = None;
    let mut topic_flag: Option<String> = None;
    let mut foreground = false;
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
            "--socket" => g.socket = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?)),
            "--token-file" => {
                g.token_file = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?))
            }
            "--data-dir" => {
                g.data_dir = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?))
            }
            "--fields" => fields = Some(value_of(args, &mut i, &inline, flag)?),
            "--rev" => rev = Some(parse_u64(flag, &value_of(args, &mut i, &inline, flag)?)?),
            "--if-rev" => if_rev = Some(parse_u64(flag, &value_of(args, &mut i, &inline, flag)?)?),
            "--file" => file = Some(PathBuf::from(value_of(args, &mut i, &inline, flag)?)),
            "--from-rev" => {
                from_rev = Some(parse_u64(flag, &value_of(args, &mut i, &inline, flag)?)?)
            }
            "--max-events" => {
                let s = value_of(args, &mut i, &inline, flag)?;
                max_events =
                    Some(s.parse::<usize>().map_err(|_| {
                        UsageError(format!("--max-events needs a number, got {s:?}"))
                    })?);
            }
            "--device" => device = Some(value_of(args, &mut i, &inline, flag)?),
            "--do" => do_steps.push(value_of(args, &mut i, &inline, flag)?),
            "--when" => when = Some(value_of(args, &mut i, &inline, flag)?),
            "--topic" => topic_flag = Some(value_of(args, &mut i, &inline, flag)?),
            "--json" => set_mode(&mut g, OutputMode::Json)?,
            "--ndjson" => set_mode(&mut g, OutputMode::Ndjson)?,
            "--compact" => g.compact = true,
            "--foreground" => foreground = true,
            "-h" | "--help" => help = true,
            "-V" | "--version" => version = true,
            _ if flag.starts_with('-') && flag.len() > 1 => {
                return Err(UsageError(format!(
                    "unknown argument '{arg}' (try `ds --help`)"
                )));
            }
            _ => positionals.push(arg.clone()),
        }
        i += 1;
    }

    // --help / --version are level-sensitive: resolve the topic from the positionals.
    if help {
        return Ok((help_command(&positionals)?, g));
    }
    if version {
        return Ok((Command::Version, g));
    }

    let Some(verb) = positionals.first() else {
        return Err(UsageError("missing verb (try `ds --help`)".to_string()));
    };
    let rest = &positionals[1..];
    let cmd = match verb.as_str() {
        "get" => {
            reject_ndjson(&g, "get")?;
            let path = one_path("get", rest)?;
            Command::Get {
                path,
                fields: fields.and_then(|f| {
                    let v: Vec<String> = f
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if v.is_empty() {
                        None
                    } else {
                        Some(v)
                    }
                }),
                rev,
            }
        }
        "set" => {
            reject_ndjson(&g, "set")?;
            let path = one_path("set", rest)?;
            let inline = rest.get(1).cloned();
            if rest.len() > 2 {
                return Err(UsageError(format!(
                    "set takes one path and one value, got extra arguments: {}",
                    rest[2..].join(" ")
                )));
            }
            let value = match (inline, file) {
                (Some(_), Some(_)) => {
                    return Err(UsageError(
                        "set takes either an inline value or --file <path>, not both".to_string(),
                    ))
                }
                (Some(v), None) => SetValue::Inline(parse_inline_value(&v)),
                (None, Some(p)) => SetValue::File(p),
                (None, None) => {
                    return Err(UsageError(
                        "set needs a value: an inline JSON/scalar or --file <path>".to_string(),
                    ))
                }
            };
            Command::Set {
                path,
                value,
                if_rev,
            }
        }
        "watch" => {
            if g.mode == OutputMode::Json {
                // docs/contracts/cli.md §2: streaming verbs accept only --ndjson.
                return Err(UsageError(
                    "--json is not valid for streaming verbs; use --ndjson".to_string(),
                ));
            }
            Command::Watch {
                path: one_path("watch", rest)?,
                from_rev,
                max_events,
            }
        }
        "schema" => {
            reject_ndjson(&g, "schema")?;
            let name = rest.first().ok_or_else(|| {
                UsageError(
                    "schema needs a name (state | ipc | stream-frame | plugin-manifest | binding)"
                        .to_string(),
                )
            })?;
            if !SCHEMA_NAMES.contains(&name.as_str()) {
                return Err(UsageError(format!(
                    "unknown schema name '{name}' (valid: {})",
                    SCHEMA_NAMES.join(", ")
                )));
            }
            if rest.len() > 1 {
                return Err(UsageError("schema takes exactly one name".to_string()));
            }
            Command::Schema { name: name.clone() }
        }
        "describe" => {
            reject_ndjson(&g, "describe")?;
            Command::Describe {
                path: one_path("describe", rest)?,
            }
        }
        "daemon" => {
            reject_ndjson(&g, "daemon")?;
            let Some(sub) = rest.first() else {
                return Err(UsageError(
                    "daemon needs a subcommand: start | stop | status".to_string(),
                ));
            };
            if rest.len() > 1 {
                return Err(UsageError(format!(
                    "daemon {} takes no further arguments: {}",
                    sub,
                    rest[1..].join(" ")
                )));
            }
            match sub.as_str() {
                "start" => Command::DaemonStart { device, foreground },
                "stop" => Command::DaemonStop,
                "status" => Command::DaemonStatus,
                other => {
                    return Err(UsageError(format!(
                        "unknown daemon subcommand '{other}' (valid: {})",
                        DAEMON_SUBS.join(", ")
                    )))
                }
            }
        }
        "status" => {
            reject_ndjson(&g, "status")?;
            if !rest.is_empty() {
                return Err(UsageError(format!(
                    "status takes no arguments: {}",
                    rest.join(" ")
                )));
            }
            Command::Status
        }
        "bind" => {
            reject_ndjson(&g, "bind")?;
            let Some(topic) = rest.first() else {
                return Err(UsageError(
                    "bind needs a topic (e.g. `ds bind deck/control/keys.0/down --do 'sleep 5'`)"
                        .to_string(),
                ));
            };
            if rest.len() > 1 {
                return Err(UsageError(format!(
                    "bind takes exactly one topic, got extra: {}",
                    rest[1..].join(" ")
                )));
            }
            if do_steps.is_empty() {
                return Err(UsageError(
                    "bind needs at least one --do '<step>' (e.g. --do 'sleep 5')".to_string(),
                ));
            }
            Command::Bind {
                topic: topic.clone(),
                dos: do_steps.clone(),
                when: when.clone(),
            }
        }
        "unbind" => {
            reject_ndjson(&g, "unbind")?;
            if rest.len() > 1 {
                return Err(UsageError(format!(
                    "unbind takes one binding id, got extra: {}",
                    rest[1..].join(" ")
                )));
            }
            match (rest.first(), &topic_flag) {
                (Some(id), None) => Command::Unbind {
                    id: Some(id.clone()),
                    topic: None,
                },
                (None, Some(t)) => Command::Unbind {
                    id: None,
                    topic: Some(t.clone()),
                },
                (Some(_), Some(_)) => {
                    return Err(UsageError(
                        "unbind takes either <id> or --topic <topic>, not both".to_string(),
                    ))
                }
                (None, None) => {
                    return Err(UsageError(
                        "unbind needs a binding id (bNNNN) or --topic <topic>".to_string(),
                    ))
                }
            }
        }
        "migrate" => {
            return Err(UsageError(
                "there is no 'migrate' verb — porting of old apps is manual and ticket-driven (docs/70-porting-notes.md)"
                    .to_string(),
            ));
        }
        v if LATER_PHASE.contains(&v) => {
            return Err(UsageError(format!(
                "verb '{v}' is not implemented in phase 1 (later ticket); see `ds --help`"
            )));
        }
        other => {
            return Err(UsageError(format!(
                "unknown verb '{other}' (try `ds --help`)"
            )));
        }
    };
    Ok((cmd, g))
}

fn set_mode(g: &mut Global, mode: OutputMode) -> Result<(), UsageError> {
    if g.mode != OutputMode::Human && g.mode != mode {
        return Err(UsageError(
            "--json and --ndjson cannot be combined".to_string(),
        ));
    }
    g.mode = mode;
    Ok(())
}

fn reject_ndjson(g: &Global, verb: &str) -> Result<(), UsageError> {
    if g.mode == OutputMode::Ndjson {
        return Err(UsageError(format!(
            "--ndjson is only valid for streaming verbs (watch); '{verb}' takes --json"
        )));
    }
    Ok(())
}

fn one_path(verb: &str, rest: &[String]) -> Result<String, UsageError> {
    let Some(path) = rest.first() else {
        return Err(UsageError(format!(
            "{verb} needs a path (e.g. `ds {verb} /controls/keys/0`)"
        )));
    };
    if !path.starts_with('/') {
        return Err(UsageError(format!(
            "path must start with '/': {path:?} (e.g. `ds {verb} /controls/keys/0`)"
        )));
    }
    if rest.len() > 1 && verb != "set" {
        return Err(UsageError(format!(
            "{verb} takes exactly one path, got extra: {}",
            rest[1..].join(" ")
        )));
    }
    Ok(path.clone())
}

/// Resolve the `--help` topic from the positionals: top, verb, group, or group+verb.
fn help_command(positionals: &[String]) -> Result<Command, UsageError> {
    let Some(first) = positionals.first() else {
        return Ok(Command::Help("top"));
    };
    match first.as_str() {
        "daemon" => match positionals.get(1).map(String::as_str) {
            None => Ok(Command::Help("daemon")),
            Some("start") => Ok(Command::Help("daemon start")),
            Some("stop") => Ok(Command::Help("daemon stop")),
            Some("status") => Ok(Command::Help("daemon status")),
            Some(other) => Err(UsageError(format!(
                "unknown daemon subcommand '{other}' (valid: {})",
                DAEMON_SUBS.join(", ")
            ))),
        },
        v if VERBS.contains(&v) => Ok(Command::Help(match v {
            "get" => "get",
            "set" => "set",
            "watch" => "watch",
            "schema" => "schema",
            "describe" => "describe",
            "status" => "status",
            "bind" => "bind",
            "unbind" => "unbind",
            _ => "top",
        })),
        "migrate" => Err(UsageError(
            "there is no 'migrate' verb — porting of old apps is manual and ticket-driven (docs/70-porting-notes.md)"
                .to_string(),
        )),
        v if LATER_PHASE.contains(&v) => Err(UsageError(format!(
            "verb '{v}' is not implemented in phase 1 (later ticket); see `ds --help`"
        ))),
        other => Err(UsageError(format!(
            "unknown verb '{other}' (try `ds --help`)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn parse_ok(v: &[&str]) -> (Command, Global) {
        parse(&args(v)).unwrap_or_else(|e| panic!("{}: {e:?}", v.join(" ")))
    }

    fn parse_err(v: &[&str]) -> String {
        parse(&args(v)).unwrap_err().0
    }

    #[test]
    fn parses_every_phase1_verb() {
        let (c, g) = parse_ok(&["get", "/controls/keys/0"]);
        assert_eq!(
            c,
            Command::Get {
                path: "/controls/keys/0".into(),
                fields: None,
                rev: None
            }
        );
        assert_eq!(g.mode, OutputMode::Human);

        let (c, g) = parse_ok(&[
            "get",
            "/meta",
            "--fields",
            "rev,daemon_pid",
            "--rev",
            "3",
            "--json",
        ]);
        assert_eq!(
            c,
            Command::Get {
                path: "/meta".into(),
                fields: Some(vec!["rev".into(), "daemon_pid".into()]),
                rev: Some(3)
            }
        );
        assert_eq!(g.mode, OutputMode::Json);

        let (c, _) = parse_ok(&[
            "set",
            "/controls/keys/0/image",
            "--json",
            "{\"kind\":\"color\",\"value\":\"#3cc86e\"}",
        ]);
        assert_eq!(
            c,
            Command::Set {
                path: "/controls/keys/0/image".into(),
                value: SetValue::Inline(json!({"kind":"color","value":"#3cc86e"})),
                if_rev: None
            }
        );

        let (c, _) = parse_ok(&["set", "/controls/keys/3/brightness", "80", "--if-rev", "41"]);
        assert_eq!(
            c,
            Command::Set {
                path: "/controls/keys/3/brightness".into(),
                value: SetValue::Inline(json!(80)),
                if_rev: Some(41)
            }
        );

        // a bare scalar is taken as a string
        let (c, _) = parse_ok(&["set", "/controls/keys/0/color", "#3cc86e"]);
        assert_eq!(
            c,
            Command::Set {
                path: "/controls/keys/0/color".into(),
                value: SetValue::Inline(json!("#3cc86e")),
                if_rev: None
            }
        );

        let (c, _) = parse_ok(&["set", "/controls/keys/0/image", "--file", "/tmp/pic.png"]);
        assert_eq!(
            c,
            Command::Set {
                path: "/controls/keys/0/image".into(),
                value: SetValue::File(PathBuf::from("/tmp/pic.png")),
                if_rev: None
            }
        );

        let (c, g) = parse_ok(&[
            "watch",
            "/controls",
            "--ndjson",
            "--from-rev",
            "2",
            "--max-events",
            "5",
        ]);
        assert_eq!(
            c,
            Command::Watch {
                path: "/controls".into(),
                from_rev: Some(2),
                max_events: Some(5)
            }
        );
        assert_eq!(g.mode, OutputMode::Ndjson);

        let (c, _) = parse_ok(&["schema", "state"]);
        assert_eq!(
            c,
            Command::Schema {
                name: "state".into()
            }
        );
        let (c, _) = parse_ok(&["describe", "/controls/keys/0"]);
        assert_eq!(
            c,
            Command::Describe {
                path: "/controls/keys/0".into()
            }
        );
        let (c, _) = parse_ok(&["daemon", "start", "--device", "virtual:/tmp/vdev.sock"]);
        assert_eq!(
            c,
            Command::DaemonStart {
                device: Some("virtual:/tmp/vdev.sock".into()),
                foreground: false
            }
        );
        let (c, _) = parse_ok(&["daemon", "start", "--foreground"]);
        assert!(matches!(
            c,
            Command::DaemonStart {
                foreground: true,
                ..
            }
        ));
        let (c, _) = parse_ok(&["daemon", "stop"]);
        assert_eq!(c, Command::DaemonStop);
        let (c, _) = parse_ok(&["daemon", "status"]);
        assert_eq!(c, Command::DaemonStatus);
        let (c, _) = parse_ok(&["status"]);
        assert_eq!(c, Command::Status);
    }

    #[test]
    fn globals_may_appear_anywhere_and_use_equals_form() {
        let (_, g) = parse_ok(&[
            "--socket=/tmp/s.sock",
            "get",
            "/meta",
            "--json",
            "--compact",
        ]);
        assert_eq!(g.socket, Some(PathBuf::from("/tmp/s.sock")));
        assert_eq!(g.mode, OutputMode::Json);
        assert!(g.compact);
        let (_, g) = parse_ok(&[
            "get",
            "/meta",
            "--socket",
            "/x.sock",
            "--token-file",
            "/x.token",
            "--data-dir",
            "/x",
        ]);
        assert_eq!(g.socket, Some(PathBuf::from("/x.sock")));
        assert_eq!(g.token_file, Some(PathBuf::from("/x.token")));
        assert_eq!(g.data_dir, Some(PathBuf::from("/x")));
    }

    #[test]
    fn migrate_is_a_usage_error() {
        let e = parse_err(&["migrate", "/x"]);
        assert!(e.contains("no 'migrate' verb"), "{e}");
        let e = parse_err(&["migrate"]);
        assert!(e.contains("no 'migrate' verb"), "{e}");
    }

    #[test]
    fn unknown_verbs_and_flags_are_usage_errors() {
        assert!(parse_err(&["frobnicate"]).contains("unknown verb"));
        assert!(parse_err(&["get", "/x", "--bogus"]).contains("unknown argument"));
        assert!(parse_err(&[]).contains("missing verb"));
        assert!(parse_err(&["emit", "/topic"]).contains("not implemented in phase 1"));
    }

    #[test]
    fn parses_bind_and_unbind() {
        let (c, _) = parse_ok(&[
            "bind",
            "deck/control/keys.0/down",
            "--do",
            "plugin demo.bump {\"by\":1}",
            "--do",
            "{\"do\":\"sleep\",\"ms\":5}",
            "--when",
            "{\"payload\":\"key\",\"op\":\"eq\",\"value\":0}",
        ]);
        assert_eq!(
            c,
            Command::Bind {
                topic: "deck/control/keys.0/down".into(),
                dos: vec![
                    "plugin demo.bump {\"by\":1}".into(),
                    "{\"do\":\"sleep\",\"ms\":5}".into()
                ],
                when: Some("{\"payload\":\"key\",\"op\":\"eq\",\"value\":0}".into()),
            }
        );
        // --do is repeatable and may use the = form
        let (c, _) = parse_ok(&[
            "bind",
            "app/focus/changed",
            "--do=set /x 1",
            "--do=emit app/y {}",
        ]);
        assert_eq!(
            c,
            Command::Bind {
                topic: "app/focus/changed".into(),
                dos: vec!["set /x 1".into(), "emit app/y {}".into()],
                when: None,
            }
        );
        // bind needs a topic and at least one --do
        assert!(parse_err(&["bind"]).contains("needs a topic"));
        assert!(parse_err(&["bind", "deck/x"]).contains("at least one --do"));
        assert!(parse_err(&["bind", "a", "b", "--do", "sleep 1"]).contains("exactly one topic"));
        // unbind by id / by topic / errors
        let (c, _) = parse_ok(&["unbind", "b0001"]);
        assert_eq!(
            c,
            Command::Unbind {
                id: Some("b0001".into()),
                topic: None
            }
        );
        let (c, _) = parse_ok(&["unbind", "--topic", "deck/control/keys.0/down"]);
        assert_eq!(
            c,
            Command::Unbind {
                id: None,
                topic: Some("deck/control/keys.0/down".into())
            }
        );
        assert!(parse_err(&["unbind"]).contains("needs a binding id"));
        assert!(parse_err(&["unbind", "b0001", "--topic", "deck/x"]).contains("not both"));
        assert!(parse_err(&["unbind", "b0001", "b0002"]).contains("got extra"));
        // --ndjson is refused for bind/unbind like the other non-streaming verbs
        assert!(
            parse_err(&["bind", "deck/x", "--do", "sleep 1", "--ndjson"])
                .contains("--ndjson is only valid")
        );
        assert!(parse_err(&["unbind", "b0001", "--ndjson"]).contains("--ndjson is only valid"));
    }

    #[test]
    fn json_on_watch_is_the_contract_usage_error() {
        let e = parse_err(&["watch", "/controls", "--json"]);
        assert_eq!(e, "--json is not valid for streaming verbs; use --ndjson");
    }

    #[test]
    fn ndjson_is_only_for_streaming_verbs_and_modes_do_not_mix() {
        assert!(parse_err(&["get", "/x", "--ndjson"]).contains("--ndjson is only valid"));
        assert!(parse_err(&["watch", "/x", "--json", "--ndjson"]).contains("cannot be combined"));
        assert!(parse_err(&["set", "/x", "1", "--json", "--ndjson"]).contains("cannot be combined"));
    }

    #[test]
    fn set_value_validation() {
        assert!(parse_err(&["set", "/x"]).contains("needs a value"));
        assert!(parse_err(&["set", "/x", "1", "--file", "/f"]).contains("not both"));
        assert!(parse_err(&["set", "/x", "1", "2"]).contains("extra arguments"));
        assert!(parse_err(&["get"]).contains("needs a path"));
        assert!(parse_err(&["get", "controls"]).contains("must start with '/'"));
        assert!(parse_err(&["schema"]).contains("needs a name"));
        assert!(parse_err(&["schema", "nope"]).contains("unknown schema name"));
        assert!(parse_err(&["daemon"]).contains("needs a subcommand"));
        assert!(parse_err(&["daemon", "restart"]).contains("unknown daemon subcommand"));
        assert!(parse_err(&["--rev", "x", "get", "/m"]).contains("--rev needs"));
    }

    #[test]
    fn help_at_every_level() {
        assert_eq!(parse_ok(&["--help"]).0, Command::Help("top"));
        assert_eq!(parse_ok(&["-h"]).0, Command::Help("top"));
        assert_eq!(parse_ok(&["get", "--help"]).0, Command::Help("get"));
        assert_eq!(parse_ok(&["get", "/x", "--help"]).0, Command::Help("get"));
        assert_eq!(parse_ok(&["set", "--help"]).0, Command::Help("set"));
        assert_eq!(parse_ok(&["watch", "--help"]).0, Command::Help("watch"));
        assert_eq!(parse_ok(&["schema", "--help"]).0, Command::Help("schema"));
        assert_eq!(
            parse_ok(&["describe", "--help"]).0,
            Command::Help("describe")
        );
        assert_eq!(parse_ok(&["status", "--help"]).0, Command::Help("status"));
        assert_eq!(parse_ok(&["bind", "--help"]).0, Command::Help("bind"));
        assert_eq!(parse_ok(&["unbind", "--help"]).0, Command::Help("unbind"));
        assert_eq!(parse_ok(&["daemon", "--help"]).0, Command::Help("daemon"));
        assert_eq!(
            parse_ok(&["daemon", "start", "--help"]).0,
            Command::Help("daemon start")
        );
        assert_eq!(
            parse_ok(&["daemon", "stop", "--help"]).0,
            Command::Help("daemon stop")
        );
        assert_eq!(
            parse_ok(&["daemon", "status", "--help"]).0,
            Command::Help("daemon status")
        );
        assert!(matches!(parse_ok(&["--version"]).0, Command::Version));
        assert!(matches!(parse_ok(&["-V"]).0, Command::Version));
        assert!(parse_err(&["migrate", "--help"]).contains("no 'migrate' verb"));
    }

    #[test]
    fn inline_value_parsing() {
        assert_eq!(parse_inline_value("80"), json!(80));
        assert_eq!(parse_inline_value("\"Salon\""), json!("Salon"));
        assert_eq!(parse_inline_value("#3cc86e"), json!("#3cc86e"));
        assert_eq!(parse_inline_value("true"), json!(true));
        assert_eq!(
            parse_inline_value("{\"kind\":\"icon\",\"name\":\"tabler:bolt\"}"),
            json!({"kind":"icon","name":"tabler:bolt"})
        );
    }
}
