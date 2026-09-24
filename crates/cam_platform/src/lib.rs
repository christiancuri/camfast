//! macOS integration that is independent of the UI framework.
//!
//! Everything here talks to AppKit / Foundation / ServiceManagement through `objc2`.
//! Unless stated otherwise, functions must be called from the main thread.

use std::ptr::NonNull;

use anyhow::{Context, anyhow, bail};
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification};
use objc2_foundation::{
    NSActivityOptions, NSBundle, NSNotification, NSNotificationCenter, NSOperationQueue,
    NSProcessInfo, NSString,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// Error shown when a login-item operation is attempted outside `CamFast.app`.
pub const NOT_IN_BUNDLE_MESSAGE: &str =
    "Only available in CamFast.app (run packaging/macos/bundle.sh)";

// ---------------------------------------------------------------------------------------------
// Power notifications
// ---------------------------------------------------------------------------------------------

/// Callbacks for system power events.
///
/// The callbacks are not `Send`: they are always invoked on the main thread (the observers are
/// registered with `NSOperationQueue.mainQueue`), and [`observe_power`] itself must be called
/// from the main thread.
pub struct PowerCallbacks {
    pub will_sleep: Box<dyn Fn() + 'static>,
    pub did_wake: Box<dyn Fn() + 'static>,
}

/// Keeps the NSWorkspace observers alive; dropping it unregisters them.
///
/// Not `Send`/`Sync`: must be dropped on the main thread, like it was created.
pub struct PowerObserver {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    // Keep the blocks alive for as long as the observers exist. NSNotificationCenter copies the
    // block, so this is belt and braces, but it also makes the ownership explicit.
    _blocks: Vec<RcBlock<dyn Fn(NonNull<NSNotification>)>>,
}

impl Drop for PowerObserver {
    fn drop(&mut self) {
        for token in self.tokens.drain(..) {
            // SAFETY: `token` was returned by addObserverForName:… on this same center.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
    }
}

/// Registers for `NSWorkspaceWillSleepNotification` / `NSWorkspaceDidWakeNotification`.
///
/// Must be called on the main thread; callbacks are delivered on the main thread.
pub fn observe_power(callbacks: PowerCallbacks) -> anyhow::Result<PowerObserver> {
    require_main_thread("observe_power")?;

    let workspace = NSWorkspace::sharedWorkspace();
    let center = workspace.notificationCenter();
    let main_queue = NSOperationQueue::mainQueue();

    let PowerCallbacks {
        will_sleep,
        did_wake,
    } = callbacks;

    let sleep_block: RcBlock<dyn Fn(NonNull<NSNotification>)> =
        RcBlock::new(move |_: NonNull<NSNotification>| {
            tracing::info!("NSWorkspaceWillSleepNotification");
            will_sleep();
        });
    let wake_block: RcBlock<dyn Fn(NonNull<NSNotification>)> =
        RcBlock::new(move |_: NonNull<NSNotification>| {
            tracing::info!("NSWorkspaceDidWakeNotification");
            did_wake();
        });

    // SAFETY: the blocks are `'static`, the queue is the main queue and we are on the main
    // thread, so the closures (which are not `Send`) are only ever invoked on the main thread.
    let sleep_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            Some(&main_queue),
            &sleep_block,
        )
    };
    let wake_token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            Some(&main_queue),
            &wake_block,
        )
    };

    Ok(PowerObserver {
        center,
        tokens: vec![sleep_token, wake_token],
        _blocks: vec![sleep_block, wake_block],
    })
}

// ---------------------------------------------------------------------------------------------
// App Nap
// ---------------------------------------------------------------------------------------------

/// Holds an NSProcessInfo activity (`UserInitiatedAllowingIdleSystemSleep`) that prevents App Nap
/// from throttling RTSP keepalives. Dropping it ends the activity.
pub struct AppNapGuard {
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl Drop for AppNapGuard {
    fn drop(&mut self) {
        // SAFETY: `token` was returned by beginActivityWithOptions:reason: of the same process info.
        unsafe { NSProcessInfo::processInfo().endActivity(&self.token) };
    }
}

/// Starts a `UserInitiatedAllowingIdleSystemSleep` activity. Safe to call from any thread.
pub fn prevent_app_nap(reason: &str) -> AppNapGuard {
    let reason = NSString::from_str(reason);
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &reason,
    );
    AppNapGuard { token }
}

// ---------------------------------------------------------------------------------------------
// Bundle / login item
// ---------------------------------------------------------------------------------------------

/// Whether the current executable is running from a `.app` bundle with a bundle identifier
/// (as opposed to a bare `cargo run` binary).
pub fn is_app_bundle() -> bool {
    let bundle = NSBundle::mainBundle();
    let path = bundle.bundlePath().to_string();
    path.ends_with(".app") && bundle.bundleIdentifier().is_some()
}

fn main_app_service() -> anyhow::Result<Retained<SMAppService>> {
    if !is_app_bundle() {
        bail!(NOT_IN_BUNDLE_MESSAGE);
    }
    // SAFETY: plain class method with no arguments.
    Ok(unsafe { SMAppService::mainAppService() })
}

fn login_item_status() -> Option<SMAppServiceStatus> {
    let service = main_app_service().ok()?;
    // SAFETY: `service` is a valid SMAppService.
    Some(unsafe { service.status() })
}

/// Whether the app is registered to open at login (SMAppService.mainApp).
/// Returns `false` when not running from a `.app` bundle.
pub fn open_at_login_enabled() -> bool {
    login_item_status() == Some(SMAppServiceStatus::Enabled)
}

/// Whether the login item was registered but still needs the user's approval in
/// System Settings > General > Login Items. Use [`open_login_items_settings`] to take them there.
pub fn open_at_login_requires_approval() -> bool {
    login_item_status() == Some(SMAppServiceStatus::RequiresApproval)
}

/// Registers/unregisters the running .app bundle as a login item (SMAppService.mainApp).
///
/// Returns an error when not running from a .app bundle. Registration that ends in
/// `RequiresApproval` is reported as `Ok(())`; check [`open_at_login_requires_approval`].
pub fn set_open_at_login(enabled: bool) -> anyhow::Result<()> {
    let service = main_app_service()?;
    // SAFETY: `service` is a valid SMAppService; the calls only touch launchd state.
    let status = unsafe { service.status() };
    if enabled {
        if matches!(
            status,
            SMAppServiceStatus::Enabled | SMAppServiceStatus::RequiresApproval
        ) {
            return Ok(());
        }
        unsafe { service.registerAndReturnError() }
            .map_err(|e| anyhow!("{}", e.localizedDescription()))
            .context("Could not enable 'Open at login'")?;
    } else {
        if status == SMAppServiceStatus::NotRegistered {
            return Ok(());
        }
        unsafe { service.unregisterAndReturnError() }
            .map_err(|e| anyhow!("{}", e.localizedDescription()))
            .context("Could not disable 'Open at login'")?;
    }
    Ok(())
}

/// Opens System Settings > General > Login Items.
pub fn open_login_items_settings() {
    // SAFETY: plain class method with no arguments.
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

// ---------------------------------------------------------------------------------------------

fn require_main_thread(what: &str) -> anyhow::Result<()> {
    if MainThreadMarker::new().is_none() {
        bail!("{what} must be called on the main thread");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_binary_is_not_a_bundle() {
        assert!(!is_app_bundle());
    }

    #[test]
    fn login_item_outside_bundle_errors() {
        let err = set_open_at_login(true).unwrap_err();
        assert_eq!(err.to_string(), NOT_IN_BUNDLE_MESSAGE);
        let err = set_open_at_login(false).unwrap_err();
        assert_eq!(err.to_string(), NOT_IN_BUNDLE_MESSAGE);
        assert!(!open_at_login_enabled());
        assert!(!open_at_login_requires_approval());
    }

    #[test]
    fn app_nap_guard_begins_and_ends_activity() {
        let guard = prevent_app_nap("test");
        drop(guard);
    }

    #[test]
    fn observe_power_off_main_thread_errors() {
        // Test threads are never the main thread.
        let result = observe_power(PowerCallbacks {
            will_sleep: Box::new(|| {}),
            did_wake: Box::new(|| {}),
        });
        assert!(result.is_err());
    }
}
