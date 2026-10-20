// SPDX-License-Identifier: GPL-3.0-or-later
//! rfkill: the kernel's radio switches, which sit beneath BlueZ.
//!
//! A radio can be off in two ways BlueZ cannot undo by itself. A **hard** block is a physical
//! switch or a firmware key -- nothing in software lifts it, so the icon says so and the menu's
//! toggle is grayed out. A **soft** block is airplane mode, `rfkill block bluetooth`, or the
//! state systemd-rfkill restored at boot; BlueZ reports it as `PowerState = "off-blocked"` and
//! refuses `Powered = true` until it is lifted.
//!
//! Lifting it is a write to `/dev/rfkill`, which systemd's udev rules tag `uaccess`: the user at
//! the seat may write it, nobody else, and no polkit prompt is involved -- the same path
//! `rfkill unblock` and every desktop's Bluetooth switch take.

use std::io::Write;
use std::path::Path;

/// `RFKILL_TYPE_BLUETOOTH`.
const TYPE_BLUETOOTH: u8 = 2;
/// `RFKILL_OP_CHANGE_ALL`: set the soft block of every switch of a type.
const OP_CHANGE_ALL: u8 = 3;

/// Whether some Bluetooth radio is blocked at the hardware switch.
pub fn hard_blocked() -> bool {
    hard_blocked_in(Path::new("/sys/class/rfkill"))
}

fn hard_blocked_in(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let read =
            |name: &str| std::fs::read_to_string(entry.path().join(name)).unwrap_or_default();
        read("type").trim() == "bluetooth" && read("hard").trim() == "1"
    })
}

/// The event `/dev/rfkill` takes: `struct rfkill_event` from `<linux/rfkill.h>`, the eight-byte
/// version every kernel accepts.
fn unblock_event() -> [u8; 8] {
    let mut event = [0u8; 8];
    // idx (u32) is ignored for CHANGE_ALL; then type, op, soft, hard.
    event[4] = TYPE_BLUETOOTH;
    event[5] = OP_CHANGE_ALL;
    event[6] = 0; // soft: not blocked
    event
}

/// Lift the soft block on every Bluetooth radio.
///
/// `WLRIX_BLUETOOTH_RFKILL` names another file to write instead, so a test against a fake BlueZ
/// never touches the machine's real radios.
pub fn unblock() -> std::io::Result<()> {
    let device = std::env::var_os("WLRIX_BLUETOOTH_RFKILL").unwrap_or_else(|| "/dev/rfkill".into());
    std::fs::OpenOptions::new()
        .write(true)
        .open(device)?
        .write_all(&unblock_event())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_unblocks_bluetooth_and_nothing_else() {
        assert_eq!(unblock_event(), [0, 0, 0, 0, 2, 3, 0, 0]);
    }

    #[test]
    fn only_a_bluetooth_switch_that_is_hard_blocked_counts() {
        let root = std::env::temp_dir().join(format!("wlrix-bt-rfkill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, kind, hard) in [("rfkill0", "wlan", "1"), ("rfkill1", "bluetooth", "0")] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), kind).unwrap();
            std::fs::write(dir.join("hard"), hard).unwrap();
        }
        assert!(
            !hard_blocked_in(&root),
            "a blocked Wi-Fi switch is not Bluetooth's"
        );
        std::fs::write(root.join("rfkill1/hard"), "1\n").unwrap();
        assert!(hard_blocked_in(&root));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!hard_blocked_in(&root), "no rfkill at all is not blocked");
    }
}
