// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX's Bluetooth status.
//!
//! BlueZ's adapter and paired devices, published as a StatusNotifierItem so `wlrix-tray` draws
//! it: an icon that says whether Bluetooth is on and anything connected, a tooltip naming what is,
//! and a menu that switches the radio and connects paired devices. It is also BlueZ's pairing
//! agent, so pairing from Bluetooth Settings can ask the person to confirm a code. On a machine
//! with no adapter the item is `Passive`, which the tray hides. See
//! `_docs/wlrix-bluetooth-roadmap.md`.
//!
//! - [`bluez`] reads bluetoothd on the system bus, says when it changes, and asks it to act.
//! - [`rfkill`] reads the hardware switch and lifts a soft block.
//! - [`indicator`] turns what was read into an icon, a status, a tooltip, and when a battery is
//!   worth a warning. No bus; unit-tested.
//! - [`menu`] builds the menu. No bus; unit-tested.
//! - [`agent`] is the pairing agent, and [`prompt`] the dialog it puts up.
//! - [`sni`] is the item on the session bus, and its registration with the tray; [`dbusmenu`]
//!   serves the menu beside it.
//! - [`daemon`] is the loop between them.

pub mod agent;
pub mod bluez;
pub mod config;
pub mod daemon;
pub mod dbusmenu;
pub mod indicator;
pub mod menu;
pub mod pidfile;
pub mod prompt;
pub mod rfkill;
pub mod signals;
pub mod sni;
pub mod xdg;
