#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cerberus_usage::{crash, single_instance, ui};

fn main() -> eframe::Result {
    crash::install_panic_hook();
    if !single_instance::acquire_app() {
        single_instance::show_already_running_notice();
        return Ok(());
    }
    // Without a console an `Err` would vanish: say what happened and leave a trace in crash.log.
    let result = ui::run();
    if let Err(e) = &result {
        crash::report_startup_failure(e);
    }
    result
}
