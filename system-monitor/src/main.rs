// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector;
mod models;
mod worker;

use std::error::Error;

slint::include_modules!();

use worker::start_worker;

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    start_worker(ui.as_weak());

    ui.run()?;

    Ok(())
}
