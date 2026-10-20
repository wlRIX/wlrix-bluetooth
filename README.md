# wlrix-bluetooth

Bluetooth status for the wlRIX desktop: BlueZ's adapter and paired devices, as an icon in `wlrix-tray`. Whether
Bluetooth is on and what is connected at a glance, a menu that switches the radio and connects or disconnects paired
devices in a click, and BlueZ's pairing agent, so pairing from Bluetooth Settings can ask whether a headset shows the
same number. Nothing at all on a machine with no adapter.

- **Language:** Rust
- **License:** GPL-3.0-or-later

The last of the laptop pieces, beside `wlrix-network`, `wlrix-power` and `wlrix-osd`; the plan is
`_docs/wlrix-bluetooth-roadmap.md` in the workspace. The windows are in `wlrix-apps`: the pairing dialog
`wlrix-bluetooth-prompt`, and Bluetooth Settings (`wlrix-settings-bluetooth`), where devices are found, paired and
forgotten.

## What it is

A daemon with no window. It reads bluetoothd on the system bus and publishes a **StatusNotifierItem** on the session
bus, which the tray draws -- the same way `wlrix-network` does.

| State                              | Icon                              | Tooltip                                  |
|------------------------------------|-----------------------------------|------------------------------------------|
| on, a device connected             | `bluetooth-active`                | each connected device, with its battery  |
| on, a connection being made        | `bluetooth-acquiring`             | "Connecting..."                          |
| on, nothing connected              | `bluetooth-disconnected`          | "No devices connected"                   |
| off, or blocked by rfkill          | `bluetooth-disabled`              | which of the two                         |
| off at the hardware switch         | `bluetooth-hardware-disabled`     | "Turned off by the hardware switch"      |

All `-symbolic`, from the `wlrix` icon theme, which inherits Adwaita. **The item is `Passive`, which the tray hides,
when there is no adapter** or bluetoothd is not running. Off is still shown, so it can be switched back on from the
menu. The first adapter by path (`hci0`) is the one shown; a second is a spare.

## The menu

```text
Bluetooth: 2 devices connected
[x] Bluetooth Enabled
--------------------------
[x] WH-1000XM4  80%
[ ] MX Master 3
--------------------------
Bluetooth Settings...
```

Paired devices only, connected first, with the battery of those that report one (`org.bluez.Battery1`). The check is
"connected": choosing a row connects or disconnects it. A connection takes a few seconds -- BlueZ's `Connect` answers
only once the device has connected or given up -- so it runs on a thread of its own, the row says *(connecting...)*
meanwhile, and a failure is a notification with BlueZ's reason ("Page Timeout" for a device out of range).

**Bluetooth Enabled** sets the adapter's `Powered`. When rfkill has it soft-blocked (airplane mode, `rfkill block`),
turning it on first lifts the block through `/dev/rfkill`, which systemd's udev rules let the user at the seat write.
A hardware switch grays the row out: nothing in software lifts it.

Finding and pairing new devices is the panel's, not the menu's: a scan's results churn too much for a menu.

## Pairing

The daemon registers `org.bluez.Agent1` at `/org/wlrix/bluetooth/agent` as BlueZ's **default agent**, capability
`KeyboardDisplay`, again whenever bluetoothd restarts. Every call is checked to come from the owner of `org.bluez`:
the object is on the system bus, and a `RequestConfirmation` from anyone else would be a convincing pairing dialog.

| Agent call                                  | Dialog                                                                 |
|---------------------------------------------|------------------------------------------------------------------------|
| `RequestConfirmation`                       | "Check that *device* shows this number" -- Pair / Cancel               |
| `DisplayPasskey`, `DisplayPinCode`          | "Type this number on *device*" -- closed when it has paired, or on `Cancel` |
| `RequestPinCode`, `RequestPasskey`          | a field for the code                                                   |
| `RequestAuthorization`, `AuthorizeService`  | "Allow *device* to connect?" -- Allow / Deny, naming the service       |

The dialog is `wlrix-bluetooth-prompt`, one line of JSON in and one out, like `wlrix-network-prompt`;
`WLRIX_BLUETOOTH_PROMPT` names another program, for testing. Closing it is a refusal. Bluetooth Settings marks a
device it pairs as trusted, so `AuthorizeService` only comes for devices paired elsewhere.

## Notifications

Only two, the person's choice: **a connection that failed**, and **a device's battery** falling to 20 % and again to
10 %, once per level until it has been charged past 30 %. Ordinary connects and disconnects are quiet -- the icon and
the menu are for those.

## Configuration

`~/.config/wlrix/bluetooth.toml`, then `/etc/wlrix/bluetooth.toml`. The first file found wins outright, unknown keys
are refused, and `wlrix-bluetooth --check-config <path>` says whether a file would be accepted.

```toml
indicator = "auto"        # auto | hidden
battery_warnings = true   # notify when a device's battery runs low
```

`hidden` keeps the icon out of the tray; the agent keeps running, since pairing from the panel needs it. `SIGHUP`
re-reads the file and `SIGTERM` stops the daemon; the pidfile is `$XDG_RUNTIME_DIR/wlrix-bluetooth.pid`. Both keys are
in the settings daemon's schema, and the panel has the tray checkbox.

## Using it

`wlrix-session` starts it. `wlrix-bluetooth --status` reads BlueZ once and prints what the icon would show:

```console
$ wlrix-bluetooth --status
status:  Active
icon:    bluetooth-active-symbolic
title:   Bluetooth: WH-1000XM4
         WH-1000XM4  80%
adapter: /org/bluez/hci0 laptop (on)
device:  38:18:4C:00:00:01 WH-1000XM4, paired, connected
```

The item's `Id` is `wlrix-bluetooth`, for ordering or hiding it with `[[item]]` in `tray.toml`.

## How it stays current

**Every BlueZ signal means "read it all again"**: `InterfacesAdded`, `InterfacesRemoved`, any `PropertiesChanged`
under `/org/bluez`, or `org.bluez` changing owner arms a quarter-second timer, and when it fires one
`GetManagedObjects` reads every adapter, device and battery. A device connecting is half a dozen property changes;
that is one read. The tray restarting is watched for, and the item registers again.

## Testing it

The mapping from BlueZ's state to the icon, the menu, the battery-warning levels, the agent's service names and the
prompt's protocol are unit-tested.

```console
$ cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check
```

A machine with no adapter can test everything with `examples/fake_bluez.rs`: one adapter, a paired headset with a
battery, a paired mouse, and a speaker out of range, on a private bus that `DBUS_SYSTEM_BUS_ADDRESS` names. `Pair` on
a device asks the agent as real numeric comparison does, and stdin drives the rest (`power off`, `block`,
`battery 15`, `found Pixel 8`, `confirm|display|pin <name>`, `cancel`):

```console
$ export DBUS_SYSTEM_BUS_ADDRESS=$(dbus-daemon --session --print-address --fork)
$ cargo run --example fake_bluez
$ WLRIX_BLUETOOTH_RFKILL=/tmp/rfkill.bin wlrix-bluetooth
```

`WLRIX_BLUETOOTH_RFKILL` sends the unblock to a file instead of `/dev/rfkill`, so a test never touches the machine's
own radios. Run the session side on a private bus too (`env -u WAYLAND_DISPLAY dbus-run-session`), with a nested
compositor and `wlrix-tray`; the menu can be driven with `busctl --user call <item name> /MenuBar
com.canonical.dbusmenu Event isvu <id> clicked i 0 0`.
