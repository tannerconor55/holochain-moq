// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(feature = "desktop")]
    turbo_kart_desktop::run();
    #[cfg(not(feature = "desktop"))]
    {
        eprintln!("turbo-kart-desktop was built without the `desktop` feature (no Tauri shell)");
        std::process::exit(2);
    }
}
