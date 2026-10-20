// SPDX-License-Identifier: GPL-3.0-or-later
//! The main loop.
//!
//! Sources on one calloop loop: "BlueZ said something" from [`crate::bluez::watch`], "a tray
//! appeared" from [`crate::sni::watch_for_tray`], what was chosen in the menu from
//! [`crate::dbusmenu`], questions from the pairing [`crate::agent`], the prompt's answers, the
//! outcome of each connection attempt, and the reload and quit pings from [`crate::signals`]. The
//! loop owns the last snapshot and the config, is the only thing that changes what the item and its
//! menu show, and is where BlueZ is asked to do things.
//!
//! # Letting a burst settle
//!
//! A device connecting changes `Connected`, then `ServicesResolved`, then gains a battery and a
//! media transport, each its own signal. As in `wlrix-network`, a signal only arms a short timer
//! and the whole state is read when it fires.
//!
//! # Notifications
//!
//! Only a connection that failed, and a device's battery running low -- the person's decision:
//! ordinary connects are what the icon and the menu are for.

use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::time::Duration;

use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};

use crate::agent::{self, AgentError, AgentEvent, Answer, Question, Reply};
use crate::bluez::{self, Snapshot};
use crate::config::Config;
use crate::dbusmenu::MenuEvent;
use crate::indicator::{self, Extra, Status, View};
use crate::menu::{self, Action, Ids};
use crate::prompt::{self, Request};
use crate::rfkill;
use crate::sni::{self, Published};

/// How long after the first of a burst of BlueZ signals the state is read.
const SETTLE: Duration = Duration::from_millis(250);

/// The Bluetooth Settings panel, from `wlrix-apps`.
const SETTINGS_PROGRAM: &str = "wlrix-settings-bluetooth";

/// How long any call this daemon makes on its own loop may take -- the same as `wlrix-network`'s.
/// Connecting is not one of them: it has its own thread and a longer limit (see [`bluez`]).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A prompt on screen.
struct Pending {
    device: String,
    child: Child,
    /// The question it is answering, and where the answer goes; `None` for a code on display.
    asked: Option<(Question, Answer)>,
}

struct Daemon {
    config: Config,
    system: zbus::blocking::Connection,
    published: Published,
    snapshot: Option<Snapshot>,
    settling: bool,
    logged: Option<(String, Status)>,
    ids: Ids,
    /// Devices this daemon is connecting.
    connecting: HashSet<String>,
    /// Where each connection attempt reports.
    connected: calloop::channel::Sender<(String, Option<String>)>,
    /// The battery level last warned about, per device.
    battery_warned: HashMap<String, Option<u8>>,
    prompts: HashMap<u64, Pending>,
    next_token: u64,
    answers: calloop::channel::Sender<(u64, Option<prompt::Answer>)>,
    exit: bool,
}

impl Daemon {
    fn refresh(&mut self) {
        let was_running = self.snapshot.is_some();
        self.snapshot = match bluez::read(&self.system) {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                if self.snapshot.is_some() {
                    eprintln!("wlrix-bluetooth: bluetoothd is not answering: {err}");
                }
                None
            }
        };
        // bluetoothd restarted, and forgot every agent with it.
        if !was_running && self.snapshot.is_some() {
            self.register_agent();
        }
        self.close_finished_displays();
        self.show();
        self.warn_batteries();
    }

    fn register_agent(&self) {
        match agent::register(&self.system) {
            Ok(()) => eprintln!("wlrix-bluetooth: registered as BlueZ's pairing agent"),
            Err(err) => {
                eprintln!("wlrix-bluetooth: could not register as the pairing agent: {err}")
            }
        }
    }

    fn hard_blocked(&self) -> bool {
        rfkill::hard_blocked()
    }

    fn show(&mut self) {
        let view = self.view();
        self.log(&view);
        self.published.show(view);
        let hard = self.hard_blocked();
        let root = menu::build(
            self.snapshot.as_ref(),
            &self.connecting,
            hard,
            &mut self.ids,
        );
        self.published.set_menu(root);
    }

    fn view(&self) -> View {
        indicator::view(
            self.snapshot.as_ref(),
            Extra {
                hard_blocked: self.hard_blocked(),
                connecting: !self.connecting.is_empty(),
            },
            self.config.indicator,
        )
    }

    fn log(&mut self, view: &View) {
        let now = (view.title.clone(), view.status);
        if self.logged.as_ref() != Some(&now) {
            eprintln!("wlrix-bluetooth: {} ({})", view.title, view.status.as_str());
            self.logged = Some(now);
        }
    }

    /// Warn once per level as a connected device's battery runs down.
    fn warn_batteries(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        for device in snapshot.connected() {
            let Some(percent) = device.battery else {
                continue;
            };
            let before = self.battery_warned.get(&device.path).copied().flatten();
            let (level, warn) = indicator::battery_level(before, percent);
            self.battery_warned.insert(device.path.clone(), level);
            if warn && self.config.battery_warnings() {
                self.notify(
                    "battery-caution-symbolic",
                    &format!("{} battery low", device.name),
                    &format!("{percent}% remaining."),
                );
            }
        }
    }

    fn on_menu(&mut self, event: MenuEvent) {
        let MenuEvent::Chosen(action) = event else {
            return;
        };
        match action {
            Action::Power(on) => self.power(on),
            Action::Device(path, true) => {
                if self.connecting.insert(path.clone()) {
                    bluez::connect(path, self.connected.clone());
                    self.show();
                }
            }
            Action::Device(path, false) => {
                if let Err(err) = bluez::disconnect(&self.system, &path) {
                    eprintln!("wlrix-bluetooth: disconnecting {path} failed: {err}");
                    self.notify(
                        "bluetooth-symbolic",
                        "Bluetooth change failed",
                        &bluez::refusal(&err),
                    );
                }
            }
            Action::OpenSettings => open_settings(self),
        }
    }

    /// Switch the adapter on or off; on lifts a soft rfkill block first, which `Powered` alone
    /// cannot.
    fn power(&mut self, on: bool) {
        let Some(adapter) = self.snapshot.as_ref().and_then(|s| s.adapter.clone()) else {
            return;
        };
        // bluetoothd notices an unblock and powers the adapter itself when its AutoEnable is set;
        // if not, the Powered below does it, and if it is too quick for bluetoothd the next read
        // still shows it off and the toggle is there to press again.
        if on
            && adapter.blocked()
            && let Err(err) = rfkill::unblock()
        {
            eprintln!("wlrix-bluetooth: could not unblock rfkill: {err}");
            self.notify(
                "bluetooth-disabled-symbolic",
                "Bluetooth could not be turned on",
                &format!("It is blocked by rfkill, and lifting that failed: {err}."),
            );
            return;
        }
        if let Err(err) = bluez::set_powered(&self.system, &adapter.path, on) {
            // Straight after an unblock bluetoothd may not have caught up yet; that is not worth a
            // notification when the adapter comes up anyway.
            if !(on && adapter.blocked()) {
                eprintln!("wlrix-bluetooth: switching Bluetooth failed: {err}");
                self.notify(
                    "bluetooth-symbolic",
                    "Bluetooth change failed",
                    &bluez::refusal(&err),
                );
            }
        }
    }

    /// A connection attempt finished.
    fn on_connected(&mut self, path: String, failure: Option<String>) {
        self.connecting.remove(&path);
        if let Some(why) = failure {
            let name = self
                .snapshot
                .as_ref()
                .and_then(|s| s.device(&path))
                .map_or_else(|| path.clone(), |device| device.name.clone());
            eprintln!("wlrix-bluetooth: connecting {name} failed: {why}");
            self.notify(
                "bluetooth-disconnected-symbolic",
                &format!("Could not connect to {name}"),
                &why,
            );
        }
        self.show();
    }

    /// The name to put in a prompt: the device's alias, or its address from the path.
    fn device_name(&self, path: &str) -> String {
        if let Some(device) = self.snapshot.as_ref().and_then(|s| s.device(path)) {
            return device.name.clone();
        }
        // A device seen for the first time a moment ago may not be in the last read.
        if let Ok(snapshot) = bluez::read(&self.system)
            && let Some(device) = snapshot.device(path)
        {
            return device.name.clone();
        }
        path.rsplit('/')
            .next()
            .and_then(|last| last.strip_prefix("dev_"))
            .map_or_else(|| path.to_owned(), |address| address.replace('_', ":"))
    }

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Ask(device, question, reply) => {
                let name = self.device_name(&device);
                let request = match &question {
                    Question::Confirm(passkey) => Request::Confirm {
                        device: name,
                        passkey: prompt::six_digits(*passkey),
                    },
                    Question::Pin => Request::Pin { device: name },
                    Question::Passkey => Request::Passkey { device: name },
                    Question::Authorize(service) => Request::Authorize {
                        device: name,
                        service: service.clone(),
                    },
                };
                match self.spawn(&request) {
                    Some((token, child)) => {
                        self.prompts.insert(
                            token,
                            Pending {
                                device,
                                child,
                                asked: Some((question, reply)),
                            },
                        );
                    }
                    None => {
                        let _ = reply.try_send(Err(AgentError::Rejected(
                            "the prompt would not start".into(),
                        )));
                    }
                }
            }
            AgentEvent::Display(device, code) => {
                // Sent again with every digit typed; one dialog is enough.
                if self
                    .prompts
                    .values()
                    .any(|p| p.device == device && p.asked.is_none())
                {
                    return;
                }
                let request = Request::Display {
                    device: self.device_name(&device),
                    code,
                };
                if let Some((token, child)) = self.spawn(&request) {
                    self.prompts.insert(
                        token,
                        Pending {
                            device,
                            child,
                            asked: None,
                        },
                    );
                }
            }
            AgentEvent::Cancel => {
                eprintln!(
                    "wlrix-bluetooth: bluetoothd cancelled; closing {} prompt(s)",
                    self.prompts.len()
                );
                for (_, mut pending) in self.prompts.drain() {
                    let _ = pending.child.kill();
                    let _ = pending.child.wait();
                    if let Some((_, reply)) = pending.asked {
                        let _ = reply.try_send(Err(AgentError::Canceled(
                            "bluetoothd stopped asking".into(),
                        )));
                    }
                }
            }
        }
    }

    /// A code on display is no longer needed once its device has paired.
    fn close_finished_displays(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let done: Vec<u64> = self
            .prompts
            .iter()
            .filter(|(_, pending)| pending.asked.is_none())
            .filter(|(_, pending)| snapshot.device(&pending.device).is_none_or(|d| d.paired))
            .map(|(token, _)| *token)
            .collect();
        for token in done {
            if let Some(mut pending) = self.prompts.remove(&token) {
                let _ = pending.child.kill();
                let _ = pending.child.wait();
            }
        }
    }

    fn spawn(&mut self, request: &Request) -> Option<(u64, Child)> {
        self.next_token += 1;
        let token = self.next_token;
        match prompt::spawn(request, token, self.answers.clone()) {
            Ok(child) => Some((token, child)),
            Err(err) => {
                eprintln!("wlrix-bluetooth: {err}");
                self.notify(
                    "bluetooth-symbolic",
                    "The pairing prompt would not start",
                    &format!("{err}. Is wlrix-bluetooth-prompt installed?"),
                );
                None
            }
        }
    }

    /// A prompt was answered, or closed.
    fn on_answer(&mut self, token: u64, answer: Option<prompt::Answer>) {
        let Some(mut pending) = self.prompts.remove(&token) else {
            return;
        };
        // Closed already, since its stdout has been read to the end; this only reaps it.
        let _ = pending.child.wait();
        let Some((question, reply)) = pending.asked else {
            return;
        };
        let answer = answer.unwrap_or_default();
        let result = match question {
            Question::Confirm(_) | Question::Authorize(_) if answer.accept == Some(true) => {
                Ok(Reply::Yes)
            }
            Question::Pin => match answer.pin {
                Some(pin) if !pin.is_empty() => Ok(Reply::Pin(pin)),
                _ => Err(AgentError::Canceled("no PIN".into())),
            },
            Question::Passkey => match answer.passkey {
                Some(passkey) => Ok(Reply::Passkey(passkey)),
                None => Err(AgentError::Canceled("no passkey".into())),
            },
            _ => Err(AgentError::Rejected("declined".into())),
        };
        let _ = reply.try_send(result);
    }

    /// Post a notification. Not delivered is not reported beyond the log.
    fn notify(&self, icon: &str, summary: &str, body: &str) {
        let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        let actions: Vec<&str> = Vec::new();
        if let Err(err) = self.published.connection().call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Bluetooth",
                0u32,
                icon,
                summary,
                body,
                actions,
                hints,
                -1i32,
            ),
        ) {
            eprintln!("wlrix-bluetooth: could not post a notification ({err}): {summary}");
        }
    }
}

fn open_settings(daemon: &Daemon) {
    // Started and left, its child reaped on a thread so it never lingers as a zombie.
    match std::process::Command::new(SETTINGS_PROGRAM).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(err) => {
            eprintln!("wlrix-bluetooth: could not start {SETTINGS_PROGRAM}: {err}");
            daemon.notify(
                "bluetooth-symbolic",
                "Bluetooth Settings would not start",
                &format!("{err}. Is {SETTINGS_PROGRAM} installed?"),
            );
        }
    }
}

fn system_bus() -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::system()?
        .method_timeout(CALL_TIMEOUT)
        .build()
}

/// Run until stopped.
pub fn run() -> Result<(), String> {
    let config = Config::load();
    let system = system_bus().map_err(|err| format!("no system bus, so no BlueZ to ask: {err}"))?;

    let snapshot = bluez::read(&system).ok();
    if snapshot.is_none() {
        // Not fatal: on a desktop there is simply no bluetoothd, and on a laptop it may be
        // starting; its name appearing is one of the signals watched for.
        eprintln!("wlrix-bluetooth: bluetoothd is not on the system bus");
    }
    let hard = rfkill::hard_blocked();
    let first = indicator::view(
        snapshot.as_ref(),
        Extra {
            hard_blocked: hard,
            connecting: false,
        },
        config.indicator,
    );
    let mut ids = Ids::default();
    let root = menu::build(snapshot.as_ref(), &HashSet::new(), hard, &mut ids);
    let (menu_events, menu_channel) = calloop::channel::channel();
    let published = sni::publish(first.clone(), root, menu_events)?;

    let mut event_loop: EventLoop<Daemon> =
        EventLoop::try_new().map_err(|err| format!("could not create the event loop: {err}"))?;
    let handle = event_loop.handle();

    // The pairing agent, on the system bus beside the BlueZ reads.
    let (agent_events, agent_channel) = calloop::channel::channel();
    system
        .object_server()
        .at(
            agent::AGENT_PATH,
            agent::Agent {
                events: agent_events,
            },
        )
        .map_err(|err| format!("could not serve the pairing agent: {err}"))?;
    handle
        .insert_source(agent_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(event) = event {
                daemon.on_agent(event);
            }
        })
        .map_err(|err| format!("could not listen to the pairing agent: {err}"))?;

    let (answers, answer_channel) = calloop::channel::channel();
    handle
        .insert_source(answer_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg((token, answer)) = event {
                daemon.on_answer(token, answer);
            }
        })
        .map_err(|err| format!("could not listen for answers: {err}"))?;

    let (connected, connected_channel) = calloop::channel::channel();
    handle
        .insert_source(connected_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg((path, failure)) = event {
                daemon.on_connected(path, failure);
            }
        })
        .map_err(|err| format!("could not listen for connections: {err}"))?;

    let (changed, changes) = calloop::channel::channel();
    bluez::watch(system.clone(), changed)?;
    let settle_handle = handle.clone();
    handle
        .insert_source(changes, move |event, _, daemon: &mut Daemon| {
            if !matches!(event, calloop::channel::Event::Msg(())) || daemon.settling {
                return;
            }
            daemon.settling = true;
            let armed = settle_handle.insert_source(
                Timer::from_duration(SETTLE),
                |_, _, daemon: &mut Daemon| {
                    daemon.settling = false;
                    daemon.refresh();
                    TimeoutAction::Drop
                },
            );
            if let Err(err) = armed {
                eprintln!("wlrix-bluetooth: could not arm the settle timer: {err}");
                daemon.settling = false;
                daemon.refresh();
            }
        })
        .map_err(|err| format!("could not listen to BlueZ: {err}"))?;

    handle
        .insert_source(menu_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(event) = event {
                daemon.on_menu(event);
            }
        })
        .map_err(|err| format!("could not listen to the menu: {err}"))?;

    let (appeared, appearances) = calloop::channel::channel();
    sni::watch_for_tray(&published, appeared)?;
    handle
        .insert_source(appearances, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(()) = event {
                daemon.published.register();
            }
        })
        .map_err(|err| format!("could not listen for the tray: {err}"))?;

    let (reload_ping, reload_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the reload ping: {err}"))?;
    handle
        .insert_source(reload_source, |_, _, daemon: &mut Daemon| {
            daemon.config = Config::load();
            daemon.show();
        })
        .map_err(|err| format!("could not watch for reloads: {err}"))?;
    crate::signals::forward_reload_to_loop(reload_ping);

    let (quit_ping, quit_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the quit ping: {err}"))?;
    handle
        .insert_source(quit_source, |_, _, daemon: &mut Daemon| daemon.exit = true)
        .map_err(|err| format!("could not watch for a stop: {err}"))?;
    crate::signals::forward_to_loop(quit_ping);

    // Registered last, once everything that keeps the item current is in place.
    published.register();

    let mut daemon = Daemon {
        config,
        system,
        published,
        snapshot,
        settling: false,
        logged: None,
        ids,
        connecting: HashSet::new(),
        connected,
        battery_warned: HashMap::new(),
        prompts: HashMap::new(),
        next_token: 0,
        answers,
        exit: false,
    };
    daemon.log(&first);
    if daemon.snapshot.is_some() {
        daemon.register_agent();
    }
    daemon.warn_batteries();
    let _pidfile = crate::pidfile::write();

    while !daemon.exit {
        event_loop
            .dispatch(None, &mut daemon)
            .map_err(|err| format!("event loop failed: {err}"))?;
    }
    for (_, mut pending) in daemon.prompts.drain() {
        let _ = pending.child.kill();
        let _ = pending.child.wait();
    }
    Ok(())
}

/// Read BlueZ once and say what the icon would show, for `--status`.
pub fn status() -> Result<(), String> {
    let config = Config::load();
    let system = system_bus().map_err(|err| format!("no system bus: {err}"))?;
    let snapshot = bluez::read(&system).ok();
    let hard = rfkill::hard_blocked();
    let view = indicator::view(
        snapshot.as_ref(),
        Extra {
            hard_blocked: hard,
            connecting: false,
        },
        config.indicator,
    );
    println!("status:  {}", view.status.as_str());
    println!("icon:    {}", view.icon);
    println!("title:   {}", view.title);
    for line in view.body.lines() {
        println!("         {line}");
    }
    match snapshot {
        None => println!("bluetoothd did not answer"),
        Some(snapshot) => {
            match &snapshot.adapter {
                Some(adapter) => println!(
                    "adapter: {} {} ({}{})",
                    adapter.path,
                    adapter.name,
                    adapter.power_state,
                    if hard { ", hardware switch off" } else { "" }
                ),
                None => println!("adapter: none"),
            }
            for device in &snapshot.devices {
                println!(
                    "device:  {} {}{}{}",
                    device.address,
                    device.name,
                    if device.paired { ", paired" } else { "" },
                    if device.connected { ", connected" } else { "" },
                );
            }
        }
    }
    Ok(())
}
