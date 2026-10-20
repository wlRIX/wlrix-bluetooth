// SPDX-License-Identifier: GPL-3.0-or-later
//! What the tray is told to draw, and when a device's battery is worth a notification -- worked
//! out from what BlueZ says. No bus here; every case is a unit test.
//!
//! # The icon
//!
//! Adwaita's Bluetooth set, found by the tray in the `wlrix` icon theme:
//!
//! | State                               | Icon                          |
//! |-------------------------------------|-------------------------------|
//! | on, a device connected              | `bluetooth-active`            |
//! | on, a connection being made         | `bluetooth-acquiring`         |
//! | on, nothing connected               | `bluetooth-disconnected`      |
//! | off, or blocked in software         | `bluetooth-disabled`          |
//! | off at the hardware switch          | `bluetooth-hardware-disabled` |
//!
//! # Hidden, and when
//!
//! `Passive`, which the tray hides, when there is no adapter -- or bluetoothd is not running --
//! and when the person said `indicator = "hidden"`. Off is still shown: the menu is where it is
//! switched back on.

use crate::bluez::{Device, Snapshot};
use crate::config::Indicator;

/// `Status`, as the StatusNotifierItem specification spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passive,
    Active,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passive => "Passive",
            Status::Active => "Active",
        }
    }
}

/// Everything the item publishes that can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub status: Status,
    pub icon: String,
    pub title: String,
    pub body: String,
}

/// What the item shows besides what BlueZ said: the rfkill hardware switch, and whether a
/// connection this daemon started is still being made.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extra {
    pub hard_blocked: bool,
    pub connecting: bool,
}

/// A device's name, with its battery when it reports one.
pub fn label(device: &Device) -> String {
    match device.battery {
        Some(percent) => format!("{}  {percent}%", device.name),
        None => device.name.clone(),
    }
}

/// Work out what to show.
pub fn view(snapshot: Option<&Snapshot>, extra: Extra, setting: Indicator) -> View {
    let Some((snapshot, adapter)) =
        snapshot.and_then(|snapshot| Some((snapshot, snapshot.adapter.as_ref()?)))
    else {
        return View {
            status: Status::Passive,
            icon: "bluetooth-disabled-symbolic".to_owned(),
            title: "Bluetooth".to_owned(),
            body: "No Bluetooth adapter".to_owned(),
        };
    };
    let status = match setting {
        Indicator::Auto => Status::Active,
        Indicator::Hidden => Status::Passive,
    };
    let off = |icon: &str, body: &str| View {
        status,
        icon: format!("{icon}-symbolic"),
        title: "Bluetooth: Off".to_owned(),
        body: body.to_owned(),
    };
    if extra.hard_blocked {
        return off(
            "bluetooth-hardware-disabled",
            "Turned off by the hardware switch",
        );
    }
    if adapter.blocked() {
        return off("bluetooth-disabled", "Turned off (airplane mode or rfkill)");
    }
    if !adapter.powered {
        return off("bluetooth-disabled", "Bluetooth is off");
    }

    let connected: Vec<&Device> = snapshot.connected().collect();
    let icon = if !connected.is_empty() {
        "bluetooth-active"
    } else if extra.connecting {
        "bluetooth-acquiring"
    } else {
        "bluetooth-disconnected"
    };
    let title = match connected.len() {
        0 => "Bluetooth: On".to_owned(),
        1 => format!("Bluetooth: {}", connected[0].name),
        n => format!("Bluetooth: {n} devices connected"),
    };
    let body = if connected.is_empty() {
        if extra.connecting {
            "Connecting...".to_owned()
        } else {
            "No devices connected".to_owned()
        }
    } else {
        connected
            .iter()
            .map(|device| label(device))
            .collect::<Vec<_>>()
            .join("\n")
    };
    View {
        status,
        icon: format!("{icon}-symbolic"),
        title,
        body,
    }
}

/// The battery levels worth a notification, lowest last.
const LEVELS: [u8; 2] = [20, 10];
/// Above this a device counts as charged, and every level is armed again.
const RECHARGED: u8 = 30;

/// Whether a device's battery is worth a warning, given the level last warned about.
///
/// Answers the new level to remember: `Some(level)` when this reading crosses one not yet
/// warned about (warn), `None` once it has been charged past [`RECHARGED`] (forget), and the old
/// level otherwise (nothing to say).
pub fn battery_level(warned: Option<u8>, percent: u8) -> (Option<u8>, bool) {
    if percent > RECHARGED {
        return (None, false);
    }
    let reached = LEVELS
        .iter()
        .copied()
        .filter(|level| percent <= *level)
        .min();
    match (reached, warned) {
        (Some(level), Some(before)) if level < before => (Some(level), true),
        (Some(level), None) => (Some(level), true),
        _ => (warned, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluez::{adapter, device};

    fn snapshot(powered: bool, devices: Vec<Device>) -> Snapshot {
        Snapshot {
            adapter: Some(adapter(powered, if powered { "on" } else { "off" })),
            devices,
        }
    }

    #[test]
    fn no_adapter_is_hidden() {
        assert_eq!(
            view(None, Extra::default(), Indicator::Auto).status,
            Status::Passive
        );
        let none = Snapshot::default();
        assert_eq!(
            view(Some(&none), Extra::default(), Indicator::Auto).status,
            Status::Passive
        );
    }

    #[test]
    fn off_is_shown_so_it_can_be_turned_on() {
        let shot = snapshot(false, vec![]);
        let shown = view(Some(&shot), Extra::default(), Indicator::Auto);
        assert_eq!(shown.status, Status::Active);
        assert_eq!(shown.icon, "bluetooth-disabled-symbolic");
        let hard = view(
            Some(&shot),
            Extra {
                hard_blocked: true,
                connecting: false,
            },
            Indicator::Auto,
        );
        assert_eq!(hard.icon, "bluetooth-hardware-disabled-symbolic");
        assert_eq!(
            view(Some(&shot), Extra::default(), Indicator::Hidden).status,
            Status::Passive
        );
    }

    #[test]
    fn what_is_connected_is_the_tooltip() {
        let shot = snapshot(
            true,
            vec![
                device("Headset", true, true, Some(80)),
                device("Mouse", true, true, None),
                device("Phone", true, false, None),
            ],
        );
        let shown = view(Some(&shot), Extra::default(), Indicator::Auto);
        assert_eq!(shown.icon, "bluetooth-active-symbolic");
        assert_eq!(shown.title, "Bluetooth: 2 devices connected");
        assert_eq!(shown.body, "Headset  80%\nMouse");
    }

    #[test]
    fn on_with_nothing_connected_or_connecting() {
        let shot = snapshot(true, vec![device("Phone", true, false, None)]);
        let idle = view(Some(&shot), Extra::default(), Indicator::Auto);
        assert_eq!(idle.icon, "bluetooth-disconnected-symbolic");
        let busy = view(
            Some(&shot),
            Extra {
                hard_blocked: false,
                connecting: true,
            },
            Indicator::Auto,
        );
        assert_eq!(busy.icon, "bluetooth-acquiring-symbolic");
    }

    #[test]
    fn each_battery_level_warns_once_until_charged() {
        assert_eq!(battery_level(None, 50), (None, false));
        assert_eq!(battery_level(None, 19), (Some(20), true));
        assert_eq!(
            battery_level(Some(20), 15),
            (Some(20), false),
            "still the same level"
        );
        assert_eq!(battery_level(Some(20), 9), (Some(10), true));
        assert_eq!(
            battery_level(Some(10), 25),
            (Some(10), false),
            "not charged enough"
        );
        assert_eq!(battery_level(Some(10), 31), (None, false));
        // A device first seen at 5% warns once, at the lowest level.
        assert_eq!(battery_level(None, 5), (Some(10), true));
    }
}
