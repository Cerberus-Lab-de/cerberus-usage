#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use cerberus_usage::{single_instance, ui};

fn main() -> eframe::Result {
    if !single_instance::acquire_app() {
        single_instance::show_already_running_notice();
        return Ok(());
    }
    ui::run()
}
