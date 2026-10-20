// SPDX-License-Identifier: GPL-3.0-or-later
//! BlueZ's pairing agent: who bluetoothd asks when a device being paired wants a person.
//!
//! Pairing a headset shows the same six digits on both ends and asks whether they match;
//! pairing a keyboard asks for the digits to be typed on it; an old device wants a PIN. bluetoothd
//! does none of that itself. It calls the *default agent* -- registered here, through
//! `org.bluez.AgentManager1`, with the capability `KeyboardDisplay` that covers all of them -- and
//! with no agent most pairings fail outright.
//!
//! This side only receives. Each call becomes an [`AgentEvent`] for the main loop, which puts up
//! the prompt and answers through the one-shot channel the event carries; the methods are `async`
//! so a person can take their time without holding the system bus's dispatch thread. The pairing
//! is started from Bluetooth Settings, which is a different process: it calls `Pair`, and the
//! questions come here.
//!
//! # Only bluetoothd may ask
//!
//! The agent object is on the system bus, where any process on the machine can call it. A
//! `RequestConfirmation` from anything but bluetoothd would be a convincing pairing dialog on this
//! user's screen, so every call is checked against the current owner of `org.bluez` and refused
//! otherwise -- as `wlrix-network`'s agent checks NetworkManager.

use zbus::message::Header;
use zbus::zvariant::OwnedObjectPath;

use crate::bluez::BLUEZ;

/// Where the agent is served.
pub const AGENT_PATH: &str = "/org/wlrix/bluetooth/agent";

/// The errors bluetoothd distinguishes. `Rejected` is "no"; `Canceled` is "nobody answered".
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// What a question was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Yes, for a confirmation or an authorization.
    Yes,
    Pin(String),
    Passkey(u32),
}

/// What a question is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    Confirm(u32),
    Pin,
    Passkey,
    Authorize(Option<String>),
}

pub type Answer = async_channel::Sender<Result<Reply, AgentError>>;

/// What the agent hands to the main loop.
#[derive(Debug)]
pub enum AgentEvent {
    /// A question for the person about the device at this path.
    Ask(String, Question, Answer),
    /// Show this code until the pairing is over; nothing to answer.
    Display(String, String),
    /// bluetoothd gave up on whatever it asked: close every prompt.
    Cancel,
}

pub struct Agent {
    pub events: calloop::channel::Sender<AgentEvent>,
}

/// Refuse a call that did not come from bluetoothd. See the module comment.
async fn from_bluez(connection: &zbus::Connection, header: &Header<'_>) -> Result<(), AgentError> {
    let sender = header
        .sender()
        .ok_or_else(|| AgentError::Rejected("a call with no sender".into()))?;
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let owner = bus
        .get_name_owner(BLUEZ.try_into().map_err(zbus::Error::from)?)
        .await
        .map_err(|err| AgentError::Rejected(format!("bluetoothd is not running: {err}")))?;
    if owner.as_str() != sender.as_str() {
        eprintln!("wlrix-bluetooth: refused an agent call from {sender}, which is not bluetoothd");
        return Err(AgentError::Rejected("only bluetoothd may ask".into()));
    }
    Ok(())
}

impl Agent {
    async fn ask(&self, device: OwnedObjectPath, question: Question) -> Result<Reply, AgentError> {
        let (reply, answer) = async_channel::bounded(1);
        self.events
            .send(AgentEvent::Ask(device.to_string(), question, reply))
            .map_err(|_| AgentError::Canceled("the daemon is stopping".into()))?;
        answer
            .recv()
            .await
            .unwrap_or_else(|_| Err(AgentError::Canceled("no answer".into())))
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    /// bluetoothd let go of the agent -- it is stopping, or another took over.
    async fn release(&self) {}

    async fn request_pin_code(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<String, AgentError> {
        from_bluez(bus, &header).await?;
        match self.ask(device, Question::Pin).await? {
            Reply::Pin(pin) => Ok(pin),
            _ => Err(AgentError::Rejected("no PIN".into())),
        }
    }

    async fn display_pin_code(
        &self,
        device: OwnedObjectPath,
        pincode: String,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        let _ = self
            .events
            .send(AgentEvent::Display(device.to_string(), pincode));
        Ok(())
    }

    async fn request_passkey(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<u32, AgentError> {
        from_bluez(bus, &header).await?;
        match self.ask(device, Question::Passkey).await? {
            Reply::Passkey(passkey) => Ok(passkey),
            _ => Err(AgentError::Rejected("no passkey".into())),
        }
    }

    /// Sent again as each digit is typed on the device; the dialog is put up the first time only.
    async fn display_passkey(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        _entered: u16,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        let _ = self.events.send(AgentEvent::Display(
            device.to_string(),
            crate::prompt::six_digits(passkey),
        ));
        Ok(())
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        self.ask(device, Question::Confirm(passkey))
            .await
            .map(|_| ())
    }

    async fn request_authorization(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        self.ask(device, Question::Authorize(None))
            .await
            .map(|_| ())
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        self.ask(device, Question::Authorize(service_name(&uuid)))
            .await
            .map(|_| ())
    }

    async fn cancel(
        &self,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_bluez(bus, &header).await?;
        let _ = self.events.send(AgentEvent::Cancel);
        Ok(())
    }
}

/// A service's name as a person would say it, for the few a device asks to use; `None` for the
/// rest, and the dialog asks about the device alone.
pub fn service_name(uuid: &str) -> Option<String> {
    // The 16-bit assigned numbers, in their 128-bit base-UUID form.
    let short = uuid
        .strip_prefix("0000")
        .and_then(|rest| rest.strip_suffix("-0000-1000-8000-00805f9b34fb"))?;
    let name = match short.to_ascii_lowercase().as_str() {
        "110a" | "110b" | "110d" => "audio",
        "110c" | "110e" | "110f" => "media controls",
        "1108" | "1112" | "111e" | "111f" => "calls",
        "1124" => "input",
        "1105" | "1106" => "file transfer",
        "1115" | "1116" => "network",
        "112f" | "1132" | "1134" => "contacts and messages",
        _ => return None,
    };
    Some(name.to_owned())
}

/// Register as the default agent. Again after bluetoothd restarts, which forgets every agent.
pub fn register(connection: &zbus::blocking::Connection) -> zbus::Result<()> {
    let path = zbus::zvariant::ObjectPath::try_from(AGENT_PATH)?;
    connection.call_method(
        Some(BLUEZ),
        "/org/bluez",
        Some("org.bluez.AgentManager1"),
        "RegisterAgent",
        &(&path, "KeyboardDisplay"),
    )?;
    connection.call_method(
        Some(BLUEZ),
        "/org/bluez",
        Some("org.bluez.AgentManager1"),
        "RequestDefaultAgent",
        &(&path,),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_a_device_asks_for_have_plain_names() {
        assert_eq!(
            service_name("0000110b-0000-1000-8000-00805f9b34fb").as_deref(),
            Some("audio")
        );
        assert_eq!(
            service_name("0000111F-0000-1000-8000-00805f9b34fb").as_deref(),
            Some("calls")
        );
        assert_eq!(service_name("0000abcd-0000-1000-8000-00805f9b34fb"), None);
        assert_eq!(service_name("not a uuid"), None);
    }
}
