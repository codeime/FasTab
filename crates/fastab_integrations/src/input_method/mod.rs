use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{Boolean, CFGetTypeID, CFType, CFTypeID, CFTypeRef, OSStatus, TCFType, TCFTypeRef};
use core_foundation::boolean::CFBoolean;
use core_foundation::bundle::{CFBundle, CFBundleRef};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::{CFURL, CFURLRef};
use core_foundation::{declare_TCFType, impl_TCFType};
use fastab_settings::state;
use fastab_util::directories::{fig_data_dir, home_dir};
use fastab_util::macos::BUNDLE_CONTENTS_HELPERS_PATH;
use macos_utils::applications;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, trace, warn};

use crate::Integration;
use crate::error::{Error, ErrorExt, Result};

pub enum __TISInputSource {}
pub type TISInputSourceRef = *const __TISInputSource;

declare_TCFType! {
    TISInputSource, TISInputSourceRef
}
impl_TCFType!(TISInputSource, TISInputSourceRef, TISInputSourceGetTypeID);

// https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/TextInputSources.h
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    pub static kTISPropertyBundleID: CFStringRef;
    pub static kTISPropertyInputSourceCategory: CFStringRef;
    pub static kTISPropertyInputSourceType: CFStringRef;
    pub static kTISPropertyInputSourceID: CFStringRef;
    pub static kTISPropertyInputSourceIsEnabled: CFStringRef;
    pub static kTISPropertyInputSourceIsSelected: CFStringRef;
    pub static kTISPropertyInputSourceIsEnableCapable: CFStringRef;
    pub static kTISPropertyInputSourceIsSelectCapable: CFStringRef;
    pub static kTISPropertyLocalizedName: CFStringRef;
    pub static kTISPropertyInputModeID: CFStringRef;

    // Can not be used as properties to filter TISCreateInputSourceList
    pub static kTISCategoryKeyboardInputSource: CFStringRef;

    pub static kTISNotifySelectedKeyboardInputSourceChanged: CFStringRef;

    pub static kTISNotifyEnabledKeyboardInputSourcesChanged: CFStringRef;

    pub fn TISInputSourceGetTypeID() -> CFTypeID;

    pub fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all_installed: bool) -> CFArrayRef;

    pub fn TISGetInputSourceProperty(input_source: TISInputSourceRef, property_key: CFStringRef) -> CFTypeRef;

    pub fn TISSelectInputSource(input_source: TISInputSourceRef) -> OSStatus;

    pub fn TISDeselectInputSource(input_source: TISInputSourceRef) -> OSStatus;

    pub fn TISEnableInputSource(input_source: TISInputSourceRef) -> OSStatus;

    pub fn TISDisableInputSource(input_source: TISInputSourceRef) -> OSStatus;

    pub fn TISRegisterInputSource(location: CFURLRef) -> OSStatus;
}

pub struct InputMethod {
    pub bundle_path: PathBuf,
}

/// Code identity of the IME we last launched (see [`code_identity`]). Compared
/// with the bundle on disk to decide whether a running process must be
/// replaced. The key name predates the code digest.
const LAUNCHED_BINARY_HASH_KEY: &str = "input-method.launched-binary-sha256";

/// What decides whether a running IME is stale. Builds record
/// `FastabCodeDigest` in the helper's Info.plist: a digest of the sources,
/// locked crates and compiler that make up its code, which stays put across
/// releases that do not touch the IME. The executable's own SHA-256 changes on
/// every release (version bump, signature), so comparing it replaced the IME,
/// and cut every open terminal's IMK connection, on each update. Bundles from
/// before the digest fall back to that SHA-256.
fn code_identity(bundle: &Path, executable: &Path) -> Option<String> {
    plist::Value::from_file(bundle.join("Contents/Info.plist"))
        .ok()
        .and_then(|info| {
            info.as_dictionary()?
                .get("FastabCodeDigest")?
                .as_string()
                .filter(|digest| !digest.is_empty())
                .map(|digest| format!("code:{digest}"))
        })
        .or_else(|| sha256_hex(executable))
}
/// An explicit uninstall must override an old symlink or the legacy enabled
/// bit. `None` means this preference predates the restore-on-launch behavior.
const RESTORE_ON_LAUNCH_KEY: &str = "input-method.restore-on-launch";
const INPUT_METHOD_LOCK_FILE: &str = "input-method.operation.lock";

async fn lock_input_method_file(path: &Path) -> Result<Flock<File>> {
    let path = path.to_path_buf();
    let mut file = tokio::task::spawn_blocking(move || -> Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Keep this file across runs so every process locks the same inode.
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?)
    })
    .await
    .map_err(|error| Error::Custom(format!("Could not open input method operation lock: {error}").into()))??;

    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => return Ok(guard),
            Err((unlocked_file, nix::errno::Errno::EWOULDBLOCK)) => {
                file = unlocked_file;
                // Waiting is cancellable: dropping this future closes the file
                // without leaving a blocking worker or an eventual lock owner.
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
            Err((_, error)) => return Err(error.into()),
        }
    }
}

async fn operation_lock() -> Result<Flock<File>> {
    let path = fig_data_dir()?.join(INPUT_METHOD_LOCK_FILE);
    lock_input_method_file(&path).await
}

fn restore_on_launch(explicit: Option<bool>, legacy_enabled: Option<bool>, correct_symlink: bool) -> bool {
    explicit.unwrap_or(legacy_enabled == Some(true) || correct_symlink)
}

/// Launch through `/usr/bin/open`, never `NSWorkspace` from our own process.
/// LaunchServices records the app that asks for a launch as the new app's
/// parent (`lsappinfo info` shows `parentASN="FasTab"`), and loginwindow
/// force-quits a parent's child apps about 3 s after the parent exits. Every
/// desktop quit or update therefore killed the IME, and with it the IMK
/// connection of every open Otty / Ghostty / Kitty window. `open` is not an
/// app, so a helper it launches has no parent and outlives the desktop app.
fn launch_without_parent_app(bundle: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("/usr/bin/open");
    // -g: a background-only helper must not take focus from the terminal.
    command.arg("-g").arg(bundle);
    command
}

fn running_and_enabled(running: bool, palette_enabled: bool) -> Result<(), InputMethodError> {
    if !running {
        return Err(InputMethodError::NotRunning);
    }
    if !palette_enabled {
        return Err(InputMethodError::NotEnabled);
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Option<String> {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

/// `kill(pid, 0)`: liveness without touching AppKit, so a wait loop costs a
/// syscall per process instead of a run through every running application.
fn process_is_alive(pid: Pid) -> bool {
    signal::kill(pid, None).is_ok()
}

/// True if every one of `pids` is gone before `timeout` elapses.
fn wait_for_exit(pids: &[Pid], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pids.iter().copied().any(process_is_alive) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `launched` is the hash we recorded when we last started the IME; `disk` is
/// the hash of the bundle we want to run. Missing tracker state is *not* stale:
/// killing on that would undo `install.sh` when it already decided the bytes
/// match. The caller pins the hash instead. Sparkle after this tracker has
/// been written still sees `Some(old) != Some(new)` and replaces.
fn process_is_stale(launched: Option<&str>, disk: Option<&str>) -> bool {
    match (launched, disk) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

use thiserror::Error;

#[derive(Error, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InputMethodError {
    #[error("Could not list input sources")]
    CouldNotListInputSources,
    #[error("No input sources for bundle identifier {:?}", .identifier)]
    NoInputSourcesForBundleIdentifier { identifier: Cow<'static, str> },
    #[error("Invalid input method bundle destination")]
    InvalidDestination,
    #[error("Invalid path to bundle. Perhaps use an absolute path instead?")]
    InvalidBundlePath,
    #[error("Invalid input method bundle: {}", .inner)]
    InvalidBundle { inner: Cow<'static, str> },
    #[error("OSStatus error code: {0}")]
    OSStatusError(OSStatus),
    #[error("Input source is not enabled")]
    NotEnabled,
    #[error("Input source is not selected")]
    NotSelected,
    #[error("Input method not running")]
    NotRunning,
    #[error("An unknown error occurred")]
    UnknownError,
    #[error("Not installed")]
    NotInstalled,
}

#[macro_export]
macro_rules! tis_action {
    ($action:ident, $function:ident) => {
        pub fn $action(&self) -> Result<(), InputMethodError> {
            debug!("{} input source.", stringify!($action));
            unsafe {
                match $function(self.as_concrete_TypeRef()) {
                    0 => Ok(()),
                    i => Err(InputMethodError::OSStatusError(i).into()),
                }
            }
        }
    };
}

#[macro_export]
macro_rules! tis_property {
    ($name:ident, $tis_property_key:expr, $cf_type:ty, $rust_type:ty, $convert:ident) => {
        #[allow(dead_code)]
        pub fn $name(&self) -> Option<$rust_type> {
            trace!("Get '{}' from input source", stringify!($name));
            self.get_property::<$cf_type>($tis_property_key)
                .map(|s| s.$convert())
        }
    };
}

#[macro_export]
macro_rules! tis_bool_property {
    ($name:ident, $tis_property_key:expr) => {
        tis_property!($name, $tis_property_key, CFBoolean, bool, into);
    };
}

#[macro_export]
macro_rules! tis_string_property {
    ($name:ident, $tis_property_key:expr) => {
        tis_property!($name, $tis_property_key, CFString, String, to_string);
    };
}

impl TISInputSource {
    tis_string_property!(bundle_id, unsafe { kTISPropertyBundleID });

    tis_string_property!(input_source_id, unsafe { kTISPropertyInputSourceID });

    tis_string_property!(category, unsafe { kTISPropertyInputSourceCategory });

    tis_string_property!(localized_name, unsafe { kTISPropertyLocalizedName });

    tis_string_property!(input_mode_id, unsafe { kTISPropertyInputModeID });

    tis_string_property!(category_keyboard, unsafe { kTISCategoryKeyboardInputSource });

    tis_bool_property!(is_enabled, unsafe { kTISPropertyInputSourceIsEnabled });

    tis_bool_property!(is_enable_capable, unsafe { kTISPropertyInputSourceIsEnableCapable });

    tis_bool_property!(is_selected, unsafe { kTISPropertyInputSourceIsSelected });

    tis_bool_property!(is_select_capable, unsafe { kTISPropertyInputSourceIsSelectCapable });

    tis_action!(enable, TISEnableInputSource);

    tis_action!(disable, TISDisableInputSource);

    tis_action!(select, TISSelectInputSource);

    tis_action!(deselect, TISDeselectInputSource);

    // TODO: change to use FromVoid
    fn get_property<T: TCFType>(&self, key: CFStringRef) -> Option<T> {
        unsafe {
            let value: CFTypeRef = TISGetInputSourceProperty(self.as_concrete_TypeRef(), key);

            if value.is_null() {
                None
            } else if T::type_id() == CFGetTypeID(value) {
                // This has to be under get rule
                // https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/TextInputSources.h#L695
                let value = <T::Ref as TCFTypeRef>::from_void_ptr(value);
                Some(T::wrap_under_get_rule(value))
            } else {
                None
            }
        }
    }
}

impl std::fmt::Debug for TISInputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TISInputSource")
            .field("bundle_id", &self.bundle_id())
            .field("input_source_id", &self.input_source_id())
            .field("input_source_category", &self.category())
            .field("input_source_is_enabled", &self.is_enabled())
            .field("input_source_is_selected", &self.is_selected())
            .field("localized_name", &self.localized_name())
            .field("input_mode_id", &self.input_mode_id())
            .field("category_keyboard", &self.category_keyboard())
            .finish()
    }
}

impl std::default::Default for InputMethod {
    fn default() -> Self {
        let fig_app_path = fastab_util::app_bundle_path();
        let bundle_path = fig_app_path
            .join(BUNDLE_CONTENTS_HELPERS_PATH)
            .join("FastabInputMethod.app");
        Self { bundle_path }
    }
}

impl InputMethod {
    /// Restore only a previously chosen integration. Older releases had no
    /// separate preference, and a failed status probe could clear their
    /// enabled bit, so an existing correct symlink is migration evidence too.
    pub fn should_restore_on_launch(&self) -> Result<bool> {
        let explicit = state::get_bool(RESTORE_ON_LAUNCH_KEY).map_err(|error| {
            Error::Custom(format!("Could not read input method restore preference: {error}").into())
        })?;
        if let Some(enabled) = explicit {
            return Ok(enabled);
        }
        // Only a genuinely absent preference permits legacy migration. A
        // failed read must not turn an explicit uninstall into a fresh opt-in.
        let legacy_enabled = state::get_bool(self.input_method_is_enabled_key())
            .map_err(|error| Error::Custom(format!("Could not read legacy input method preference: {error}").into()))?;
        let correct_symlink = match std::fs::read_link(self.target_bundle_path()?) {
            Ok(target) => target == self.bundle_path,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        Ok(restore_on_launch(None, legacy_enabled, correct_symlink))
    }

    fn set_restore_on_launch(enabled: bool) -> Result<()> {
        state::set_value(RESTORE_ON_LAUNCH_KEY, enabled)
            .map_err(|error| Error::Custom(format!("Could not save input method restore preference: {error}").into()))
    }

    pub fn input_method_directory() -> PathBuf {
        home_dir().unwrap().join("Library").join("Input Methods")
    }

    pub fn list_all_input_sources(
        properties: Option<&CFDictionary<CFType, CFType>>,
        include_all_installed: bool,
    ) -> Option<Vec<TISInputSource>> {
        let properties: CFDictionaryRef = match properties {
            Some(properties) => properties.as_concrete_TypeRef(),
            None => ptr::null(),
        };

        let sources = unsafe { TISCreateInputSourceList(properties, include_all_installed) };
        if sources.is_null() {
            return None;
        }

        let sources = unsafe { CFArray::<TISInputSource>::wrap_under_create_rule(sources) };

        Some(sources.into_iter().map(|value| value.to_owned()).collect())
    }

    pub fn register(location: impl AsRef<Path>) -> Result<(), InputMethodError> {
        debug!("Registering input source...");

        let url = match CFURL::from_path(location, true) {
            Some(url) => url,
            None => return Err(InputMethodError::InvalidDestination),
        };

        unsafe {
            match TISRegisterInputSource(url.as_concrete_TypeRef()) {
                0 => Ok(()),
                i => Err(InputMethodError::OSStatusError(i)),
            }
        }
    }

    pub fn list_input_sources_for_bundle_id(bundle_id: &str) -> Option<Vec<TISInputSource>> {
        let key: CFString = unsafe { CFString::wrap_under_create_rule(kTISPropertyBundleID) };
        let value = CFString::from(bundle_id);
        let properties = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

        InputMethod::list_all_input_sources(Some(&properties), true)
    }
}

unsafe extern "C" {
    pub fn CFBundleGetIdentifier(bundle: CFBundleRef) -> CFStringRef;
    pub fn CFPreferencesSynchronize(
        application_id: CFStringRef,
        username: CFStringRef,
        hostname: CFStringRef,
    ) -> Boolean;
    pub static kCFPreferencesCurrentUser: CFStringRef;
    pub static kCFPreferencesCurrentHost: CFStringRef;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

impl InputMethod {
    pub fn input_source(&self) -> Result<TISInputSource, InputMethodError> {
        let bundle_id_string: String = self.bundle_id()?;

        let sources = InputMethod::list_input_sources_for_bundle_id(&bundle_id_string)
            .ok_or(InputMethodError::CouldNotListInputSources)?;

        let bundle_identifier = CFString::from(bundle_id_string.as_str());
        match sources.len() {
            0 => Err(InputMethodError::NoInputSourcesForBundleIdentifier {
                identifier: bundle_identifier.to_string().into(),
            }),
            _ => sources
                .into_iter()
                .next()
                .ok_or_else(|| InputMethodError::NoInputSourcesForBundleIdentifier {
                    identifier: bundle_identifier.to_string().into(),
                }),
        }
    }

    pub fn target_bundle_path(&self) -> Result<PathBuf, InputMethodError> {
        let input_method_name = match self.bundle_path.components().next_back() {
            Some(name) => name.as_os_str(),
            None => {
                return Err(InputMethodError::InvalidBundle {
                    inner: "Input method bundle name cannot be determined".into(),
                });
            },
        };

        Ok(InputMethod::input_method_directory().join(input_method_name))
    }

    pub fn bundle_id(&self) -> Result<String, InputMethodError> {
        let url = match CFURL::from_path(&self.bundle_path, true) {
            Some(url) => url,
            None => {
                return Err(InputMethodError::InvalidBundle {
                    inner: "Could not get URL for input method bundle".into(),
                });
            },
        };

        let bundle = match CFBundle::new(url) {
            Some(bundle) => bundle,
            None => {
                return Err(InputMethodError::InvalidBundle {
                    inner: format!("Could not load bundle for URL {}", self.bundle_path.display()).into(),
                });
            },
        };

        let identifier = unsafe { CFBundleGetIdentifier(bundle.as_concrete_TypeRef()) };

        if identifier.is_null() {
            return Err(InputMethodError::InvalidBundle {
                inner: "Could find bundle identifier".into(),
            });
        }

        let bundle_identifier = unsafe { CFString::wrap_under_get_rule(identifier) };

        Ok(bundle_identifier.to_string())
    }

    /// Start the input method if it is down, or replace it when the bundle on
    /// disk is no longer the binary we launched. A current helper is left alone:
    /// open Otty / Ghostty / Kitty windows hold IMK connections to that process
    /// and macOS never re-attaches them to a replacement.
    pub fn launch(&self) {
        self.ensure_current_binary_running(&self.bundle_path);
    }

    /// Process ids of every running instance of this input method.
    pub fn running_process_ids(&self) -> Vec<i32> {
        self.running_pids().into_iter().map(Pid::as_raw).collect()
    }

    fn running_pids(&self) -> Vec<Pid> {
        let Ok(bundle_id) = self.bundle_id() else {
            return Vec::new();
        };
        applications::running_application_pids(&bundle_id)
            .into_iter()
            .map(Pid::from_raw)
            .collect()
    }

    fn is_running(&self) -> bool {
        !self.running_pids().is_empty()
    }

    fn executable_path(&self) -> PathBuf {
        self.bundle_path.join("Contents/MacOS/fastab_input_method")
    }

    fn on_disk_code_identity(&self) -> Option<String> {
        code_identity(&self.bundle_path, &self.executable_path())
    }

    fn launched_binary_hash() -> Option<String> {
        state::get_string(LAUNCHED_BINARY_HASH_KEY).ok().flatten()
    }

    fn record_launched_binary(&self) {
        if let Some(identity) = self.on_disk_code_identity() {
            state::set_value(LAUNCHED_BINARY_HASH_KEY, identity).ok();
        }
    }

    /// The process on the machine runs different code than this bundle. A
    /// missing tracker is not stale — see [`process_is_stale`].
    fn running_process_is_stale(&self) -> bool {
        process_is_stale(
            Self::launched_binary_hash().as_deref(),
            self.on_disk_code_identity().as_deref(),
        )
    }

    /// SIGTERM these processes, then SIGKILL whatever is still alive. Returns
    /// only once they are all gone, because `open` on a bundle whose process is
    /// still up just activates that process — launching a replacement before
    /// then would leave the stale helper serving every terminal while we record
    /// the new binary's hash against it.
    fn stop(pids: &[Pid]) {
        for &pid in pids {
            signal::kill(pid, Signal::SIGTERM).ok();
        }
        if wait_for_exit(pids, Duration::from_millis(800)) {
            return;
        }

        info!("Input method ignored SIGTERM; sending SIGKILL");
        for &pid in pids {
            signal::kill(pid, Signal::SIGKILL).ok();
        }
        wait_for_exit(pids, Duration::from_millis(400));
    }

    /// Caller must have established that none of our processes are running, so
    /// that whatever comes up is this binary. The hash is recorded here rather
    /// than after waiting for the process to appear: a launch slower than the
    /// wait would leave the old hash on record, and the next install would then
    /// read a perfectly current helper as stale and kill it.
    fn start_from(&self, bundle: &Path) {
        debug!("Launching input method...");
        match launch_without_parent_app(bundle).status() {
            Ok(status) if status.success() => {},
            Ok(status) => warn!(%status, bundle = %bundle.display(), "open failed to launch the input method"),
            Err(error) => warn!(%error, bundle = %bundle.display(), "Could not run open for the input method"),
        }
        self.record_launched_binary();
    }

    fn ensure_current_binary_running(&self, launch_bundle: &Path) {
        let running = self.running_pids();

        if running.is_empty() {
            self.start_from(launch_bundle);
        } else if self.running_process_is_stale() {
            info!("Input method binary changed; replacing the running process");
            Self::stop(&running);
            self.start_from(launch_bundle);
        } else if Self::launched_binary_hash().is_none() {
            // A helper that predates this tracker. Pin its hash so the next
            // build is recognised as a change instead of staying invisible.
            self.record_launched_binary();
        }
    }
}

impl InputMethod {
    /// Serialize startup recovery with a user-initiated install or uninstall.
    /// Recheck intent after taking the lock so a concurrent uninstall wins.
    pub async fn restore_if_chosen(&self) -> Result<bool> {
        let _operation_guard = operation_lock().await?;
        if !self.should_restore_on_launch()? {
            return Ok(false);
        }
        self.install_locked().await?;
        // The old installer can return Ok while TIS is still registering the
        // helper. Check observable readiness, with a short bounded window for
        // LaunchServices to expose the new process. Keep the restore intent on
        // failure so a later launch can retry.
        let mut status = self.is_installed().await;
        for _ in 0..7 {
            if status.is_ok() {
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            status = self.is_installed().await;
        }
        status.map(|()| true)
    }

    async fn install_locked(&self) -> Result<()> {
        // Persist the choice before changing native integration state. If the
        // database is unavailable, report failure without a partial install.
        Self::set_restore_on_launch(true)?;
        {
            let destination = self.target_bundle_path()?;

            // Only recreate the symlink if it's missing or points to the wrong place.
            // Removing an existing correct symlink breaks the TIS registration.
            let needs_symlink = match fs::read_link(&destination).await {
                Ok(existing) => existing != self.bundle_path,
                Err(_) => true,
            };

            if needs_symlink {
                fs::remove_file(&destination).await.ok();

                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .with_context(|_| format!("Could not create directory {}", parent.display()))?;
                }

                fs::symlink(&self.bundle_path, &destination)
                    .await
                    .with_context(|_| format!("Could not create symlink {}", destination.display()))?;

                // Register with TIS after creating a new symlink
                run_on_main(|| InputMethod::register(&destination))?;
            }

            // Restart only when the on-disk binary is not what we last launched.
            // TIS recognition is not a reason to kill: a CLI process has no
            // NSApplication, so that check is almost always false and used to
            // pkill a healthy IME on every `ftab integrations install`.
            self.ensure_current_binary_running(&destination);

            // TISEnableInputSource opens macOS Keyboard settings for a
            // third-party source. Persist the palette directly during install
            // and launch recovery instead of invoking that UI-opening API.
            let bundle_id = self.bundle_id()?;
            if !force_enable_in_hitoolbox(&bundle_id) {
                return Err(InputMethodError::NotEnabled.into());
            }

            // The IME self-registers ~500 ms after NSApplication starts. Poll
            // briefly; do not sit for 13 s when the source is already there.
            let mut tis_ready = false;
            for attempt in 0..8 {
                let found = run_on_main(|| self.input_source().map(|_| ()));
                match found {
                    Ok(()) => {
                        tis_ready = true;
                        break;
                    },
                    Err(e) => {
                        debug!("Waiting for TIS, attempt {}: {}", attempt + 1, e);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    },
                }
            }

            if tis_ready {
                // select() never triggers a dialog, but it does steal the
                // current Non Keyboard IM. Dual-install: leave Easy Complete
                // (or Sogou, Amazon Q, …) selected when it already is.
                if self.should_select_input_method() {
                    for attempt in 0..5 {
                        let result = run_on_main(|| self.input_source()?.select());
                        match result {
                            Ok(()) => {
                                info!("Input method selected on attempt {}", attempt + 1);
                                break;
                            },
                            Err(e) => {
                                debug!("select() attempt {}: {e}", attempt + 1);
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            },
                        }
                    }
                } else {
                    info!("Leaving the current Non Keyboard IM selected; Fastab stays enabled");
                }
            } else {
                info!("TIS did not recognise the input source yet; IME will register on next launch");
            }
        }

        self.set_is_enabled(true);

        Ok(())
    }
}

#[async_trait]
impl Integration for InputMethod {
    async fn is_installed(&self) -> Result<()> {
        // let attr = fs::metadata(&self.bundle_path)?;
        let destination = self.target_bundle_path()?;

        // check that symlink to input method exists in input_methods_directory
        let symlink = fs::read_link(destination).await;

        match symlink {
            Ok(symlink) => {
                // does it point to the correct location
                if symlink != self.bundle_path {
                    return Err(InputMethodError::InvalidBundle {
                        inner: "Symbolic link is incorrect".into(),
                    }
                    .into());
                }
            },
            Err(err) if err.kind() == ErrorKind::NotFound => return Err(InputMethodError::NotInstalled.into()),
            Err(err) => return Err(err.into()),
        }

        // A CLI process has no NSApplication and often cannot list TIS sources.
        // The running helper registered itself on startup; the persisted
        // enabled palette list tells us whether new terminals may attach.
        // A status query must never rewrite the user's restore preference.
        let bundle_id = self.bundle_id()?;
        running_and_enabled(self.is_running(), is_bundle_in_hitoolbox_enabled(&bundle_id))?;
        Ok(())
    }

    async fn install(&self) -> Result<()> {
        let _operation_guard = operation_lock().await?;
        self.install_locked().await
    }

    async fn uninstall(&self) -> Result<()> {
        let _operation_guard = operation_lock().await?;
        Self::set_restore_on_launch(false)?;
        self.set_is_enabled(false);

        let destination = self.target_bundle_path()?;

        // Best effort. TIS cannot list sources in a process without an
        // NSApplication, which is `ftab integrations uninstall` and so
        // uninstall.sh. A failure here used to return before the palette
        // entries, the process and the symlink were touched, while callers
        // counted `CouldNotListInputSources` as a finished uninstall. The
        // palette write below removes the source from both lists either way.
        let tis = run_on_main(|| {
            let input_source = self.input_source()?;
            // Independent: a source that was never selected still gets disabled.
            let deselected = input_source.deselect();
            input_source.disable().and(deselected)
        });
        if let Err(error) = tis {
            debug!(%error, "TIS could not deselect or disable the input source; removing its palette entries directly");
        }

        // Key-scoped write, same crate as install. Whole-domain `defaults
        // export/import` is what raced the IME during install and can drop
        // every keyboard layout on a failed python3.
        let ours = self.bundle_id().unwrap_or_else(|_| "app.fastab.inputmethod".into());
        let palette_removed = fastab_hitoolbox::remove_palette_entries(&[&ours]);

        // Every instance, and only once they are gone: the old Cocoa
        // `terminate` asked the first one to quit and did not wait.
        Self::stop(&self.running_pids());

        // Already gone counts as removed, so running uninstall again succeeds.
        match fs::remove_file(&destination).await {
            Ok(()) => {},
            Err(error) if error.kind() == ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }

        // Reported last, so a preferences write that did not stick still lets
        // the process and the symlink go.
        if !palette_removed {
            return Err(Error::Custom(
                "Could not remove the input method from the HIToolbox palette lists".into(),
            ));
        }
        Ok(())
    }

    fn describe(&self) -> String {
        "Input Method".into()
    }

    // No `migrate`: `install` already repoints a stale symlink and re-registers
    // it with TIS, and it runs in the same post-install pass. Doing both raced
    // two tasks on the same path under ~/Library/Input Methods.
}

impl InputMethod {
    // Called from separate process in order to check status of Input Method
    pub fn finish_input_method_installation(bundle_path: Option<PathBuf>) -> Result<(), InputMethodError> {
        let input_method = match bundle_path {
            Some(bundle_path) if bundle_path.is_absolute() => InputMethod { bundle_path },
            Some(_) => return Err(InputMethodError::InvalidBundlePath),
            None => InputMethod::default(),
        };

        let source = input_method.input_source()?;

        if !source.is_enabled().unwrap_or_default() {
            return Err(InputMethodError::NotEnabled);
        }

        if source.is_selected().unwrap_or_default() {
            return Ok(());
        }

        // Enabled is what new IME terminals match. Do not steal a sibling
        // palette just to make `is_installed` report selected.
        if !input_method.should_select_input_method() {
            return Ok(());
        }

        source.select()?;

        if !source.is_selected().unwrap_or_default() {
            return Err(InputMethodError::NotSelected);
        }

        Ok(())
    }

    fn should_select_input_method(&self) -> bool {
        let ours = match self.bundle_id() {
            Ok(id) => id,
            Err(_) => return true,
        };
        !fastab_hitoolbox::has_other_selected_non_keyboard(&ours)
    }

    fn input_method_is_enabled_key(&self) -> String {
        let input_method_bundle_id = self.bundle_id().ok().unwrap_or_else(|| "unknown-bundle-id".into());
        format!("input-method={input_method_bundle_id}.enabled")
    }

    pub fn is_enabled(&self) -> Option<bool> {
        self.read_is_enabled().ok().flatten()
    }

    /// Keeps a failed state read apart from a key that was never set.
    pub fn read_is_enabled(&self) -> fastab_settings::Result<Option<bool>> {
        state::get_bool(self.input_method_is_enabled_key())
    }

    fn set_is_enabled(&self, enabled: bool) {
        let key = self.input_method_is_enabled_key();
        state::set_value(key, enabled).ok();
    }
}

/// Whether `bundle_id` is in HIToolbox's `AppleEnabledInputSources`.
/// Selected-only is not a substitute: bounce left the palette selected
/// and disabled, and new IME terminals then attached to nothing.
fn is_bundle_in_hitoolbox_enabled(bundle_id: &str) -> bool {
    fastab_hitoolbox::is_palette_enabled(bundle_id)
}

/// Writes `bundle_id` into both HIToolbox palette lists. The public
/// `TISEnableInputSource` API opens Keyboard settings for third-party sources;
/// writing only `AppleSelectedInputSources` left Otty without a caret.
///
/// Key-scoped, and shared with the IME through `fastab_hitoolbox`. `install.sh`
/// runs this and an IME launch in the same pass, and the whole-domain
/// `defaults export`/`import` this replaced was a read-modify-write over every
/// key in the domain: whichever of the two finished second dropped the other's
/// entry.
fn force_enable_in_hitoolbox(bundle_id: &str) -> bool {
    if fastab_hitoolbox::ensure_palette_enabled(bundle_id) {
        info!("HIToolbox enabled+selected lists confirmed for {bundle_id}");
        true
    } else {
        info!("Could not persist HIToolbox enabled+selected lists for {bundle_id}");
        false
    }
}

/// Returns true if the calling thread is the process main thread.
#[cfg(feature = "dispatch")]
fn is_main_thread() -> bool {
    // `pthread_main_np` lives in libSystem and is always linked on macOS.
    unsafe extern "C" {
        fn pthread_main_np() -> std::os::raw::c_int;
    }
    unsafe { pthread_main_np() != 0 }
}

fn run_on_main<T, F>(work: F) -> T
where
    F: Send + FnOnce() -> T,
    T: Send,
{
    cfg_if::cfg_if! {
        if #[cfg(feature = "dispatch")] {
            // `dispatch_sync` onto the main queue *from the main thread itself* is a
            // deadlock that libdispatch traps with SIGTRAP. This happens in the CLI
            // (`ftab integrations install input-method`), whose work runs on the main
            // thread and where the `dispatch` feature is enabled via workspace feature
            // unification. Run inline in that case; only dispatch when we are on another
            // thread (e.g. fastab_desktop, which has a live main run loop to service it).
            if is_main_thread() {
                work()
            } else {
                dispatch::Queue::main().exec_sync(work)
            }
        } else {
            work()
        }
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    const TEST_INPUT_METHOD_BUNDLE_ID: &str = "com.amazon.inputmethod.codewhisperer";
    const TEST_INPUT_METHOD_BUNDLE_URL: &str = "/Applications/Fastab.app/Contents/Helpers/FastabInputMethod.app";

    fn input_method() -> TISInputSource {
        let key: CFString = unsafe { CFString::wrap_under_create_rule(kTISPropertyBundleID) };
        let value = CFString::from_static_string(TEST_INPUT_METHOD_BUNDLE_ID);
        let properties = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
        let sources = InputMethod::list_all_input_sources(Some(&properties), true).unwrap_or_default();
        sources.into_iter().next().unwrap()
    }

    /// An `NSWorkspace` launch from the desktop made the IME its LaunchServices
    /// child, and loginwindow killed it ~3 s after every desktop quit. A
    /// helper started by `open` has no parent app.
    #[test]
    fn the_ime_is_launched_by_open_so_no_app_owns_it() {
        let command = launch_without_parent_app(Path::new(TEST_INPUT_METHOD_BUNDLE_URL));
        assert_eq!(command.get_program(), "/usr/bin/open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-g", TEST_INPUT_METHOD_BUNDLE_URL]);
    }

    #[ignore]
    #[test]
    fn check_enabled() {
        let method = InputMethod {
            bundle_path: TEST_INPUT_METHOD_BUNDLE_URL.into(),
        };

        println!(
            "{} enabled: {}",
            method.input_source().unwrap().bundle_id().unwrap(),
            method.input_source().unwrap().is_enabled().unwrap()
        );
    }

    #[ignore]
    #[tokio::test]
    async fn install() {
        let method = InputMethod {
            bundle_path: TEST_INPUT_METHOD_BUNDLE_URL.into(),
        };

        let bundle_id = TEST_INPUT_METHOD_BUNDLE_ID;
        match InputMethod::list_input_sources_for_bundle_id(bundle_id) {
            Some(inputs) => {
                println!("Uninstalling...");
                for s in inputs.iter() {
                    println!("{}", s.is_enabled().unwrap_or_default());
                }

                match method.uninstall().await {
                    Ok(_) => println!("Uninstalled!"),
                    Err(e) => println!("{e}"),
                }
            },
            None => {
                println!("No input sources found for {bundle_id}");
                println!("Installing...");
                match method.install().await {
                    Ok(_) => println!("Installed!"),
                    Err(e) => println!("{e}"),
                };
            },
        }
    }

    #[ignore]
    #[test]
    fn toggle_selection() {
        let source = input_method();
        let selected = source.is_selected();
        match selected {
            Some(true) => {
                source.select().ok();
                assert!(source.is_selected().unwrap_or_default());
                source.deselect().ok();
                assert!(!source.is_selected().unwrap_or(true));
                source.select().ok();
                assert!(selected == source.is_selected());
            },
            Some(false) => {
                source.deselect().ok();
                assert!(!source.is_selected().unwrap_or_default());
                source.select().ok();
                assert!(source.is_selected().unwrap_or(false));
                source.deselect().ok();
                assert!(selected == source.is_selected());
            },

            None => unreachable!("Is selected should be defined"),
        }
    }

    #[ignore]
    #[test]
    fn get_input_source_by_bundle_id() {
        let bundle_identifier = TEST_INPUT_METHOD_BUNDLE_ID; //"com.apple.CharacterPaletteIM";
        let sources = InputMethod::list_input_sources_for_bundle_id(bundle_identifier);
        match sources {
            Some(sources) => {
                println!("Found {} matching source", sources.len());
                assert!(sources.len() == 1);
                assert!(sources[0].bundle_id().unwrap() == bundle_identifier);
                assert!(sources[0].category().unwrap() == "TISCategoryPaletteInputSource");

                println!("{:?}", sources[0]);
            },
            None => unreachable!("{} should always exist.", bundle_identifier),
        }
    }

    #[ignore]
    #[test]
    fn uninstall_all() {
        let sources = InputMethod::list_input_sources_for_bundle_id(TEST_INPUT_METHOD_BUNDLE_ID).unwrap_or_default();
        for s in sources.iter() {
            s.deselect().ok();
            s.disable().ok();
        }
    }

    #[ignore]
    #[test]
    fn test_list_all_input_methods() {
        let sources = InputMethod::list_all_input_sources(None, true).unwrap_or_default();

        assert!(!sources.is_empty());
        for source in sources.iter() {
            println!("{source:?}");
        }
    }

    /// The palette write is shared with the IME (`fastab_hitoolbox`) and goes
    /// through CFPreferences. A `python3` spawn here would be both a dependency
    /// on Command Line Tools and a whole-domain read-modify-write, which raced
    /// the IME's own write during an install and dropped one of the two entries.
    #[test]
    fn the_hitoolbox_patch_is_shared_and_spawns_nothing() {
        // Comments stripped: this is about the code, not about prose that names
        // the interpreter being avoided.
        let prod = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!prod.contains("python3"));
        assert!(!prod.contains("plistlib"));
        assert!(prod.contains("fastab_hitoolbox::ensure_palette_enabled"));
        assert!(prod.contains("fastab_hitoolbox::remove_palette_entries"));
        // Reading the enabled list rather than the selected one is what stops a
        // selected-and-disabled leftover from passing for installed.
        assert!(prod.contains("fastab_hitoolbox::is_palette_enabled"));
    }

    /// Every release changed the IME executable's SHA-256 (version bump,
    /// signature), so an update always replaced a helper whose code had not
    /// changed. The digest the build records decides instead.
    #[test]
    fn code_identity_prefers_the_recorded_digest() {
        let dir = std::env::temp_dir().join(format!("fastab-ime-identity-{}", std::process::id()));
        let executable = dir.join("Contents/MacOS/fastab_input_method");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"release one").unwrap();
        let plist = |extra: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
                 <key>CFBundleShortVersionString</key><string>0.0.19</string>{extra}</dict></plist>"
            )
        };

        // A bundle from before the digest keeps the old per-file hash.
        std::fs::write(dir.join("Contents/Info.plist"), plist("")).unwrap();
        assert_eq!(code_identity(&dir, &executable), sha256_hex(&executable));

        std::fs::write(
            dir.join("Contents/Info.plist"),
            plist("<key>FastabCodeDigest</key><string>abc123</string>"),
        )
        .unwrap();
        assert_eq!(code_identity(&dir, &executable).as_deref(), Some("code:abc123"));
        // A rebuilt executable with the same code digest is not stale.
        std::fs::write(&executable, b"release two").unwrap();
        assert_eq!(code_identity(&dir, &executable).as_deref(), Some("code:abc123"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn process_is_stale_only_when_the_hash_changed() {
        assert!(!process_is_stale(None, None));
        assert!(!process_is_stale(Some("aaa"), None));
        assert!(!process_is_stale(None, Some("aaa")));
        assert!(!process_is_stale(Some("aaa"), Some("aaa")));
        assert!(process_is_stale(Some("aaa"), Some("bbb")));
    }

    #[test]
    fn restore_migrates_an_existing_link_but_respects_explicit_uninstall() {
        assert!(!restore_on_launch(None, None, false));
        assert!(restore_on_launch(None, Some(true), false));
        // A failed legacy status probe may already have cleared this bit.
        assert!(restore_on_launch(None, Some(false), true));
        // An uninstall may fail after recording the decision but before the
        // symlink is removed; that stale link must not reinstall the IME.
        assert!(!restore_on_launch(Some(false), Some(true), true));
        assert!(restore_on_launch(Some(true), Some(false), false));
    }

    #[test]
    fn stopped_helper_is_distinct_from_disabled_palette() {
        assert_eq!(running_and_enabled(false, true), Err(InputMethodError::NotRunning));
        assert_eq!(running_and_enabled(true, false), Err(InputMethodError::NotEnabled));
        assert_eq!(running_and_enabled(true, true), Ok(()));
    }

    #[tokio::test]
    async fn operation_lock_serializes_separate_open_handles_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INPUT_METHOD_LOCK_FILE);
        let first = lock_input_method_file(&path).await.unwrap();
        let other = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let (other, _) = Flock::lock(other, FlockArg::LockExclusiveNonblock)
            .expect_err("a separately opened handle must wait for the first operation");

        drop(first);
        let second =
            Flock::lock(other, FlockArg::LockExclusiveNonblock).expect("the file lock is released with its guard");
        drop(second);
    }

    #[test]
    fn operation_lock_waiters_do_not_starve_fs_or_survive_cancellation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(INPUT_METHOD_LOCK_FILE);
            let first = lock_input_method_file(&path).await.unwrap();
            let waiters: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    tokio::spawn(async move { lock_input_method_file(&path).await })
                })
                .collect();

            tokio::time::sleep(Duration::from_millis(250)).await;
            let all_waiting = waiters.iter().all(|waiter| !waiter.is_finished());
            let probe = dir.path().join("fs-probe");
            let probe_result = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::fs::write(&probe, b"ready").await?;
                tokio::fs::read(&probe).await
            })
            .await;

            for waiter in &waiters {
                waiter.abort();
            }
            let mut all_cancelled = true;
            for waiter in waiters {
                all_cancelled &= waiter.await.is_err_and(|error| error.is_cancelled());
            }
            drop(first);
            let second_result = tokio::time::timeout(Duration::from_secs(2), lock_input_method_file(&path)).await;

            assert!(
                all_waiting,
                "all eight tasks should wait while the first guard holds the lock"
            );
            assert_eq!(
                probe_result
                    .expect("lock waiters must not occupy the blocking pool used by Tokio fs")
                    .unwrap()
                    .as_slice(),
                b"ready"
            );
            assert!(all_cancelled, "each pending lock task must be cancellable");
            let second = second_result
                .expect("cancelled waiters must not acquire the lock later")
                .unwrap();
            drop(second);
        });
    }

    #[test]
    fn sha256_hex_is_stable_for_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"ime-binary").unwrap();
        let first = sha256_hex(&path).unwrap();
        let second = sha256_hex(&path).unwrap();
        assert_eq!(first, second);
        std::fs::write(&path, b"ime-binary-v2").unwrap();
        assert_ne!(first, sha256_hex(&path).unwrap());
    }

    #[test]
    fn serialize_deserialize_error() {
        let error = InputMethodError::InvalidBundle {
            inner: "Invalid bundle".into(),
        };
        let serialized = serde_json::to_string(&error).unwrap();
        println!("invalid_bundle: {serialized}");
        let deserialized: InputMethodError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(error, deserialized);

        let error = InputMethodError::UnknownError;
        let serialized = serde_json::to_string(&error).unwrap();
        println!("unknown_error: {serialized}");
        let deserialized: InputMethodError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(error, deserialized);
    }
}
