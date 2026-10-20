// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX Bluetooth status: BlueZ's adapter and devices, as an icon in `wlrix-tray`, and its
//! pairing agent.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rest = args.iter();
    if let Some(argument) = rest.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!(
                    "wlrix-bluetooth {}\n\n\
                     Bluetooth status for the wlRIX desktop: the adapter and paired devices as\n\
                     a StatusNotifierItem, drawn by wlrix-tray, and BlueZ's pairing agent.\n\
                     Started by wlrix-session; needs bluetoothd on the system bus and a\n\
                     session D-Bus.\n\n\
                     Usage: wlrix-bluetooth [options]\n\n\
                     Options:\n  \
                       --status               read BlueZ once, say what the icon\n                         \
                                              would show, exit\n  \
                       --check-config <path>  say whether that file would be accepted, exit\n  \
                       -h, --help             this message\n  \
                       -V, --version          print the version\n\n\
                     Settings live in ~/.config/wlrix/bluetooth.toml, or /etc/wlrix/bluetooth.toml.",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            "--version" | "-V" => {
                println!("wlrix-bluetooth {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--check-config" => {
                let Some(path) = rest.next() else {
                    eprintln!("wlrix-bluetooth: --check-config needs a path");
                    std::process::exit(2);
                };
                if let Err(why) = wlrix_bluetooth::config::check(std::path::Path::new(path)) {
                    eprintln!("{why}");
                    std::process::exit(1);
                }
                return;
            }
            "--status" => {
                if let Err(err) = wlrix_bluetooth::daemon::status() {
                    eprintln!("wlrix-bluetooth: {err}");
                    std::process::exit(1);
                }
                return;
            }
            other => {
                eprintln!("wlrix-bluetooth: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if let Err(err) = wlrix_bluetooth::daemon::run() {
        eprintln!("wlrix-bluetooth: {err}");
        std::process::exit(1);
    }
}
