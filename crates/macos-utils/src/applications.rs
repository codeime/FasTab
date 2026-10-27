use std::sync::{Mutex, OnceLock};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::Bool;
use objc2::{ClassType, msg_send};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSBundle, NSString};

#[derive(Debug)]
pub struct MacOSApplication {
    pub name: Option<String>,
    pub bundle_identifier: Option<String>,
    pub bundle_path: Option<String>,
    pub process_identifier: libc::pid_t,
}

pub fn running_applications() -> Vec<MacOSApplication> {
    // These queries also run on Rust worker threads, where AppKit's temporary
    // arrays would otherwise remain in a thread-lifetime autorelease pool.
    autoreleasepool(|_| unsafe {
        let workspace = NSWorkspace::sharedWorkspace();
        let apps = workspace.runningApplications();
        apps.iter()
            .map(|app| {
                let name = app.localizedName().map(|s| s.to_string());
                let bundle_identifier = app.bundleIdentifier().map(|s| s.to_string());
                let bundle_path = app.bundleURL().and_then(|url| url.path()).map(|s| s.to_string());
                let process_identifier = app.processIdentifier();

                MacOSApplication {
                    name,
                    bundle_identifier,
                    bundle_path,
                    process_identifier,
                }
            })
            .collect()
    })
}

/// PIDs of the running applications with this bundle identifier.
///
/// Asks AppKit for the match instead of enumerating every running application
/// and allocating its name, bundle id and path the way [`running_applications`]
/// does, which makes it cheap enough to poll in a wait loop.
pub fn running_application_pids(bundle_identifier: &str) -> Vec<libc::pid_t> {
    autoreleasepool(|_| {
        let identifier = NSString::from_str(bundle_identifier);
        let apps = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier) };
        apps.iter().map(|app| unsafe { app.processIdentifier() }).collect()
    })
}

/// Read the bundle of the actual running process, not a LaunchServices match
/// that might name another installed version or a mounted disk image.
pub fn running_application_version(pid: libc::pid_t) -> Option<String> {
    type CachedVersion = (libc::pid_t, u64, Option<String>);
    static CACHE: OnceLock<Mutex<Option<CachedVersion>>> = OnceLock::new();
    autoreleasepool(|_| {
        let app = unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid) }?;
        let launched_at = unsafe { app.launchDate()?.timeIntervalSince1970() }.to_bits();
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some((cached_pid, cached_launch, version)) = cache.as_ref() {
            if *cached_pid == pid && *cached_launch == launched_at {
                return version.clone();
            }
        }
        let version = (|| unsafe {
            let bundle_url = app.bundleURL()?;
            let bundle = NSBundle::bundleWithURL(&bundle_url)?;
            let value = bundle.objectForInfoDictionaryKey(&NSString::from_str("CFBundleShortVersionString"))?;
            let is_string: Bool = msg_send![&*value, isKindOfClass: NSString::class()];
            if !is_string.as_bool() {
                return None;
            }
            let version: Retained<NSString> = Retained::cast(value);
            Some(version.to_string())
        })();
        *cache = Some((pid, launched_at, version.clone()));
        version
    })
}

/// CLI callers know a terminal bundle rather than its GUI PID. Ambiguous
/// multiple instances remain unknown so capability selection stays conservative.
pub fn unique_running_application_version(bundle_identifier: &str) -> Option<String> {
    let pids = running_application_pids(bundle_identifier);
    match pids.as_slice() {
        [pid] => running_application_version(*pid),
        _ => None,
    }
}

// There is deliberately no `launch_application`. `NSWorkspace` launches made
// from our process record it as the new app's LaunchServices parent, and
// loginwindow force-quits child apps after their parent exits. Launch helpers
// with `/usr/bin/open` (see `fastab_integrations::input_method`).

#[cfg(test)]
mod tests {
    use super::*;

    /// [`running_application_pids`] decides whether callers consider a helper to
    /// be up, so it has to see everything the full enumeration sees.
    #[test]
    fn pid_query_agrees_with_owned_snapshots_after_the_query_thread_exits() {
        // Every query's pool has drained, and the originating thread is gone,
        // before the returned bundle strings are used for another AppKit query.
        let applications = std::thread::spawn(running_applications).join().unwrap();
        for app in applications {
            let Some(bundle_id) = app.bundle_identifier.as_deref() else {
                continue;
            };
            if running_application_pids(bundle_id).contains(&app.process_identifier) {
                continue;
            }

            // A process that exited between the two calls is not a mismatch.
            let still_running = running_applications()
                .iter()
                .any(|other| other.process_identifier == app.process_identifier);
            assert!(
                !still_running,
                "{bundle_id} (pid {}) is running but the bundle id query missed it",
                app.process_identifier
            );
        }
    }
}
