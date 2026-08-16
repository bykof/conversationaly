#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// No logger is initialised here on purpose. This used to be
// `set_var("RUST_LOG", "info")` + `env_logger::init()`, which (a) overwrote the
// RUST_LOG that clean_run.sh exports, so `./clean_run.sh debug` never produced
// debug output, and (b) wrote only to stderr — discarded entirely on a Windows
// release build, which has no console. Logging is now set up by
// tauri-plugin-log in `app_lib::run()`, which writes both to stdout and to a
// rotating file in the app log dir.
fn main() {
    app_lib::run();
}
