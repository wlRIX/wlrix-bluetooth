// SPDX-License-Identifier: GPL-3.0-or-later
//! The pairing dialog, `wlrix-bluetooth-prompt`, run as a child process.
//!
//! The dialog is an Avalonia application in `wlrix-apps`, like `wlrix-network-prompt`, and this
//! daemon draws nothing. The exchange is one line of JSON each way:
//!
//! ```text
//! -> {"mode":"confirm","device":"WH-1000XM4","passkey":"012345"}
//! <- {"accept":true}
//! -> {"mode":"pin","device":"Old Keyboard"}
//! <- {"pin":"0000"}
//! -> {"mode":"display","device":"K380","code":"123456"}
//!    (nothing: it stays up until it is closed from here)
//! ```
//!
//! and Cancel -- or closing the window -- is an exit with nothing written. A `display` dialog has
//! nothing to answer: it shows the code to type on the device and is closed from here, when
//! bluetoothd says the pairing is over.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

use serde::{Deserialize, Serialize};

/// The program, found on `PATH`. `WLRIX_BLUETOOTH_PROMPT` names another, for running a build of
/// the dialog that is not installed.
const PROGRAM: &str = "wlrix-bluetooth-prompt";

/// What the dialog is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Request {
    /// "Does the device show this number?" -- numeric comparison, the usual case.
    Confirm { device: String, passkey: String },
    /// "Type this on the device" -- a keyboard being paired.
    Display { device: String, code: String },
    /// The PIN a legacy device wants: often printed on it, often `0000`.
    Pin { device: String },
    /// The six-digit passkey a device shows.
    Passkey { device: String },
    /// "Allow this device to connect?" -- a service on a device that is paired but not trusted.
    Authorize {
        device: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        service: Option<String>,
    },
}

/// What the person said. Each field belongs to one mode; the others are absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub accept: Option<bool>,
    #[serde(default)]
    pub pin: Option<String>,
    #[serde(default)]
    pub passkey: Option<u32>,
}

/// A passkey as devices show it: six digits, zero-padded.
pub fn six_digits(passkey: u32) -> String {
    format!("{passkey:06}")
}

/// One line of request.
pub fn request_line(request: &Request) -> String {
    serde_json::to_string(request).expect("a request always serializes")
}

/// The dialog's answer, or `None` for a Cancel, a crash, or anything else that is not one line of
/// the expected shape.
pub fn parse_answer(text: &str) -> Option<Answer> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Start the dialog. The answer arrives on `answers` with `token`, once it is given -- `None` for
/// a Cancel or a dialog that was closed. Returns the child, for closing it early.
pub fn spawn(
    request: &Request,
    token: u64,
    answers: calloop::channel::Sender<(u64, Option<Answer>)>,
) -> Result<Child, String> {
    let program = std::env::var("WLRIX_BLUETOOTH_PROMPT").unwrap_or_else(|_| PROGRAM.to_owned());
    let mut child = Command::new(&program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("could not start {program}: {err}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let line = request_line(request);
    std::thread::Builder::new()
        .name("prompt".into())
        .spawn(move || {
            // Written here rather than before the thread starts: a dialog that never reads its
            // stdin would otherwise block the main loop on a full pipe.
            let _ = writeln!(stdin, "{line}");
            drop(stdin);
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = answers.send((token, parse_answer(&text)));
        })
        .map_err(|err| format!("could not start the prompt thread: {err}"))?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_the_shape_the_dialog_parses() {
        let line = request_line(&Request::Confirm {
            device: "Headset".into(),
            passkey: six_digits(12345),
        });
        assert_eq!(
            line,
            r#"{"mode":"confirm","device":"Headset","passkey":"012345"}"#
        );
        assert_eq!(
            request_line(&Request::Authorize {
                device: "Phone".into(),
                service: None
            }),
            r#"{"mode":"authorize","device":"Phone"}"#
        );
    }

    #[test]
    fn an_answer_is_read_and_anything_else_is_a_cancel() {
        assert_eq!(
            parse_answer(r#"{"accept":true}"#).unwrap().accept,
            Some(true)
        );
        assert_eq!(
            parse_answer(r#"{"pin":"0000"}"#).unwrap().pin.as_deref(),
            Some("0000")
        );
        assert_eq!(
            parse_answer(r#"{"passkey":123456}"#).unwrap().passkey,
            Some(123456)
        );
        assert_eq!(parse_answer(""), None, "Cancel writes nothing");
        assert_eq!(parse_answer("garbage"), None);
    }

    #[test]
    fn a_missing_dialog_is_an_error_not_a_hang() {
        // SAFETY of the test: the variable is read once, at spawn, in this process only.
        unsafe { std::env::set_var("WLRIX_BLUETOOTH_PROMPT", "/nonexistent/prompt") };
        let (sender, _channel) = calloop::channel::channel();
        let request = Request::Pin { device: "x".into() };
        assert!(spawn(&request, 1, sender).is_err());
        unsafe { std::env::remove_var("WLRIX_BLUETOOTH_PROMPT") };
    }
}
