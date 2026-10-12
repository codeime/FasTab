//! Only changes that affect completion results should restart the engine.
//! The same snapshot handles direct settings writes and their watcher echo.

use fastab_settings::settings::Settings;
use serde_json::Value;

// Keep these aligned with CompleteRequest and the engine's runtime settings.
// Presentation, keybindings and insertion preferences are applied without
// regenerating rows; credentials have their own JevSettingsChanged event.
const COMPLETION_KEYS: &[&str] = &[
    "autocomplete.disable",
    "autocomplete.disableForCommands",
    "autocomplete.fuzzySearch",
    "autocomplete.firstTokenCompletion",
    "autocomplete.onlyShowOnTab",
    "autocomplete.sortMethod",
    "autocomplete.hideAutoExecuteSuggestion",
    "autocomplete.immediatelyRunDangerousCommands",
    "autocomplete.immediatelyExecuteAfterSpace",
    "autocomplete.alwaysSuggestCurrentToken",
    "autocomplete.preferVerboseSuggestions",
    "autocomplete.personalShortcutsToken",
    "autocomplete.scriptTimeout",
    "autocomplete.history.disableLoading",
    "autocomplete.developerMode",
    "autocomplete.developerModeNPM",
    "autocomplete.devCompletionsFolder",
    "beta.history.mode",
    "beta.history.allShells",
    "beta.history.customCommand",
    "beta.autocomplete.auto-cache",
];

#[derive(PartialEq, Eq)]
pub(crate) struct CompletionSettings(Vec<Option<Value>>);

impl CompletionSettings {
    pub(crate) fn capture(settings: &Settings) -> Self {
        Self(
            COMPLETION_KEYS
                .iter()
                .map(|key| settings.get_value(key).ok().flatten())
                .collect(),
        )
    }

    pub(crate) fn refresh(&mut self, settings: &Settings) -> bool {
        let next = Self::capture(settings);
        if *self == next {
            return false;
        }
        *self = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_and_watcher_echo_do_not_restart_completion() {
        let settings = Settings::new_fake();
        let mut applied = CompletionSettings::capture(&settings);
        for (key, value) in [
            ("dashboard.language", Value::from("zh-CN")),
            ("autocomplete.fontFamily", Value::from("Menlo")),
            ("autocomplete.width", Value::from(480)),
            ("app.silentLaunch", Value::from(true)),
            ("autocomplete.insertSpaceAutomatically", Value::from(false)),
        ] {
            settings.set_value(key, value).unwrap();
            assert!(!applied.refresh(&settings), "{key}");
        }
        for (key, value) in [
            ("autocomplete.fuzzySearch", Value::from(false)),
            ("beta.history.mode", Value::from("off")),
            ("autocomplete.sortMethod", Value::from("alphabetical")),
            ("autocomplete.disable", Value::from(true)),
        ] {
            settings.set_value(key, value).unwrap();
            assert!(applied.refresh(&settings), "{key}");
            assert!(!applied.refresh(&settings), "watcher echo: {key}");
            settings.remove_value(key).unwrap();
            assert!(applied.refresh(&settings), "removed: {key}");
            assert!(!applied.refresh(&settings));
        }
    }
}
