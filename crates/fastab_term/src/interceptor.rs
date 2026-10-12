use anyhow::Result;
use dashmap::DashMap;
use fastab_proto::figterm::Action;
use fastab_settings::keybindings::{KeyBinding, KeyBindings};
use tracing::trace;

use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::ipc::{Generation, RequestOrigin};

// TODO: remove hardcoded list of global actions and use `availability`
const GLOBAL_ACTIONS: &[&str] = &["toggleAutocomplete", "showAutocomplete"];

const IGNORE_ACTION: &str = "ignore";

/// Read this setting at the point where a global Tab is intercepted.
///
/// `fastab_term` is a long-lived process and normally loads the settings file once
/// at startup.  `onlyShowOnTab` is editable from the dashboard while a shell is
/// still running, though, so caching it in a `LazyLock` makes the old setting
/// stick until the terminal is restarted.  Refresh the settings snapshot
/// before reading it; this branch only runs for a Tab while the overlay is
/// hidden and advertises a current input, so it is not on ordinary typing.
fn only_show_on_tab_enabled() -> bool {
    let _ = fastab_settings::settings::init_global();
    fastab_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false)
}

/// Return the private action used for the one-item Tab shortcut.
///
/// This must stay distinct from `showAutocomplete`: the latter is also a user
/// keybinding/action and showing the list must never accept a row as a side
/// effect.  The overlay can therefore preserve the legacy special case only
/// for this action. Check the key before refreshing settings from disk.
fn tab_only_action(key_event: &KeyEvent, read_only_show_on_tab: impl FnOnce() -> bool) -> Option<&'static str> {
    (key_event.key == KeyCode::Tab && key_event.modifiers == Modifiers::NONE && read_only_show_on_tab())
        .then_some("showAutocompleteFromTab")
}

pub fn key_from_text(text: impl AsRef<str>) -> Option<KeyEvent> {
    let text = text.as_ref();

    let mut modifiers = Modifiers::NONE;
    let mut remaining = text;
    let key_txt = loop {
        match remaining.split_once('+') {
            Some(("", "")) | None => {
                break remaining;
            },
            Some((modifier_txt, key)) => {
                modifiers |= match modifier_txt {
                    "ctrl" | "control" => Modifiers::CTRL,
                    "shift" => Modifiers::SHIFT,
                    "alt" | "option" => Modifiers::ALT,
                    "meta" | "command" => Modifiers::META,
                    _ => Modifiers::NONE,
                };
                remaining = key;
            },
        }
    };

    let key = match key_txt {
        "backspace" => KeyCode::Backspace,
        "enter" => KeyCode::Enter,
        "arrowleft" | "left" => KeyCode::LeftArrow,
        "arrowright" | "right" => KeyCode::RightArrow,
        "arrowup" | "up" => KeyCode::UpArrow,
        "arrowdown" | "down" => KeyCode::DownArrow,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "tab" => KeyCode::Tab,
        // "backtab" => KeyCode::BackTab,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "esc" => KeyCode::Escape,
        f_key if f_key.starts_with('f') => {
            let f_key = f_key.trim_start_matches('f');
            let f_key = f_key.parse::<u8>().ok()?;
            KeyCode::Function(f_key)
        },
        c => {
            let mut chars = c.chars();
            let mut first_char = chars.next()?;

            if modifiers.contains(Modifiers::SHIFT) && first_char.is_ascii_lowercase() {
                first_char = first_char.to_ascii_uppercase();
                modifiers.remove(Modifiers::SHIFT);
            }

            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(first_char)
        },
    };

    Some(KeyEvent { key, modifiers })
}

#[derive(Debug, Clone, Default)]
pub struct KeyInterceptor {
    intercept_global: bool,
    intercept: bool,
    intercepts_owner: Option<RequestOrigin>,

    window_visible: bool,
    visibility_owner: Option<RequestOrigin>,

    // TODO: this should be based on `availability`
    _global_actions: Vec<Action>,

    mappings: DashMap<KeyEvent, String, fnv::FnvBuildHasher>,
}

impl KeyInterceptor {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn load_key_intercepts(&mut self) -> Result<()> {
        let key_bindings = KeyBindings::load_hardcoded();
        for KeyBinding { identifier, binding } in key_bindings {
            if let Some(binding) = key_from_text(binding) {
                self.insert_binding(binding, identifier);
            }
        }
        Ok(())
    }

    pub fn set_intercept_global(&mut self, intercept_global: bool) {
        trace!("Setting intercept global to {intercept_global}");
        self.intercept_global = intercept_global;
        self.intercepts_owner = Some(RequestOrigin::Local);
    }

    pub fn set_intercept(&mut self, intercept: bool) {
        trace!("Setting intercept to {intercept}");
        self.intercept = intercept;
        self.intercepts_owner = Some(RequestOrigin::Local);
    }

    pub fn set_window_visible(&mut self, window_visible: bool) {
        self.set_window_visible_from(window_visible, RequestOrigin::Local);
    }

    pub(crate) fn set_window_visible_from(&mut self, window_visible: bool, origin: RequestOrigin) {
        trace!("Setting window visible to {window_visible}");
        self.window_visible = window_visible;
        self.visibility_owner = Some(origin);
    }

    pub(crate) fn set_intercepts_from(&mut self, global: bool, bound: bool, origin: RequestOrigin) {
        self.intercept_global = global;
        self.intercept = bound;
        self.intercepts_owner = Some(origin);
    }

    /// Connection retirement must not clear local IPC state written after the
    /// old remote request. Visibility arrives separately from the flag pair.
    pub(crate) fn retire_remote_except(&mut self, ready: Option<Generation>) {
        if self.intercepts_owner.is_some_and(|origin| !origin.is_current(ready)) {
            self.intercept_global = false;
            self.intercept = false;
            self.intercepts_owner = None;
        }
        if self.visibility_owner.is_some_and(|origin| !origin.is_current(ready)) {
            self.window_visible = false;
            self.visibility_owner = None;
        }
    }

    pub fn set_actions(&mut self, actions: &[Action], override_actions: bool) {
        if override_actions {
            self.mappings.clear();
        }

        for Action { identifier, bindings } in actions {
            for binding in bindings {
                if let Some(binding) = key_from_text(binding) {
                    self.insert_binding(binding, identifier.clone());
                }
            }
        }
    }

    fn insert_binding(&mut self, binding: KeyEvent, identifier: String) {
        if let Some(key) = match binding.key {
            KeyCode::UpArrow => Some(KeyCode::ApplicationUpArrow),
            KeyCode::DownArrow => Some(KeyCode::ApplicationDownArrow),
            KeyCode::LeftArrow => Some(KeyCode::ApplicationLeftArrow),
            KeyCode::RightArrow => Some(KeyCode::ApplicationRightArrow),
            _ => None,
        } {
            self.mappings.insert(
                KeyEvent {
                    key,
                    modifiers: binding.modifiers,
                },
                identifier.clone(),
            );
        };

        if let KeyCode::Char(key) = binding.key {
            // Fill in other case if there is a ctrl or alt, i.e. ctrl+r is the same as ctrl+R
            //
            // This will prevent ctrl+shift+r from being the same as ctrl+r but that is probably
            // fine since we lose context due to parsing ambiguity in the original xterm spec
            // when other modifiers are present
            if (binding.modifiers.contains(Modifiers::CTRL) || binding.modifiers.contains(Modifiers::ALT))
                && key.is_ascii_alphabetic()
            {
                self.mappings.insert(
                    KeyEvent {
                        key: KeyCode::Char(if key.is_ascii_uppercase() {
                            key.to_ascii_lowercase()
                        } else {
                            key.to_ascii_uppercase()
                        }),
                        modifiers: binding.modifiers,
                    },
                    identifier.clone(),
                );
            }
        }

        self.mappings.insert(binding, identifier);
    }

    pub fn reset(&mut self) {
        trace!("Resetting key interceptor");
        self.intercept_global = false;
        self.intercept = false;
        self.intercepts_owner = None;
    }

    pub fn intercept_key(&self, key_event: &KeyEvent) -> Option<String> {
        trace!(?key_event, "Intercepting key");

        match (self.intercept_global, self.intercept) {
            (true, false) => {
                if let Some(action) = tab_only_action(key_event, only_show_on_tab_enabled) {
                    return Some(action.into());
                }

                match self.mappings.get(key_event) {
                    Some(action) if action.value() == IGNORE_ACTION => None,
                    Some(action) if GLOBAL_ACTIONS.contains(&action.value().as_str()) => Some(action.value().clone()),
                    _ => None,
                }
            },
            (_, true) => {
                if self.window_visible {
                    match self.mappings.get(key_event) {
                        Some(action) if action.value() == IGNORE_ACTION => None,
                        Some(action) => Some(action.value().clone()),
                        None => None,
                    }
                } else {
                    None
                }
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_from_text() {
        let assert_key = |text: &str, key, modifiers| {
            assert_eq!(key_from_text(text), Some(KeyEvent { key, modifiers }));
        };

        assert_key("a", KeyCode::Char('a'), Modifiers::NONE);
        assert_key("ctrl+a", KeyCode::Char('a'), Modifiers::CTRL);
        assert_key("ctrl+shift+a", KeyCode::Char('A'), Modifiers::CTRL);
        assert_key("backspace", KeyCode::Backspace, Modifiers::NONE);

        // invalid
        assert_eq!(key_from_text("invalid"), None);
        assert_eq!(key_from_text("ctrl+invalid"), None);
    }

    #[test]
    fn test_key_interceptor() {
        let mut interceptor = KeyInterceptor::new();
        interceptor.load_key_intercepts().unwrap();

        assert_eq!(
            interceptor.intercept_key(&KeyEvent {
                key: KeyCode::Tab,
                modifiers: Modifiers::NONE
            }),
            None
        );

        interceptor.set_intercept(true);
        interceptor.set_window_visible(true);

        assert_eq!(
            interceptor.intercept_key(&KeyEvent {
                key: KeyCode::Tab,
                modifiers: Modifiers::NONE
            }),
            Some("insertCommonPrefixOrInsertSelected".into())
        );
        assert_eq!(
            interceptor.intercept_key(&KeyEvent {
                key: KeyCode::DownArrow,
                modifiers: Modifiers::NONE
            }),
            Some("navigateDown".into())
        );
    }

    #[test]
    fn only_show_on_tab_is_limited_to_a_hidden_tab() {
        let tab = KeyEvent {
            key: KeyCode::Tab,
            modifiers: Modifiers::NONE,
        };
        let enter = KeyEvent {
            key: KeyCode::Enter,
            modifiers: Modifiers::NONE,
        };

        assert_eq!(tab_only_action(&tab, || true), Some("showAutocompleteFromTab"));
        assert_eq!(tab_only_action(&enter, || true), None);
        assert_eq!(tab_only_action(&tab, || false), None);
        assert_eq!(
            tab_only_action(
                &KeyEvent {
                    modifiers: Modifiers::SHIFT,
                    ..tab
                },
                || true
            ),
            None
        );
    }

    #[test]
    fn tab_only_action_reads_live_settings_only_for_unmodified_tab() {
        let reads = std::cell::Cell::new(0);
        let setting = std::cell::Cell::new(true);
        let read_setting = || {
            reads.set(reads.get() + 1);
            setting.get()
        };

        for key in [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::DownArrow,
        ] {
            assert_eq!(
                tab_only_action(
                    &KeyEvent {
                        key,
                        modifiers: Modifiers::NONE,
                    },
                    read_setting
                ),
                None
            );
        }
        for modifiers in [Modifiers::SHIFT, Modifiers::CTRL, Modifiers::ALT, Modifiers::META] {
            assert_eq!(
                tab_only_action(
                    &KeyEvent {
                        key: KeyCode::Tab,
                        modifiers,
                    },
                    read_setting
                ),
                None
            );
        }
        assert_eq!(reads.get(), 0);

        let tab = KeyEvent {
            key: KeyCode::Tab,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(tab_only_action(&tab, read_setting), Some("showAutocompleteFromTab"));
        setting.set(false);
        assert_eq!(tab_only_action(&tab, read_setting), None);
        setting.set(true);
        assert_eq!(tab_only_action(&tab, read_setting), Some("showAutocompleteFromTab"));
        assert_eq!(reads.get(), 3);
    }
}
