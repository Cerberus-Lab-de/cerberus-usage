#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(debug_assertions)]
    println!("cerberus-usage {}", env!("CARGO_PKG_VERSION"));
}
