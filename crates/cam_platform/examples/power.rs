//! Manual test for sleep/wake notifications and the App Nap guard.
//!
//! Run with `cargo run -p cam_platform --example power`, then put the Mac to sleep
//! (Apple menu > Sleep, or close the lid) and wake it. Ctrl-C to quit.

use cam_platform::{PowerCallbacks, observe_power, prevent_app_nap};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");

    let _nap = prevent_app_nap("cam_platform power example");
    let _observer = observe_power(PowerCallbacks {
        will_sleep: Box::new(|| println!("[power] will sleep")),
        did_wake: Box::new(|| println!("[power] did wake")),
    })?;

    println!(
        "is_app_bundle = {}, open_at_login_enabled = {}",
        cam_platform::is_app_bundle(),
        cam_platform::open_at_login_enabled()
    );
    println!("Waiting for sleep/wake notifications (Ctrl-C to quit)...");

    // NSWorkspace notifications need a running AppKit main loop.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.run();
    Ok(())
}
