// SPDX-License-Identifier: GPL-3.0-or-later
//! The menu the Bluetooth icon opens, as a tree of rows -- built from a snapshot, no bus.
//!
//! ```text
//! Bluetooth: 2 devices connected     (heading; cannot be chosen)
//! [x] Bluetooth Enabled
//! ---------------------------
//! [x] WH-1000XM4  80%
//! [ ] MX Master 3
//! ---------------------------
//! Bluetooth Settings...
//! ```
//!
//! Paired devices only, connected first; the check is "connected", and choosing a row connects
//! or disconnects it. Finding and pairing new devices is the panel's: a scan's results churn too
//! much for a menu that stays the same between two clicks.
//!
//! Ids are fixed per row and per device for as long as the daemon runs ([`Ids`]), so a click that
//! arrives after the list changed still means the device that was clicked -- the same rule as
//! `wlrix-network`'s menu.

use std::collections::{HashMap, HashSet};

use crate::bluez::Snapshot;
use crate::indicator::{Extra, label, view};

/// What choosing a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Switch the adapter on or off.
    Power(bool),
    /// Connect (`true`) or disconnect a device, by its BlueZ path.
    Device(String, bool),
    /// Open Bluetooth Settings.
    OpenSettings,
}

/// A row's check box or radio button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Check(bool),
    Radio(bool),
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub toggle: Option<Toggle>,
    pub children: Vec<Row>,
    pub action: Option<Action>,
}

impl Row {
    fn new(id: i32, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            separator: false,
            toggle: None,
            children: Vec::new(),
            action: None,
        }
    }

    fn heading(id: i32, label: impl Into<String>) -> Self {
        Self {
            enabled: false,
            ..Self::new(id, label)
        }
    }

    fn separator(id: i32) -> Self {
        Self {
            separator: true,
            ..Self::new(id, "")
        }
    }

    /// The label as dbusmenu carries it, with `_` doubled so it is not read as a mnemonic.
    pub fn wire_label(&self) -> String {
        self.label.replace('_', "__")
    }

    /// This row or one under it, by id.
    pub fn find(&self, id: i32) -> Option<&Row> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }
}

const HEADING: i32 = 1;
const POWER: i32 = 2;
const DEVICES_SEPARATOR: i32 = 3;
const SETTINGS_SEPARATOR: i32 = 4;
const SETTINGS: i32 = 5;
/// Devices are numbered from here, in the order they were first seen.
const FIRST_DEVICE: i32 = 100;

/// Each device's row id, kept for the life of the daemon.
#[derive(Debug, Default)]
pub struct Ids {
    by_path: HashMap<String, i32>,
    next: i32,
}

impl Ids {
    fn get(&mut self, path: &str) -> i32 {
        if let Some(id) = self.by_path.get(path) {
            return *id;
        }
        let id = FIRST_DEVICE + self.next;
        self.next += 1;
        self.by_path.insert(path.to_owned(), id);
        id
    }
}

/// Build the menu's root, whose children are the rows. `connecting` is the devices this daemon
/// is connecting, whose rows say so and cannot be chosen again until it is done.
pub fn build(
    snapshot: Option<&Snapshot>,
    connecting: &HashSet<String>,
    hard_blocked: bool,
    ids: &mut Ids,
) -> Row {
    let mut root = Row::new(0, "");
    let extra = Extra {
        hard_blocked,
        connecting: !connecting.is_empty(),
    };
    let shown = view(snapshot, extra, crate::config::Indicator::Auto);
    root.children.push(Row::heading(HEADING, shown.title));
    let Some(snapshot) = snapshot else {
        return root;
    };
    let Some(adapter) = &snapshot.adapter else {
        return root;
    };

    let on = adapter.powered;
    root.children.push(Row {
        toggle: Some(Toggle::Check(on)),
        action: Some(Action::Power(!on)),
        // Nothing in software lifts a hardware switch.
        enabled: !hard_blocked,
        ..Row::new(POWER, "Bluetooth Enabled")
    });

    if on {
        let paired: Vec<_> = snapshot.paired().collect();
        if !paired.is_empty() {
            root.children.push(Row::separator(DEVICES_SEPARATOR));
        }
        for device in paired {
            let busy = connecting.contains(&device.path);
            let text = if busy {
                format!("{} (connecting...)", device.name)
            } else {
                label(device)
            };
            root.children.push(Row {
                toggle: Some(Toggle::Check(device.connected)),
                action: Some(Action::Device(device.path.clone(), !device.connected)),
                enabled: !busy,
                ..Row::new(ids.get(&device.path), text)
            });
        }
    }

    root.children.push(Row::separator(SETTINGS_SEPARATOR));
    root.children.push(Row {
        action: Some(Action::OpenSettings),
        ..Row::new(SETTINGS, "Bluetooth Settings...")
    });
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluez::{adapter, device};

    fn snapshot(powered: bool) -> Snapshot {
        Snapshot {
            adapter: Some(adapter(powered, if powered { "on" } else { "off" })),
            devices: vec![
                device("Headset", true, true, Some(80)),
                device("Mouse", true, false, None),
                device("Stranger", false, false, None),
            ],
        }
    }

    fn labels(row: &Row) -> Vec<String> {
        row.children
            .iter()
            .map(|child| child.label.clone())
            .collect()
    }

    #[test]
    fn paired_devices_with_their_state_and_battery() {
        let menu = build(
            Some(&snapshot(true)),
            &HashSet::new(),
            false,
            &mut Ids::default(),
        );
        assert_eq!(
            labels(&menu),
            [
                "Bluetooth: Headset",
                "Bluetooth Enabled",
                "",
                "Headset  80%",
                "Mouse",
                "",
                "Bluetooth Settings..."
            ]
        );
        assert_eq!(menu.children[1].toggle, Some(Toggle::Check(true)));
        assert_eq!(menu.children[1].action, Some(Action::Power(false)));
        assert_eq!(menu.children[3].toggle, Some(Toggle::Check(true)));
        assert_eq!(
            menu.children[4].action,
            Some(Action::Device("/org/bluez/hci0/dev_Mouse".into(), true))
        );
    }

    #[test]
    fn off_lists_no_devices_and_the_hardware_switch_grays_the_toggle() {
        let menu = build(
            Some(&snapshot(false)),
            &HashSet::new(),
            false,
            &mut Ids::default(),
        );
        assert_eq!(
            labels(&menu),
            [
                "Bluetooth: Off",
                "Bluetooth Enabled",
                "",
                "Bluetooth Settings..."
            ]
        );
        assert_eq!(menu.children[1].action, Some(Action::Power(true)));
        let blocked = build(
            Some(&snapshot(false)),
            &HashSet::new(),
            true,
            &mut Ids::default(),
        );
        assert!(!blocked.children[1].enabled);
    }

    #[test]
    fn a_device_being_connected_says_so_and_waits() {
        let busy = HashSet::from(["/org/bluez/hci0/dev_Mouse".to_owned()]);
        let menu = build(Some(&snapshot(true)), &busy, false, &mut Ids::default());
        let row = menu
            .children
            .iter()
            .find(|row| row.label.starts_with("Mouse"))
            .unwrap();
        assert_eq!(row.label, "Mouse (connecting...)");
        assert!(!row.enabled);
    }

    #[test]
    fn device_ids_stay_put_when_the_list_changes() {
        let mut ids = Ids::default();
        let mut shot = snapshot(true);
        let before = build(Some(&shot), &HashSet::new(), false, &mut ids);
        let mouse = before
            .children
            .iter()
            .find(|r| r.label == "Mouse")
            .unwrap()
            .id;
        shot.devices.remove(0);
        let after = build(Some(&shot), &HashSet::new(), false, &mut ids);
        assert_eq!(
            after
                .children
                .iter()
                .find(|r| r.label == "Mouse")
                .unwrap()
                .id,
            mouse
        );
    }

    #[test]
    fn no_bluetoothd_is_only_a_heading() {
        assert_eq!(
            labels(&build(None, &HashSet::new(), false, &mut Ids::default())),
            ["Bluetooth"]
        );
    }
}
