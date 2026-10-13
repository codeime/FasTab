use accessibility_sys::{AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use core_foundation::base::TCFType;
use core_foundation::boolean::{CFBoolean, kCFBooleanTrue};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use objc2::ClassType;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

pub use crate::accessibility_guide::{accessibility_guide_is_active, begin_accessibility_guide};

static ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility";
static ACCESSIBILITY_SETTINGS_URL_LEGACY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// macOS 27 renamed the Privacy list that holds the Accessibility TCC toggle.
/// The deep-link query stays `Privacy_Accessibility` — that is the TCC service
/// id, not the sidebar label.
pub const ACCESSIBILITY_PANE_RENAME_MAJOR: isize = 27;

pub fn open_accessibility() -> bool {
    let primary = accessibility_settings_url(crate::os::OperatingSystemVersion::get().major());
    let secondary = if primary == ACCESSIBILITY_SETTINGS_URL {
        ACCESSIBILITY_SETTINGS_URL_LEGACY
    } else {
        ACCESSIBILITY_SETTINGS_URL
    };
    open_settings_url(primary) || open_settings_url(secondary)
}

/// Ventura (13) introduced the PrivacySecurity Settings pane. On Monterey the
/// same `x-apple.systempreferences:` scheme still opens, so a successful
/// `openURL` on the new id is not proof the Accessibility list appeared.
pub(crate) fn accessibility_settings_url(major: isize) -> &'static str {
    if major >= 13 {
        ACCESSIBILITY_SETTINGS_URL
    } else {
        ACCESSIBILITY_SETTINGS_URL_LEGACY
    }
}

pub fn accessibility_settings_pane_name(major: isize, zh: bool) -> &'static str {
    if major >= ACCESSIBILITY_PANE_RENAME_MAJOR {
        if zh {
            "设备控制和数据访问"
        } else {
            "Device Control and Data Access"
        }
    } else if zh {
        "辅助功能"
    } else {
        "Accessibility"
    }
}

pub fn accessibility_settings_parent_name(major: isize, zh: bool) -> &'static str {
    if !zh {
        "Privacy & Security"
    } else if major >= ACCESSIBILITY_PANE_RENAME_MAJOR {
        "隐私与安全"
    } else {
        "隐私与安全性"
    }
}

pub fn accessibility_permission_hint(major: isize, zh: bool) -> &'static str {
    match (major >= ACCESSIBILITY_PANE_RENAME_MAJOR, zh) {
        (true, false) => {
            "Required to read the focused terminal window and position completions. Click to open System Settings → Privacy & Security → Device Control and Data Access. A stale list row is removed first, then drag FasTab into the list beside the card. FasTab has its own TCC identity and can sit beside Easy Complete."
        },
        (false, false) => {
            "Required to read the focused terminal window and position completions. Click to open System Settings → Privacy & Security → Accessibility. A stale list row is removed first, then drag FasTab into the list beside the card. FasTab has its own TCC identity and can sit beside Easy Complete."
        },
        (true, true) => {
            "用于读取当前聚焦的终端窗口并定位补全弹窗。点击后打开系统设置 → 隐私与安全 → 设备控制和数据访问；列表里失效的旧条目会先被移除，再把 FasTab 拖进旁边的列表。FasTab 有自己的 TCC 身份，可以和 Easy Complete 并排安装。"
        },
        (false, true) => {
            "用于读取当前聚焦的终端窗口并定位补全弹窗。点击后打开系统设置 → 隐私与安全性 → 辅助功能；列表里失效的旧条目会先被移除，再把 FasTab 拖进旁边的列表。FasTab 有自己的 TCC 身份，可以和 Easy Complete 并排安装。"
        },
    }
}

fn open_settings_url(url: &str) -> bool {
    let string = NSString::from_str(url);
    let Some(nsurl) = (unsafe { NSURL::initWithString(NSURL::alloc(), &string) }) else {
        return false;
    };
    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    unsafe { workspace.openURL(&nsurl) }
}

/// Raises the system TCC sheet. Product flows must not call this — they use
/// [`begin_accessibility_guide`] from the settings Grant button instead.
pub fn prompt_for_accessibility() -> bool {
    unsafe {
        let prompt_key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let prompt_value = CFBoolean::wrap_under_get_rule(kCFBooleanTrue);
        let options = CFDictionary::from_CFType_pairs(&[(prompt_key.as_CFType(), prompt_value.as_CFType())]);

        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    }
}

pub fn accessibility_is_enabled() -> bool {
    unsafe { AXIsProcessTrusted() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monterey_opens_the_legacy_accessibility_pane() {
        assert_eq!(accessibility_settings_url(12), ACCESSIBILITY_SETTINGS_URL_LEGACY);
        assert_eq!(accessibility_settings_url(13), ACCESSIBILITY_SETTINGS_URL);
        assert_eq!(accessibility_settings_url(26), ACCESSIBILITY_SETTINGS_URL);
        assert_eq!(accessibility_settings_url(27), ACCESSIBILITY_SETTINGS_URL);
    }

    #[test]
    fn pane_label_renames_on_macos_27_without_changing_the_url() {
        assert_eq!(accessibility_settings_pane_name(26, false), "Accessibility");
        assert_eq!(
            accessibility_settings_pane_name(27, false),
            "Device Control and Data Access"
        );
        assert_eq!(accessibility_settings_pane_name(26, true), "辅助功能");
        assert_eq!(accessibility_settings_pane_name(27, true), "设备控制和数据访问");
        assert_eq!(accessibility_settings_parent_name(26, true), "隐私与安全性");
        assert_eq!(accessibility_settings_parent_name(27, true), "隐私与安全");
        assert!(accessibility_permission_hint(26, false).contains("→ Accessibility"));
        assert!(accessibility_permission_hint(27, false).contains("→ Device Control and Data Access"));
        assert!(!accessibility_settings_url(27).contains("DeviceControl"));
    }
}
