// SPDX-License-Identifier: GPL-3.0-or-later
//! A stand-in bluetoothd, for trying `wlrix-bluetooth`, its pairing prompt and Bluetooth
//! Settings on a machine with no adapter -- or without pairing real headphones a dozen times.
//!
//! It takes `org.bluez` on whatever bus `DBUS_SYSTEM_BUS_ADDRESS` names, which is never the real
//! system bus: start a private one and point everything at it.
//!
//! ```text
//! dbus-daemon --session --print-address --fork      # the address it prints is the fake system bus
//! DBUS_SYSTEM_BUS_ADDRESS=<address> cargo run --example fake_bluez
//! DBUS_SYSTEM_BUS_ADDRESS=<address> wlrix-bluetooth
//! ```
//!
//! It serves one adapter, `hci0`, with a paired headset (with a battery), a paired mouse, and a
//! speaker that is out of range -- connecting to it fails as a real one would. `Pair` on a device
//! asks the registered agent for a confirmation, as numeric comparison does. Lines on standard
//! input change the rest:
//!
//! ```text
//! power on|off            the adapter's Powered
//! block | unblock         PowerState "off-blocked", as rfkill would
//! battery <percent>       the headset's battery
//! found <name>            a new, unpaired device appears, as a scan would find it
//! confirm|display|pin <name>   the agent is asked, about that device
//! cancel                  the agent is told to stop asking
//! ```

use std::collections::HashMap;
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

const ADAPTER_PATH: &str = "/org/bluez/hci0";

#[derive(Clone)]
struct Fake {
    powered: bool,
    blocked: bool,
    discoverable: bool,
    discovering: bool,
    /// The agent bluetoothd would call: its unique name and path.
    agent: Option<(String, OwnedObjectPath)>,
}

#[derive(Clone)]
struct DeviceState {
    name: String,
    icon: String,
    paired: bool,
    trusted: bool,
    connected: bool,
    reachable: bool,
}

type Shared = Arc<Mutex<Fake>>;
type Devices = Arc<Mutex<HashMap<String, DeviceState>>>;
type Bus = Arc<Mutex<Option<zbus::blocking::Connection>>>;

fn changed(bus: &Bus, path: &str, interface: &str) {
    let Some(connection) = bus.lock().unwrap().clone() else {
        return;
    };
    let none: HashMap<&str, Value> = HashMap::new();
    let invalidated: Vec<&str> = Vec::new();
    let _ = connection.emit_signal(
        None::<&str>,
        path,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &(interface, none, invalidated),
    );
}

fn device_path(name: &str) -> String {
    let address: String = name.bytes().take(6).map(|b| format!("{b:02X}")).collect();
    format!("{ADAPTER_PATH}/dev_{}", address)
}

struct AgentManager(Shared);

#[zbus::interface(name = "org.bluez.AgentManager1")]
impl AgentManager {
    fn register_agent(
        &self,
        agent: OwnedObjectPath,
        capability: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        eprintln!("fake_bluez: RegisterAgent({agent}, {capability:?}) from {sender}");
        self.0.lock().unwrap().agent = Some((sender, agent));
    }

    fn request_default_agent(&self, agent: OwnedObjectPath) {
        eprintln!("fake_bluez: RequestDefaultAgent({agent})");
    }

    fn unregister_agent(&self, agent: OwnedObjectPath) {
        eprintln!("fake_bluez: UnregisterAgent({agent})");
        self.0.lock().unwrap().agent = None;
    }
}

struct Adapter {
    fake: Shared,
    devices: Devices,
    bus: Bus,
}

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn address(&self) -> String {
        "00:1A:7D:DA:71:13".into()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        "fake-laptop".into()
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        "fake-laptop".into()
    }
    #[zbus(property)]
    fn powered(&self) -> bool {
        self.fake.lock().unwrap().powered
    }
    #[zbus(property)]
    fn set_powered(&mut self, on: bool) -> zbus::fdo::Result<()> {
        let mut fake = self.fake.lock().unwrap();
        if on && fake.blocked {
            return Err(zbus::fdo::Error::Failed("Blocked through rfkill".into()));
        }
        eprintln!("fake_bluez: Powered = {on}");
        fake.powered = on;
        if !on {
            for device in self.devices.lock().unwrap().values_mut() {
                device.connected = false;
            }
        }
        Ok(())
    }
    #[zbus(property)]
    fn power_state(&self) -> String {
        let fake = self.fake.lock().unwrap();
        if fake.blocked {
            "off-blocked".into()
        } else if fake.powered {
            "on".into()
        } else {
            "off".into()
        }
    }
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        self.fake.lock().unwrap().discoverable
    }
    #[zbus(property)]
    fn set_discoverable(&mut self, on: bool) {
        eprintln!("fake_bluez: Discoverable = {on}");
        self.fake.lock().unwrap().discoverable = on;
    }
    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.fake.lock().unwrap().discovering
    }

    fn start_discovery(&self) {
        eprintln!("fake_bluez: StartDiscovery");
        self.fake.lock().unwrap().discovering = true;
        changed(&self.bus, ADAPTER_PATH, "org.bluez.Adapter1");
    }

    fn stop_discovery(&self) {
        eprintln!("fake_bluez: StopDiscovery");
        self.fake.lock().unwrap().discovering = false;
        changed(&self.bus, ADAPTER_PATH, "org.bluez.Adapter1");
    }

    fn remove_device(&self, device: OwnedObjectPath) {
        eprintln!("fake_bluez: RemoveDevice({device})");
        self.devices.lock().unwrap().remove(device.as_str());
        let bus = self.bus.clone();
        let path = device.to_string();
        std::thread::spawn(move || {
            if let Some(connection) = bus.lock().unwrap().clone() {
                let server = connection.object_server();
                let _ = server.remove::<Device, _>(path.as_str());
                let _ = server.remove::<Battery, _>(path.as_str());
            }
        });
    }
}

struct Device {
    path: String,
    devices: Devices,
    fake: Shared,
    bus: Bus,
}

impl Device {
    fn state(&self) -> DeviceState {
        self.devices.lock().unwrap()[&self.path].clone()
    }
    fn edit(&self, edit: impl FnOnce(&mut DeviceState)) {
        if let Some(device) = self.devices.lock().unwrap().get_mut(&self.path) {
            edit(device);
        }
        changed(&self.bus, &self.path, "org.bluez.Device1");
    }
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn address(&self) -> String {
        self.path
            .rsplit("dev_")
            .next()
            .unwrap_or_default()
            .to_owned()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.state().name
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        self.state().name
    }
    #[zbus(property)]
    fn icon(&self) -> String {
        self.state().icon
    }
    #[zbus(property)]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(ADAPTER_PATH).unwrap()
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.state().paired
    }
    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.state().trusted
    }
    #[zbus(property)]
    fn set_trusted(&mut self, on: bool) {
        eprintln!("fake_bluez: {} Trusted = {on}", self.path);
        self.edit(|device| device.trusted = on);
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.state().connected
    }

    async fn connect(&self) -> zbus::fdo::Result<()> {
        eprintln!("fake_bluez: Connect({})", self.path);
        // A real one takes a second or two -- waited out on a thread, not on zbus's executor,
        // which has every other call on this connection to answer meanwhile.
        let (done, wait) = async_channel::bounded(1);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = done.send_blocking(());
        });
        let _ = wait.recv().await;
        if !self.fake.lock().unwrap().powered {
            return Err(zbus::fdo::Error::Failed("Not Ready".into()));
        }
        if !self.state().reachable {
            return Err(zbus::fdo::Error::Failed("Page Timeout".into()));
        }
        self.edit(|device| device.connected = true);
        Ok(())
    }

    fn disconnect(&self) {
        eprintln!("fake_bluez: Disconnect({})", self.path);
        self.edit(|device| device.connected = false);
    }

    async fn pair(&self) -> zbus::fdo::Result<()> {
        eprintln!("fake_bluez: Pair({})", self.path);
        // The agent's answer comes back on this same connection, so the blocking call to it is
        // made on a thread: on the executor it would wait on itself.
        let (done, wait) = async_channel::bounded(1);
        let (fake, bus, path) = (self.fake.clone(), self.bus.clone(), self.path.clone());
        std::thread::spawn(move || {
            let _ = done.send_blocking(ask(&fake, &bus, "RequestConfirmation", &path));
        });
        match wait
            .recv()
            .await
            .unwrap_or_else(|_| Err("no answer".into()))
        {
            Ok(()) => {
                self.edit(|device| device.paired = true);
                Ok(())
            }
            Err(err) => Err(zbus::fdo::Error::AuthFailed(format!(
                "Authentication Failed: {err}"
            ))),
        }
    }

    fn cancel_pairing(&self) {
        eprintln!("fake_bluez: CancelPairing({})", self.path);
    }
}

struct Battery {
    percent: Arc<Mutex<u8>>,
}

#[zbus::interface(name = "org.bluez.Battery1")]
impl Battery {
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        *self.percent.lock().unwrap()
    }
}

/// Call the registered agent, as bluetoothd would, from the connection that owns `org.bluez`.
fn ask(fake: &Shared, bus: &Bus, method: &str, device: &str) -> Result<(), String> {
    let Some((agent, path)) = fake.lock().unwrap().agent.clone() else {
        return Err("no agent registered".into());
    };
    let Some(connection) = bus.lock().unwrap().clone() else {
        return Err("no bus".into());
    };
    let device = ObjectPath::try_from(device).map_err(|err| err.to_string())?;
    eprintln!("fake_bluez: asking the agent: {method}({device})");
    let result = match method {
        "RequestConfirmation" => connection.call_method(
            Some(agent.as_str()),
            path.as_str(),
            Some("org.bluez.Agent1"),
            method,
            &(&device, 123456u32),
        ),
        "DisplayPasskey" => connection.call_method(
            Some(agent.as_str()),
            path.as_str(),
            Some("org.bluez.Agent1"),
            method,
            &(&device, 4321u32, 0u16),
        ),
        "RequestPinCode" => connection
            .call_method(
                Some(agent.as_str()),
                path.as_str(),
                Some("org.bluez.Agent1"),
                method,
                &(&device,),
            )
            .inspect(|reply| {
                let pin: String = reply.body().deserialize().unwrap_or_default();
                eprintln!("fake_bluez: the agent answered PIN {pin:?}");
            }),
        _ => return Err(format!("unknown agent method {method}")),
    };
    match result {
        Ok(_) => {
            eprintln!("fake_bluez: the agent said yes to {method}");
            Ok(())
        }
        Err(err) => {
            eprintln!("fake_bluez: the agent said no to {method}: {err}");
            Err(err.to_string())
        }
    }
}

fn main() -> zbus::Result<()> {
    let fake: Shared = Arc::new(Mutex::new(Fake {
        powered: true,
        blocked: false,
        discoverable: false,
        discovering: false,
        agent: None,
    }));
    let devices: Devices = Arc::new(Mutex::new(HashMap::new()));
    let bus: Bus = Arc::new(Mutex::new(None));
    let battery = Arc::new(Mutex::new(80u8));

    let connection = zbus::blocking::connection::Builder::system()?
        .method_timeout(Duration::from_secs(300))
        .name("org.bluez")?
        .serve_at("/", zbus::fdo::ObjectManager)?
        .serve_at("/org/bluez", AgentManager(fake.clone()))?
        .serve_at(
            ADAPTER_PATH,
            Adapter {
                fake: fake.clone(),
                devices: devices.clone(),
                bus: bus.clone(),
            },
        )?
        .build()?;
    *bus.lock().unwrap() = Some(connection.clone());

    let add = |name: &str, icon: &str, paired: bool, connected: bool, reachable: bool| {
        let path = device_path(name);
        devices.lock().unwrap().insert(
            path.clone(),
            DeviceState {
                name: name.into(),
                icon: icon.into(),
                paired,
                trusted: paired,
                connected,
                reachable,
            },
        );
        let _ = connection.object_server().at(
            path.as_str(),
            Device {
                path: path.clone(),
                devices: devices.clone(),
                fake: fake.clone(),
                bus: bus.clone(),
            },
        );
        path
    };
    let headset = add("WH-1000XM4", "audio-headphones", true, true, true);
    connection.object_server().at(
        headset.as_str(),
        Battery {
            percent: battery.clone(),
        },
    )?;
    add("MX Master 3", "input-mouse", true, false, true);
    add("Kitchen Speaker", "audio-speakers", true, false, false);
    eprintln!("fake_bluez: ready");

    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|err| zbus::Error::Failure(err.to_string()))?;
        let (word, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match word {
            "power" => {
                fake.lock().unwrap().powered = rest == "on";
                changed(&bus, ADAPTER_PATH, "org.bluez.Adapter1");
            }
            "block" | "unblock" => {
                let mut state = fake.lock().unwrap();
                state.blocked = word == "block";
                if state.blocked {
                    state.powered = false;
                }
                drop(state);
                changed(&bus, ADAPTER_PATH, "org.bluez.Adapter1");
            }
            "battery" => {
                *battery.lock().unwrap() = rest.parse().unwrap_or(50);
                changed(&bus, &headset, "org.bluez.Battery1");
            }
            "found" => {
                add(rest, "phone", false, false, true);
            }
            "confirm" | "display" | "pin" => {
                let method = match word {
                    "confirm" => "RequestConfirmation",
                    "display" => "DisplayPasskey",
                    _ => "RequestPinCode",
                };
                let path = device_path(rest);
                let (fake, bus) = (fake.clone(), bus.clone());
                std::thread::spawn(move || {
                    let _ = ask(&fake, &bus, method, &path);
                });
            }
            "cancel" => {
                let agent = fake.lock().unwrap().agent.clone();
                if let Some((name, path)) = agent {
                    let _ = connection.call_method(
                        Some(name.as_str()),
                        path.as_str(),
                        Some("org.bluez.Agent1"),
                        "Cancel",
                        &(),
                    );
                    eprintln!("fake_bluez: told the agent to cancel");
                }
            }
            "" => continue,
            _ => eprintln!("fake_bluez: did not understand {line:?}"),
        }
    }
    Ok(())
}
