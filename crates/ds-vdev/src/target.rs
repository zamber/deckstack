//! Frame targets: the addressable screens of the virtual deck. A chunked image push names
//! its target by `(cmd, index)` on the wire (`0x35`/panel, `0x36`/key N, `0x45`/dial);
//! `ds-vdevctl frames <target>` names them `panel`, `key:N`, `dial`.

use std::fmt;

use ds_device::report::{DIAL_CMD, KEY_CMD, PANEL_CMD};

/// One addressable screen of the virtual deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// The whole 15-key panel (`02 35 …`, index 0).
    Panel,
    /// One key, 0..15 (`02 36 …`).
    Key(u8),
    /// The dial screen (`02 45 …`, index 0).
    Dial,
}

impl Target {
    /// The wire key of this target: `(image command, index)`.
    pub fn wire_key(self) -> (u8, u8) {
        match self {
            Target::Panel => (PANEL_CMD, 0),
            Target::Key(i) => (KEY_CMD, i),
            Target::Dial => (DIAL_CMD, 0),
        }
    }

    /// Parse the `ds-vdevctl frames` spelling: `panel`, `key:N` (N < 15), `dial`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "panel" => Some(Target::Panel),
            "dial" => Some(Target::Dial),
            _ => s
                .strip_prefix("key:")
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|&i| i < 15)
                .map(Target::Key),
        }
    }

    /// The reverse of [`Target::parse`], e.g. for `ds-vdevctl frames` output.
    pub fn name(self) -> String {
        match self {
            Target::Panel => "panel".to_string(),
            Target::Key(i) => format!("key:{i}"),
            Target::Dial => "dial".to_string(),
        }
    }

    /// Every target a module can address, for `frames` with no argument.
    pub fn all_for(pid: u16) -> Vec<Target> {
        match pid {
            p if p == ds_device::module::module_info(ds_protocol::ModuleKind::Keys).pid => {
                vec![Target::Panel]
                    .into_iter()
                    .chain((0..15).map(Target::Key))
                    .collect()
            }
            p if p == ds_device::module::module_info(ds_protocol::ModuleKind::Dial).pid => {
                vec![Target::Dial]
            }
            _ => Vec::new(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_round_trip() {
        for t in [Target::Panel, Target::Key(0), Target::Key(14), Target::Dial] {
            assert_eq!(Target::parse(&t.name()), Some(t));
        }
        assert_eq!(Target::parse("key:15"), None);
        assert_eq!(Target::parse("key:x"), None);
        assert_eq!(Target::parse("keys"), None);
        assert_eq!(Target::Key(3).wire_key(), (0x36, 3));
        assert_eq!(Target::Panel.wire_key(), (0x35, 0));
        assert_eq!(Target::Dial.wire_key(), (0x45, 0));
    }
}
