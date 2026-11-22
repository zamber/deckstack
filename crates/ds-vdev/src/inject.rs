//! Input injection: scripted device→host input events, replayed from an ndjson file
//! (`--inject events.ndjson`, `ds-vdevctl inject events.ndjson`). Each line is one event:
//!
//! ```json
//! {"module": "keys", "keys": [0, 3]}
//! {"module": "faders", "faders": [10, 20, 30, 40, 50], "at_ms": 120}
//! {"module": "knobs", "knob": 0, "knob_delta": 1}
//! {"module": "knobs", "knob_press": 2, "pressed": true}
//! {"module": "rollers", "roller": 1, "roller_delta": -1}
//! {"module": "dial", "dial_delta": 1}
//! {"module": "dial", "dial_pressed": true}
//! {"module": "base", "orientation": "horizontal"}
//! ```
//!
//! `at_ms` is the timestamp of the event inside the script (used by the `--inject` replayer
//! to space events out). The hub encodes each event into the module's raw input report
//! (`01 <tag> <len u16 LE> <payload…>`, padded to the 64-byte input report) and broadcasts it
//! as an `0x81` INPUT frame to every connected host.

use ds_device::frame::{kind, Frame};
use ds_device::module::module_info;
use ds_protocol::ModuleKind;
use serde::{Deserialize, Serialize};

/// One scripted input event (one ndjson line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectEvent {
    /// Module name (`keys`, `dial`, `base`, `faders`, `knobs`, `rollers`).
    pub module: String,
    /// Timestamp inside the script, in milliseconds (replayer spacing only).
    #[serde(default)]
    pub at_ms: u64,
    /// Keys module: indices currently pressed (full key-state report).
    #[serde(default)]
    pub keys: Option<Vec<u8>>,
    /// Faders module: the five values 0..=100.
    #[serde(default)]
    pub faders: Option<Vec<u8>>,
    /// Knobs module: detent of knob `knob` by `knob_delta` (+1/-1).
    #[serde(default)]
    pub knob: Option<u8>,
    #[serde(default)]
    pub knob_delta: Option<i8>,
    /// Knobs module: push of knob `knob_press`, pressed or released.
    #[serde(default)]
    pub knob_press: Option<u8>,
    #[serde(default)]
    pub pressed: Option<bool>,
    /// Rollers module: notch of roller `roller` by `roller_delta` (+1/-1).
    #[serde(default)]
    pub roller: Option<u8>,
    #[serde(default)]
    pub roller_delta: Option<i8>,
    /// Dial module: one detent by `dial_delta` (+1/-1).
    #[serde(default)]
    pub dial_delta: Option<i8>,
    /// Dial module: push pressed/released.
    #[serde(default)]
    pub dial_pressed: Option<bool>,
    /// Base module: `"horizontal"` or `"vertical"`.
    #[serde(default)]
    pub orientation: Option<String>,
}

/// Parse a whole ndjson script into `(at_ms, event)` pairs, preserving file order.
pub fn parse_events_ndjson(text: &str) -> Result<Vec<InjectEvent>, String> {
    let mut events = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let ev: InjectEvent =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", lineno + 1))?;
        module_kind(&ev.module).map_err(|e| format!("line {}: {e}", lineno + 1))?;
        events.push(ev);
    }
    Ok(events)
}

fn module_kind(name: &str) -> Result<ModuleKind, String> {
    ModuleKind::from_name(name).ok_or_else(|| format!("unknown module '{name}'"))
}

/// Encode one event as the module's raw input report (report id `0x01` first, padded to the
/// 64-byte input report length, exactly what the hardware sends).
pub fn encode_input_report(ev: &InjectEvent) -> Result<Vec<u8>, String> {
    let kind = module_kind(&ev.module)?;
    let info = module_info(kind);
    let payload: Vec<u8> = match kind {
        ModuleKind::Keys => {
            let keys = ev.keys.as_deref().ok_or("keys event needs \"keys\"")?;
            let mut p = vec![0x02, 0x0F];
            p.resize(17, 0);
            for &k in keys {
                let k = usize::from(k);
                if k >= 15 {
                    return Err(format!("key index {k} out of range 0..15"));
                }
                p[2 + k] = 1;
            }
            p
        }
        ModuleKind::Faders => {
            let values = ev
                .faders
                .as_deref()
                .ok_or("faders event needs \"faders\"")?;
            if values.len() != 5 {
                return Err("faders event needs exactly 5 values".into());
            }
            let mut p = vec![0x02, 0x05];
            p.extend_from_slice(values);
            p
        }
        ModuleKind::Knobs => {
            if let (Some(idx), Some(delta)) = (ev.knob, ev.knob_delta) {
                if idx >= 3 || delta.abs() != 1 {
                    return Err("knob event needs knob 0..3 and knob_delta ±1".into());
                }
                let mut p = vec![0x00, 0x03, 0xFF, 0xFF, 0xFF];
                p[2 + usize::from(idx)] = u8::from(delta == 1);
                p
            } else if let (Some(idx), Some(pressed)) = (ev.knob_press, ev.pressed) {
                if idx >= 3 {
                    return Err("knob_press out of range 0..3".into());
                }
                let mut p = vec![0x03, 0x03, 0, 0, 0];
                p[2 + usize::from(idx)] = u8::from(pressed);
                p
            } else {
                return Err("knobs event needs knob+knob_delta or knob_press+pressed".into());
            }
        }
        ModuleKind::Rollers => {
            let idx = usize::from(ev.roller.ok_or("rollers event needs \"roller\"")?);
            let delta = ev
                .roller_delta
                .ok_or("rollers event needs \"roller_delta\"")?;
            if idx >= 2 || delta.abs() != 1 {
                return Err("roller event needs roller 0..2 and roller_delta ±1".into());
            }
            let mut p = vec![0x01, 0x02, 0xFF, 0xFF];
            // the parser maps 0 -> +1, anything else -> -1
            p[2 + idx] = if delta == 1 { 0 } else { 1 };
            p
        }
        ModuleKind::Dial => {
            if let Some(delta) = ev.dial_delta {
                if delta.abs() != 1 {
                    return Err("dial_delta must be ±1".into());
                }
                // the parser maps 0 -> +1, anything else -> -1
                vec![0x03, 0x01, u8::from(delta != 1)]
            } else if let Some(pressed) = ev.dial_pressed {
                vec![0x04, 0x01, u8::from(pressed)]
            } else {
                return Err("dial event needs dial_delta or dial_pressed".into());
            }
        }
        ModuleKind::Base => {
            let orientation = ev
                .orientation
                .as_deref()
                .ok_or("base event needs \"orientation\"")?;
            match orientation {
                "horizontal" => vec![0x01],
                "vertical" => vec![0x00],
                other => return Err(format!("unknown orientation '{other}'")),
            }
        }
    };
    let mut report = vec![0x01, info.input_tag];
    report.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    report.extend_from_slice(&payload);
    report.resize(64, 0); // input reports are 64 B (report id included)
    Ok(report)
}

/// Encode one event as the `0x81` INPUT frame to broadcast to connected hosts.
pub fn event_frame(ev: &InjectEvent) -> Result<Frame, String> {
    let kind = module_kind(&ev.module)?;
    let report = encode_input_report(ev)?;
    Frame::new(kind::INPUT, module_info(kind).pid, report).map_err(|e| format!("input frame: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_every_module_event() {
        let events = parse_events_ndjson(
            r#"
            {"module": "keys", "keys": [0, 3]}
            {"module": "faders", "faders": [10, 20, 30, 40, 50]}
            {"module": "knobs", "knob": 1, "knob_delta": -1}
            {"module": "knobs", "knob_press": 2, "pressed": true}
            {"module": "rollers", "roller": 0, "roller_delta": 1}
            {"module": "dial", "dial_delta": 1, "at_ms": 40}
            {"module": "dial", "dial_pressed": true}
            {"module": "base", "orientation": "horizontal"}
            "#,
        )
        .unwrap();
        assert_eq!(events.len(), 8);
        assert_eq!(events[5].at_ms, 40);

        let f = event_frame(&events[0]).unwrap();
        assert_eq!(f.kind, kind::INPUT);
        assert_eq!(f.pid, 0x01FE);
        assert_eq!(f.payload.len(), 64);
        assert_eq!(&f.payload[..6], &[0x01, 0x34, 17, 0, 0x02, 0x0F]);
        assert_eq!(f.payload[6], 1);
        assert_eq!(f.payload[6 + 3], 1);
        assert_eq!(f.payload[6 + 1], 0);

        // the host-side parser decodes what we encoded
        let inputs = ds_protocol::parse_input(ModuleKind::Keys, &f.payload);
        assert!(matches!(&inputs[0], ds_protocol::Input::Keys(k) if k[0] && k[3] && !k[1]));

        let f = event_frame(&events[1]).unwrap();
        assert_eq!(&f.payload[..9], &[0x01, 0x2A, 7, 0, 0x02, 0x05, 10, 20, 30]);
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Faders, &f.payload)[0],
            ds_protocol::Input::Faders([10, 20, 30, 40, 50])
        ));

        let f = event_frame(&events[2]).unwrap();
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Knobs, &f.payload)[0],
            ds_protocol::Input::Knob {
                index: 1,
                delta: -1
            }
        ));
        let f = event_frame(&events[3]).unwrap();
        assert!(ds_protocol::parse_input(ModuleKind::Knobs, &f.payload)
            .iter()
            .any(|i| matches!(
                i,
                ds_protocol::Input::KnobPress {
                    index: 2,
                    pressed: true
                }
            )));
        let f = event_frame(&events[4]).unwrap();
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Rollers, &f.payload)[0],
            ds_protocol::Input::Roller { index: 0, delta: 1 }
        ));
        let f = event_frame(&events[5]).unwrap();
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Dial, &f.payload)[0],
            ds_protocol::Input::Dial { delta: 1 }
        ));
        let f = event_frame(&events[6]).unwrap();
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Dial, &f.payload)[0],
            ds_protocol::Input::DialPress { pressed: true }
        ));
        let f = event_frame(&events[7]).unwrap();
        assert!(matches!(
            ds_protocol::parse_input(ModuleKind::Base, &f.payload)[0],
            ds_protocol::Input::Orientation(ds_protocol::Orientation::Horizontal)
        ));
    }

    #[test]
    fn rejects_malformed_events() {
        assert!(parse_events_ndjson("{not json").is_err());
        assert!(parse_events_ndjson(r#"{"module": "nope"}"#).is_err());
        assert!(event_frame(&InjectEvent {
            module: "keys".into(),
            at_ms: 0,
            keys: Some(vec![15]),
            faders: None,
            knob: None,
            knob_delta: None,
            knob_press: None,
            pressed: None,
            roller: None,
            roller_delta: None,
            dial_delta: None,
            dial_pressed: None,
            orientation: None,
        })
        .is_err());
    }
}
