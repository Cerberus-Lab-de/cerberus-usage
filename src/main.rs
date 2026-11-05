#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cerberus_usage::{cleanup, crash, single_instance, ui};

fn main() -> eframe::Result {
    // Only flag: run by the uninstaller after the user agreed. No window, no single-instance lock.
    if std::env::args_os().nth(1).is_some_and(|a| a == "--uninstall-cleanup") {
        cleanup::run_uninstall_cleanup();
        return Ok(());
    }
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
