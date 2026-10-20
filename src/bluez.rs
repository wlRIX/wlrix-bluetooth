// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading BlueZ, on the system bus, and asking it to do things.
//!
//! Everything comes from one `GetManagedObjects` on `org.bluez`: the adapters, the devices each
//! knows, and a battery for each device that reports one. BlueZ announces changes with
//! `InterfacesAdded` and `InterfacesRemoved` from its object manager and `PropertiesChanged` on
//! each object; as in `wlrix-network`, any of them only means "read it all again", which here is a
//! single call however many devices there are.
//!
//! # One adapter
//!
//! A laptop has one, and the rare second (a USB dongle beside the built-in one) is a spare. The
//! first by path -- `hci0` -- is the one shown and switched; the panel can still see the rest.
//!
//! # Connecting takes a while
//!
//! `Device1.Connect` answers only when the device has connected or given up, which for a headset
//! being taken out of its case is several seconds and for one out of range is half a minute. So
//! it runs on a thread of its own, with its own connection and a long timeout, and its outcome
//! comes back to the loop through a channel -- see [`connect`].

use std::collections::HashMap;
use std::time::Duration;

use zbus::MatchRule;
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

pub const BLUEZ: &str = "org.bluez";
const ROOT: &str = "/org/bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const DBUS: &str = "org.freedesktop.DBus";

/// How long a connection attempt may take. BlueZ gives up on its own well before this; it is
/// only here so a bluetoothd that never answers cannot hold a thread forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// `a{oa{sa{sv}}}`: every object, interface by interface.
type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// The adapter, as far as the icon and the menu care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub path: String,
    /// What other devices see this computer as: its alias, which defaults to the host name.
    pub name: String,
    pub powered: bool,
    /// BlueZ's `PowerState`: `on`, `off`, `off-enabling`, `on-disabling` or `off-blocked`.
    pub power_state: String,
}

impl Adapter {
    /// Switched off through rfkill, which `Powered = true` alone cannot undo.
    pub fn blocked(&self) -> bool {
        self.power_state == "off-blocked"
    }
}

/// A device the adapter knows: paired, or merely seen by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    pub address: String,
    /// The alias -- the device's own name unless somebody renamed it -- or the address.
    pub name: String,
    /// BlueZ's guess at what it is (`audio-headphones`, `input-mouse`, `phone`), an icon name.
    pub icon: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// `org.bluez.Battery1`, for devices that report one.
    pub battery: Option<u8>,
}

/// Everything read in one go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub adapter: Option<Adapter>,
    /// The adapter's devices, sorted: connected first, then by name.
    pub devices: Vec<Device>,
}

impl Snapshot {
    /// The paired devices -- the ones the menu lists.
    pub fn paired(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|device| device.paired)
    }

    pub fn connected(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|device| device.connected)
    }

    pub fn device(&self, path: &str) -> Option<&Device> {
        self.devices.iter().find(|device| device.path == path)
    }
}

/// Read BlueZ. `Err` when bluetoothd is not running -- or, on a machine with no adapter, cannot be
/// started -- which is an ordinary "no adapter" for the icon.
pub fn read(connection: &zbus::blocking::Connection) -> zbus::Result<Snapshot> {
    let reply = connection.call_method(
        Some(BLUEZ),
        "/",
        Some(OBJECT_MANAGER),
        "GetManagedObjects",
        &(),
    )?;
    let objects: Objects = reply.body().deserialize()?;
    Ok(parse(&objects))
}

fn parse(objects: &Objects) -> Snapshot {
    let mut adapters: Vec<Adapter> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let properties = interfaces.get(ADAPTER)?;
            Some(Adapter {
                path: path.to_string(),
                name: text(properties, "Alias")
                    .or_else(|| text(properties, "Name"))
                    .unwrap_or_default(),
                powered: take(properties, "Powered").unwrap_or(false),
                // Older BlueZ has no PowerState; Powered says as much as it can.
                power_state: text(properties, "PowerState").unwrap_or_else(|| {
                    if take(properties, "Powered").unwrap_or(false) {
                        "on".to_owned()
                    } else {
                        "off".to_owned()
                    }
                }),
            })
        })
        .collect();
    adapters.sort_by(|a, b| a.path.cmp(&b.path));
    let Some(adapter) = adapters.into_iter().next() else {
        return Snapshot::default();
    };

    let mut devices: Vec<Device> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let properties = interfaces.get(DEVICE)?;
            let owner: OwnedObjectPath = take(properties, "Adapter")?;
            if owner.as_str() != adapter.path {
                return None;
            }
            let address = text(properties, "Address").unwrap_or_default();
            Some(Device {
                path: path.to_string(),
                name: text(properties, "Alias")
                    .or_else(|| text(properties, "Name"))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| address.clone()),
                address,
                icon: text(properties, "Icon").unwrap_or_default(),
                paired: take(properties, "Paired").unwrap_or(false),
                trusted: take(properties, "Trusted").unwrap_or(false),
                connected: take(properties, "Connected").unwrap_or(false),
                battery: interfaces
                    .get(BATTERY)
                    .and_then(|battery| take::<u8>(battery, "Percentage")),
            })
        })
        .collect();
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Snapshot {
        adapter: Some(adapter),
        devices,
    }
}

fn take<T>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    map.get(key)?.try_clone().ok()?.try_into().ok()
}

fn text(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    take::<String>(map, key)
}

/// Switch the adapter on or off.
pub fn set_powered(
    connection: &zbus::blocking::Connection,
    adapter: &str,
    on: bool,
) -> zbus::Result<()> {
    connection.call_method(
        Some(BLUEZ),
        adapter,
        Some(PROPERTIES),
        "Set",
        &(ADAPTER, "Powered", Value::from(on)),
    )?;
    Ok(())
}

/// Disconnect a device. Quick: BlueZ answers once the links are down.
pub fn disconnect(connection: &zbus::blocking::Connection, device: &str) -> zbus::Result<()> {
    connection.call_method(Some(BLUEZ), device, Some(DEVICE), "Disconnect", &())?;
    Ok(())
}

/// Connect a device, on a thread of its own; `done` is sent the device's path and, when it
/// failed, why. See the module comment.
pub fn connect(device: String, done: calloop::channel::Sender<(String, Option<String>)>) {
    let spawned = std::thread::Builder::new()
        .name("bluez-connect".into())
        .spawn(move || {
            let result = zbus::blocking::connection::Builder::system()
                .and_then(|builder| builder.method_timeout(CONNECT_TIMEOUT).build())
                .and_then(|connection| {
                    connection.call_method(
                        Some(BLUEZ),
                        device.as_str(),
                        Some(DEVICE),
                        "Connect",
                        &(),
                    )
                });
            let _ = done.send((device, result.err().map(|err| refusal(&err))));
        });
    if let Err(err) = spawned {
        eprintln!("wlrix-bluetooth: could not start a connection thread: {err}");
    }
}

/// What BlueZ said when it refused, without the D-Bus error name in front -- "Page Timeout"
/// rather than "org.bluez.Error.Failed: Page Timeout".
pub fn refusal(err: &zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(_, Some(description), _) => description.clone(),
        other => other.to_string(),
    }
}

/// Ask for every signal that can change what is shown.
pub fn subscribe(connection: &zbus::blocking::Connection) -> zbus::Result<()> {
    let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(BLUEZ)?
            .interface(PROPERTIES)?
            .member("PropertiesChanged")?
            .path_namespace(ROOT)?
            .build(),
    )?;
    for member in ["InterfacesAdded", "InterfacesRemoved"] {
        bus.add_match_rule(
            MatchRule::builder()
                .msg_type(Type::Signal)
                .sender(BLUEZ)?
                .interface(OBJECT_MANAGER)?
                .member(member)?
                .build(),
        )?;
    }
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(DBUS)?
            .interface(DBUS)?
            .member("NameOwnerChanged")?
            .add_arg(BLUEZ)?
            .build(),
    )?;
    Ok(())
}

/// Start the thread that turns those signals into "something changed" on the loop.
pub fn watch(
    connection: zbus::blocking::Connection,
    changed: calloop::channel::Sender<()>,
) -> Result<(), String> {
    subscribe(&connection).map_err(|err| format!("could not watch BlueZ: {err}"))?;
    std::thread::Builder::new()
        .name("bluez".into())
        .spawn(move || {
            for message in zbus::blocking::MessageIterator::from(&connection) {
                let Ok(message) = message else { continue };
                // The reads share this connection, so their replies come through here too.
                if message.message_type() != Type::Signal {
                    continue;
                }
                if changed.send(()).is_err() {
                    return;
                }
            }
        })
        .map_err(|err| format!("could not start the BlueZ thread: {err}"))?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn device(name: &str, paired: bool, connected: bool, battery: Option<u8>) -> Device {
    Device {
        path: format!("/org/bluez/hci0/dev_{name}"),
        address: "00:11:22:33:44:55".into(),
        name: name.into(),
        icon: "audio-headphones".into(),
        paired,
        trusted: paired,
        connected,
        battery,
    }
}

#[cfg(test)]
pub(crate) fn adapter(powered: bool, power_state: &str) -> Adapter {
    Adapter {
        path: "/org/bluez/hci0".into(),
        name: "laptop".into(),
        powered,
        power_state: power_state.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
        value.into().try_into().unwrap()
    }

    fn properties(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }

    fn path(text: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(text).unwrap()
    }

    fn laptop() -> Objects {
        let mut objects = Objects::new();
        objects.insert(
            path("/org/bluez/hci0"),
            HashMap::from([(
                ADAPTER.to_owned(),
                properties(vec![
                    ("Alias", owned("laptop")),
                    ("Powered", owned(true)),
                    ("PowerState", owned("on")),
                ]),
            )]),
        );
        let device = |alias: &str, paired: bool, connected: bool| {
            properties(vec![
                (
                    "Adapter",
                    owned(zbus::zvariant::ObjectPath::try_from("/org/bluez/hci0").unwrap()),
                ),
                ("Address", owned("AA:BB:CC:DD:EE:FF")),
                ("Alias", owned(alias.to_owned())),
                ("Icon", owned("audio-headphones")),
                ("Paired", owned(paired)),
                ("Trusted", owned(paired)),
                ("Connected", owned(connected)),
            ])
        };
        objects.insert(
            path("/org/bluez/hci0/dev_AA"),
            HashMap::from([
                (DEVICE.to_owned(), device("Zed Headset", true, true)),
                (
                    BATTERY.to_owned(),
                    properties(vec![("Percentage", owned(80u8))]),
                ),
            ]),
        );
        objects.insert(
            path("/org/bluez/hci0/dev_BB"),
            HashMap::from([(DEVICE.to_owned(), device("a mouse", true, false))]),
        );
        objects.insert(
            path("/org/bluez/hci0/dev_CC"),
            HashMap::from([(DEVICE.to_owned(), device("", false, false))]),
        );
        objects
    }

    #[test]
    fn no_adapter_is_an_empty_snapshot() {
        assert_eq!(parse(&Objects::new()), Snapshot::default());
    }

    #[test]
    fn the_adapter_and_its_devices_are_read() {
        let snapshot = parse(&laptop());
        let adapter = snapshot.adapter.clone().unwrap();
        assert!(adapter.powered && !adapter.blocked());
        let names: Vec<&str> = snapshot.devices.iter().map(|d| d.name.as_str()).collect();
        // Connected first, then by name ignoring case; a nameless device goes by its address.
        assert_eq!(names, ["Zed Headset", "a mouse", "AA:BB:CC:DD:EE:FF"]);
        assert_eq!(snapshot.devices[0].battery, Some(80));
        assert_eq!(snapshot.paired().count(), 2);
        assert_eq!(snapshot.connected().count(), 1);
    }

    #[test]
    fn a_blocked_adapter_says_so() {
        assert!(adapter(false, "off-blocked").blocked());
        assert!(!adapter(false, "off").blocked());
    }
}
