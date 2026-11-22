//! The event bus and router of ds-daemon (ticket DS-040; contract
//! `docs/contracts/actions.md`, component `docs/components/event-router.md`).
//!
//! Every event source lands on the [`Bus`]:
//!
//! - **state changes** — the state task publishes one [`Event::State`] per changed path
//!   per committed revision (frames and `/telemetry/*` never enter the bus, S1);
//! - **device input** — the device task decodes input reports with [`InputDecoder`] into
//!   `deck/control/<control>/<gesture>` events, and connect/disconnect into
//!   `deck/device/<serial>/<event>`;
//! - **daemon lifecycle** — `daemon/start`, `daemon/stop`;
//! - **external emits** — the IPC `emit` op publishes `app/<sender>/<topic>` (or any
//!   schema-allowed `deck/…` topic) with the given payload.
//!
//! Subscribers (IPC `subscribe`/`unsubscribe` ops, later the binding engine of DS-041/042)
//! register a [`Pattern`]: a **state-path pattern** (the wire `path` names a state path,
//! e.g. `/controls/keys/*` — matching keeps the exact `ds-state` prefix semantics) or a
//! **topic pattern** (the wire `path` starts with a topic root: `/deck/…`, `/app/…`,
//! `/daemon/…`, `/plugin/…`; the leading `/` is stripped and [`topics::TopicPattern`]
//! matching applies, where `.` and `/` both separate segments). `/telemetry/*`
//! subscriptions stay on the direct store subscription and never touch the bus (S1).
//!
//! Dispatch order, wildcard rules and backpressure are documented on [`bus`] and in the
//! two docs; in one line: exact patterns before wildcard patterns, subscription order
//! within each group, FIFO per subscriber, bounded queue with drop-oldest + counted
//! `dropped` messages so publishers never block.

pub mod bus;
pub mod topics;

pub use bus::{Bus, Event, FnSink, IpcSink, Sink, SUBSCRIBER_QUEUE_CAP};
pub use topics::TopicPattern;

use ds_ipc::error::{ErrorCode, IpcError};
use ds_state::Path;
use serde_json::{json, Value};

use ds_protocol::{Input, ModuleKind, Orientation};
use mh_device::ModuleId;

/// A subscription pattern: state-path or topic.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// A state-path pattern (`/controls/keys/*`) — exact `ds-state` matching semantics.
    State(Path),
    /// A topic pattern (`deck/control/keys/*`) — `topics::TopicPattern` semantics.
    Topic(TopicPattern),
}

impl Pattern {
    /// Parse the wire `path` of a `subscribe` op. Topic roots (`deck`, `app`, `daemon`,
    /// `plugin`) as the first segment make it a topic pattern (leading `/` stripped);
    /// anything else is validated as a state path exactly like before (E_PATH on unknown
    /// top-level sections).
    pub fn parse_wire(wire: &str) -> Result<Self, IpcError> {
        let first = wire.trim_start_matches('/').split('/').next().unwrap_or("");
        if topics::is_topic_root(first) {
            let raw = wire.trim_start_matches('/');
            let pat = TopicPattern::parse(raw).map_err(|e| {
                IpcError::new(ErrorCode::EPath, format!("bad topic pattern {wire:?}: {e}"))
            })?;
            return Ok(Pattern::Topic(pat));
        }
        let path = Path::parse(wire).map_err(crate::state::state_error)?;
        path.validate_read().map_err(crate::state::state_error)?;
        Ok(Pattern::State(path))
    }

    /// The original wire spelling (as sent in the `subscribe` op).
    pub fn as_wire(&self) -> String {
        match self {
            Pattern::State(p) => p.as_str().to_string(),
            Pattern::Topic(p) => format!("/{}", p.as_str()),
        }
    }

    /// Whether the pattern contains a `*` wildcard (dispatch-order grouping).
    pub fn is_wildcard(&self) -> bool {
        match self {
            Pattern::State(p) => p.has_wildcard(),
            Pattern::Topic(p) => p.is_wildcard(),
        }
    }

    /// Whether this pattern matches the event.
    pub fn matches(&self, ev: &Event) -> bool {
        match (self, ev) {
            (Pattern::State(p), Event::State { path, .. }) => match Path::parse(path) {
                Ok(ep) => p.matches(&ep),
                Err(_) => false,
            },
            (Pattern::Topic(p), Event::Topic { topic, .. }) => p.matches(topic),
            _ => false,
        }
    }
}

/// A `deck/device/<serial>/<event>` lifecycle event.
pub fn device_event(serial: &str, event: &str, payload: Value) -> Event {
    let topic = format!("deck/device/{serial}/{event}");
    debug_assert!(
        topics::validate_topic(&topic).is_ok(),
        "bad device topic {topic}"
    );
    Event::topic(topic, payload)
}

/// Decode device input reports into bus events. Tracks the previous key/fader state so
/// the full-state `Input::Keys`/`Input::Faders` reports become per-control
/// down/up/slide gestures (mh-protocol sends full state on every change).
///
/// Mapping (actions.md §1):
///
/// | input report | topic | payload |
/// |---|---|---|
/// | `Keys` (diff) | `deck/control/keys.<i>/down\|up` | `{"control","key","pressed"}` |
/// | `Dial { delta }` | `deck/control/dial/rotate` | `{"control":"dial","delta"}` |
/// | `DialPress` | `deck/control/dial/down\|up` | `{"control":"dial","pressed"}` |
/// | `Knob { index, delta }` | `deck/control/knobs.<i>/rotate` | `{"control","index","delta"}` |
/// | `KnobPress` | `deck/control/knobs.<i>/down\|up` | `{"control","index","pressed"}` |
/// | `Roller { index, delta }` | `deck/control/rollers.<i>/rotate` | `{"control","index","delta"}` |
/// | `Faders` (diff) | `deck/control/faders.<i>/slide` | `{"control","index","value"}` |
/// | `Orientation` | `deck/device/<serial>/orientation` | `{"orientation"}` |
///
/// `long`/`double` gestures need hold/double timing and are not synthesized yet (they
/// land with the binding engine, DS-042); `Unknown` reports produce no event.
pub struct InputDecoder {
    prev_keys: [bool; 15],
    prev_faders: [u8; 5],
}

impl Default for InputDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl InputDecoder {
    /// A fresh decoder (previous state: nothing pressed, faders 0).
    pub fn new() -> Self {
        Self {
            prev_keys: [false; 15],
            prev_faders: [0; 5],
        }
    }

    /// Decode one input report from `module` into zero or more bus events.
    pub fn decode(&mut self, module: &ModuleId, input: &Input) -> Vec<Event> {
        match input {
            Input::Keys(keys) => {
                let mut out = Vec::new();
                for (i, pressed) in keys.iter().enumerate() {
                    if *pressed != self.prev_keys[i] {
                        let gesture = if *pressed { "down" } else { "up" };
                        out.push(Event::topic(
                            format!("deck/control/keys.{i}/{gesture}"),
                            json!({"control": format!("keys.{i}"), "key": i, "pressed": pressed}),
                        ));
                    }
                }
                self.prev_keys = *keys;
                out
            }
            Input::Faders(values) => {
                let mut out = Vec::new();
                for (i, v) in values.iter().enumerate() {
                    if *v != self.prev_faders[i] {
                        out.push(Event::topic(
                            format!("deck/control/faders.{i}/slide"),
                            json!({"control": format!("faders.{i}"), "index": i, "value": v}),
                        ));
                    }
                }
                self.prev_faders = *values;
                out
            }
            Input::Knob { index, delta } => vec![Event::topic(
                format!("deck/control/knobs.{index}/rotate"),
                json!({"control": format!("knobs.{index}"), "index": index, "delta": delta}),
            )],
            Input::KnobPress { index, pressed } => {
                let gesture = if *pressed { "down" } else { "up" };
                vec![Event::topic(
                    format!("deck/control/knobs.{index}/{gesture}"),
                    json!({"control": format!("knobs.{index}"), "index": index, "pressed": pressed}),
                )]
            }
            Input::Roller { index, delta } => vec![Event::topic(
                format!("deck/control/rollers.{index}/rotate"),
                json!({"control": format!("rollers.{index}"), "index": index, "delta": delta}),
            )],
            Input::Dial { delta } => vec![Event::topic(
                "deck/control/dial/rotate",
                json!({"control": "dial", "delta": delta}),
            )],
            Input::DialPress { pressed } => {
                let gesture = if *pressed { "down" } else { "up" };
                vec![Event::topic(
                    format!("deck/control/dial/{gesture}"),
                    json!({"control": "dial", "pressed": pressed}),
                )]
            }
            Input::Orientation(o) => vec![device_event(
                &module.serial,
                "orientation",
                json!({"orientation": match o {
                    Orientation::Horizontal => "horizontal",
                    Orientation::Vertical => "vertical",
                }}),
            )],
            Input::Unknown { tag, payload } => {
                let _ = (tag, payload);
                Vec::new()
            }
        }
    }
}

/// The module kind an input report belongs to (for logs/sanity; the decoder is
/// kind-agnostic because the report itself says what it is).
pub fn module_kind_name(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::Base => "base",
        ModuleKind::Keys => "keys",
        ModuleKind::Dial => "dial",
        ModuleKind::Knobs => "knobs",
        ModuleKind::Faders => "faders",
        ModuleKind::Rollers => "rollers",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_protocol::Input;

    fn keys_module() -> ModuleId {
        ModuleId {
            kind: ModuleKind::Keys,
            serial: "virtual-0102".into(),
        }
    }

    fn dial_module() -> ModuleId {
        ModuleId {
            kind: ModuleKind::Dial,
            serial: "virtual-0202".into(),
        }
    }

    #[test]
    fn pattern_parse_wire_splits_topic_and_state() {
        match Pattern::parse_wire("/deck/control/keys/*").unwrap() {
            Pattern::Topic(p) => {
                assert_eq!(p.as_str(), "deck/control/keys/*");
                assert!(p.matches("deck/control/keys.0/down"));
            }
            _ => panic!("must be a topic pattern"),
        }
        match Pattern::parse_wire("/controls/keys/*").unwrap() {
            Pattern::State(p) => assert_eq!(p.as_str(), "/controls/keys/*"),
            _ => panic!("must be a state pattern"),
        }
        assert_eq!(Pattern::parse_wire("/app/*").unwrap().as_wire(), "/app/*");
        // unknown top-level state path still fails like before (E_PATH)
        let err = Pattern::parse_wire("/nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::EPath);
        // bad topic syntax
        assert!(Pattern::parse_wire("/deck/bad seg").is_err());
    }

    #[test]
    fn keys_report_becomes_down_then_up_per_changed_key() {
        let mut d = InputDecoder::new();
        let m = keys_module();
        let mut keys = [false; 15];
        keys[0] = true;
        keys[3] = true;
        let evs = d.decode(&m, &Input::Keys(keys));
        let topics: Vec<&str> = evs.iter().map(|e| e.subject()).collect();
        assert_eq!(
            topics,
            vec!["deck/control/keys.0/down", "deck/control/keys.3/down"]
        );
        assert_eq!(evs[0].wire()["payload"]["pressed"], true);
        assert_eq!(evs[0].wire()["topic"], "deck/control/keys.0/down");
        for e in &evs {
            topics::validate_topic(e.subject()).unwrap();
            ds_ipc::message::validate_server_message(&e.wire()).unwrap();
        }
        // same state again -> no events
        assert!(d.decode(&m, &Input::Keys(keys)).is_empty());
        // release key 0, keep 3
        keys[0] = false;
        let evs = d.decode(&m, &Input::Keys(keys));
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].subject(), "deck/control/keys.0/up");
        assert_eq!(evs[0].wire()["payload"]["pressed"], false);
    }

    #[test]
    fn dial_knob_roller_fader_inputs_map_to_control_topics() {
        let mut d = InputDecoder::new();
        let dial = dial_module();
        let evs = d.decode(&dial, &Input::Dial { delta: -1 });
        assert_eq!(evs[0].subject(), "deck/control/dial/rotate");
        assert_eq!(evs[0].wire()["payload"]["delta"], -1);
        let evs = d.decode(&dial, &Input::DialPress { pressed: true });
        assert_eq!(evs[0].subject(), "deck/control/dial/down");
        let evs = d.decode(&dial, &Input::DialPress { pressed: false });
        assert_eq!(evs[0].subject(), "deck/control/dial/up");

        let knobs = ModuleId {
            kind: ModuleKind::Knobs,
            serial: "virtual-0302".into(),
        };
        let evs = d.decode(&knobs, &Input::Knob { index: 2, delta: 1 });
        assert_eq!(evs[0].subject(), "deck/control/knobs.2/rotate");
        let evs = d.decode(
            &knobs,
            &Input::KnobPress {
                index: 0,
                pressed: true,
            },
        );
        assert_eq!(evs[0].subject(), "deck/control/knobs.0/down");

        let rollers = ModuleId {
            kind: ModuleKind::Rollers,
            serial: "virtual-0402".into(),
        };
        let evs = d.decode(&rollers, &Input::Roller { index: 1, delta: 1 });
        assert_eq!(evs[0].subject(), "deck/control/rollers.1/rotate");

        let faders = ModuleId {
            kind: ModuleKind::Faders,
            serial: "virtual-0502".into(),
        };
        let evs = d.decode(&faders, &Input::Faders([10, 0, 0, 0, 100]));
        let topics: Vec<&str> = evs.iter().map(|e| e.subject()).collect();
        assert_eq!(
            topics,
            vec!["deck/control/faders.0/slide", "deck/control/faders.4/slide"]
        );
        assert_eq!(evs[0].wire()["payload"]["value"], 10);
        // unchanged faders -> no events
        assert!(d
            .decode(&faders, &Input::Faders([10, 0, 0, 0, 100]))
            .is_empty());
        // unknown reports produce nothing
        assert!(d
            .decode(
                &dial,
                &Input::Unknown {
                    tag: 0x99,
                    payload: vec![1]
                }
            )
            .is_empty());
    }

    #[test]
    fn orientation_becomes_a_device_event() {
        let mut d = InputDecoder::new();
        let base = ModuleId {
            kind: ModuleKind::Base,
            serial: "virtual-0002".into(),
        };
        let evs = d.decode(&base, &Input::Orientation(Orientation::Horizontal));
        assert_eq!(evs[0].subject(), "deck/device/virtual-0002/orientation");
        assert_eq!(evs[0].wire()["payload"]["orientation"], "horizontal");
        topics::validate_topic(evs[0].subject()).unwrap();
    }

    #[test]
    fn device_event_helper_builds_valid_topics() {
        let ev = device_event(
            "virtual-0102",
            "connected",
            json!({"device": "virtual:/tmp/v.sock"}),
        );
        assert_eq!(ev.subject(), "deck/device/virtual-0102/connected");
        topics::validate_topic(ev.subject()).unwrap();
    }
}
