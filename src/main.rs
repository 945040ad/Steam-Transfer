//! Steam Game Transfer — copy installed Steam games between PCs over USB4/Thunderbolt,
//! ethernet, Wi-Fi or a USB drive, without downloading them again.

#![windows_subsystem = "windows"]

mod app;
mod apps;
mod hardware;
mod installer;
mod launchers;
mod net;
mod pipeline;
mod settings;
mod steam;
mod transfer;
mod util;
mod vdf;
#[cfg(test)]
mod e2e_tests;

slint::include_modules!();

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| args.iter().any(|a| a.eq_ignore_ascii_case(flag));

    if has("--uninstall") {
        installer::uninstall();
        return;
    }
    let Some(_instance) = installer::SingleInstance::acquire(has("--after-install")) else {
        installer::message("Steam Game Transfer is already open.");
        return;
    };
    if let Err(e) = app::run() {
        installer::message(&format!("Steam Game Transfer couldn't start:\n{e}"));
    }
}
