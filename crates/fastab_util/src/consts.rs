pub const APP_BUNDLE_ID: &str = "app.fastab";
pub const APP_BUNDLE_NAME: &str = "Fastab.app";

#[cfg(target_os = "macos")]
pub const APP_PROCESS_NAME: &str = "fastab";

#[cfg(target_os = "linux")]
pub const APP_PROCESS_NAME: &str = "fastab";

#[cfg(windows)]
pub const APP_PROCESS_NAME: &str = "fastab.exe";

/// The name configured under `"package.productName"` in the tauri.conf.json file.
pub const TAURI_PRODUCT_NAME: &str = "Fastab";

pub const CLI_BINARY_NAME: &str = "ftab";
pub const CLI_BINARY_NAME_MINIMAL: &str = "ftab-minimal";
pub const PTY_BINARY_NAME: &str = "fterm";
/// Compatibility name retained for shell hooks loaded before the PTY rename.
pub const LEGACY_PTY_BINARY_NAME: &str = "fastabterm";

pub const CLI_CRATE_NAME: &str = "fastab_cli";

pub const URL_SCHEMA: &str = "fastab";

pub fn is_product_url_scheme(scheme: &str) -> bool {
    scheme == URL_SCHEMA
}

pub const PRODUCT_NAME: &str = "FasTab";

/// Stable marker written into shell and SSH configuration files. Keep it independent
/// of the display name so existing integrations remain recognizable on uninstall.
pub const INTEGRATION_MARKER_NAME: &str = "Fastab";

pub const RUNTIME_DIR_NAME: &str = "fastabrun";

/// Easy Complete's runtime dir. Dual-install keeps Fastab sockets in
/// [`RUNTIME_DIR_NAME`] and leaves `ecterm` under this name.
pub const PREVIOUS_PRODUCT_RUNTIME_DIR_NAME: &str = "ecrun";

/// Data directory name used in paths like ~/.local/share/{DATA_DIR_NAME}
#[cfg(unix)]
pub const DATA_DIR_NAME: &str = "fastab";
#[cfg(windows)]
pub const DATA_DIR_NAME: &str = "fastab";

/// Backup directory name
pub const BACKUP_DIR_NAME: &str = ".fastab.dotfiles.bak";

pub const GITHUB_REPO_NAME: &str = "codeime/easy-complete";

pub mod url {
    pub const USER_MANUAL: &str = "https://github.com/codeime/easy-complete";
    pub const RELEASE_NOTES: &str = "https://github.com/codeime/easy-complete/releases";
    pub const ISSUE_TRACKER: &str = "https://github.com/codeime/easy-complete/issues/new/choose";
    pub const AUTOCOMPLETE_WIKI: &str = "https://github.com/codeime/easy-complete";
    pub const AUTOCOMPLETE_SSH_WIKI: &str = "https://github.com/codeime/easy-complete";
}

/// Build time env vars
pub mod build {
    /// The target of the current build, e.g. "aarch64-unknown-linux-musl"
    pub const TARGET_TRIPLE: Option<&str> = option_env!("AMAZON_Q_BUILD_TARGET_TRIPLE");

    /// The variant of the current build
    pub const VARIANT: Option<&str> = option_env!("AMAZON_Q_BUILD_VARIANT");

    /// A git full sha hash of the current build
    pub const HASH: Option<&str> = option_env!("AMAZON_Q_BUILD_HASH");

    /// The datetime in rfc3339 format of the current build
    pub const DATETIME: Option<&str> = option_env!("AMAZON_Q_BUILD_DATETIME");

    /// CI run that produced this binary, independent of the release version.
    pub const RUN_ID: Option<&str> = option_env!("FASTAB_BUILD_RUN_ID");

    /// If `fish` tests should be skipped
    pub const SKIP_FISH_TESTS: bool = option_env!("AMAZON_Q_BUILD_SKIP_FISH_TESTS").is_some();

    /// If `shellcheck` tests should be skipped
    pub const SKIP_SHELLCHECK_TESTS: bool = option_env!("AMAZON_Q_BUILD_SKIP_SHELLCHECK_TESTS").is_some();
}

/// macOS specific constants
pub mod macos {
    pub const BUNDLE_CONTENTS_MACOS_PATH: &str = "Contents/MacOS";
    pub const BUNDLE_CONTENTS_RESOURCE_PATH: &str = "Contents/Resources";
    pub const BUNDLE_CONTENTS_HELPERS_PATH: &str = "Contents/Helpers";
    pub const BUNDLE_CONTENTS_INFO_PLIST_PATH: &str = "Contents/Info.plist";

    /// Distributed notification Fastab desktop posts when it wants the IME caret.
    /// Distinct from Easy Complete / Amazon Q (`com.amazon.codewhisperer.edit_buffer_updated`)
    /// so dual-install does not poke the sibling helper.
    pub const EDIT_BUFFER_UPDATED_NOTIFICATION: &str = "app.fastab.edit_buffer_updated";
}

/// Linux-specific constants. `directories.rs` and the leftover desktop Linux
/// branch both import these; they were dropped during the rebrand and the
/// crate no longer compiled on the CI ubuntu job.
pub mod linux {
    pub const DESKTOP_ENTRY_NAME: &str = "fastab.desktop";
    pub const PACKAGE_NAME: &str = "fastab";
    pub const DESKTOP_APP_WM_CLASS: &str = "Fastab";
}

pub mod env_var {
    macro_rules! define_env_vars {
        ($($(#[$meta:meta])* $ident:ident = $name:expr),*) => {
            $(
                $(#[$meta])*
                pub const $ident: &str = $name;
            )*

            pub const ALL: &[&str] = &[$($ident),*];
        }
    }

    define_env_vars! {
        /// The UUID of the current parent qterm instance
        QTERM_SESSION_ID = "QTERM_SESSION_ID",

        /// The current parent socket to connect to
        Q_PARENT = "Q_PARENT",

        /// Set the [`Q_PARENT`] parent socket to connect to
        Q_SET_PARENT = "Q_SET_PARENT",

        /// Guard for the [`Q_SET_PARENT`] check
        Q_SET_PARENT_CHECK = "Q_SET_PARENT_CHECK",

        /// Set if qterm is running, contains the version
        Q_TERM = "Q_TERM",

        /// Sets the current log level
        Q_LOG_LEVEL = "Q_LOG_LEVEL",

        /// Overrides the ZDOTDIR environment variable
        Q_ZDOTDIR = "Q_ZDOTDIR",

        /// Indicates a process was launched by Amazon Q
        PROCESS_LAUNCHED_BY_Q = "PROCESS_LAUNCHED_BY_Q",

        /// The shell to use in qterm
        Q_SHELL = "Q_SHELL",

        /// Indicates the user is debugging the shell
        Q_DEBUG_SHELL = "Q_DEBUG_SHELL",

        /// Overrides the path to the bundle metadata released with certain desktop builds.
        Q_BUNDLE_METADATA_PATH = "Q_BUNDLE_METADATA_PATH"
    }
}

pub mod system_paths {
    /// System installation paths
    pub const APPLICATIONS_DIR: &str = "/Applications";
    pub const USR_LOCAL_BIN: &str = "/usr/local/bin";
    pub const USR_SHARE: &str = "/usr/share";
    pub const OPT_HOMEBREW_BIN: &str = "/opt/homebrew/bin";
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use super::*;

    #[test]
    fn product_url_scheme_is_fastab_only() {
        assert!(is_product_url_scheme("fastab"));
        assert!(!is_product_url_scheme("ec"));
        assert!(!is_product_url_scheme("http"));
        assert!(!is_product_url_scheme("fig"));
    }

    #[test]
    fn edit_buffer_notification_is_fastab_only() {
        let sibling = ["com.amazon.", "codewhisperer", ".edit_buffer_updated"].concat();
        assert_eq!(
            macos::EDIT_BUFFER_UPDATED_NOTIFICATION,
            "app.fastab.edit_buffer_updated"
        );
        assert_ne!(macos::EDIT_BUFFER_UPDATED_NOTIFICATION, sibling.as_str());
    }

    #[test]
    fn test_build_envs() {
        if let Some(build_variant) = build::VARIANT {
            println!("build_variant: {build_variant}");
            assert!(["full", "minimal"].contains(&&*build_variant.to_ascii_lowercase()));
        }

        if let Some(build_hash) = build::HASH {
            println!("build_hash: {build_hash}");
            assert!(!build_hash.is_empty());
        }

        if let Some(build_datetime) = build::DATETIME {
            println!("build_datetime: {build_datetime}");
            println!("{}", OffsetDateTime::parse(build_datetime, &Rfc3339).unwrap());
        }
    }
}
