// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// App logic lives in the lib crate so the mobile targets can link it too.
fn main() {
    // Headless diagnostics: tailsend --selftest <serve|send|peers> [...]
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--selftest" {
        let code = tailsend_lib::selftest::run(&args[2..]);
        if code != 0 {
            std::process::exit(code);
        }
        return;
    }

    tailsend_lib::run();
}
