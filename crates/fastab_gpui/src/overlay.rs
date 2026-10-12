use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Bounds, Entity, Pixels, Point, Size, WindowBounds, WindowHandle, WindowOptions, point, px,
    size,
};

use crate::list::{DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_ROW_HEIGHT, DEFAULT_WIDTH, SuggestionList};

use crate::macos::{
    OVERLAY_WINDOW_TITLE, harden_overlay_window_handle, park_overlay_window_handle, try_set_overlay_frame_handle,
};

/// Notification from the laid out suggestion viewport.
pub type VisibleRowsCallback = Arc<dyn Fn(std::ops::Range<usize>, (u64, u64)) + Send + Sync>;

/// Shared overlay entity: visibility, selection, and the suggestion rows.
pub struct OverlayState {
    pub visible: bool,
    pub selected: usize,
    pub items: Vec<crate::list::SuggestionItem>,
    pub search_term: String,
    /// `getQueryTerm` tail used for highlighting (`De` from `~/De`).
    /// Directory-prefix stripping uses the unquoted `search_term` instead:
    /// this field is only the last path segment when the spec splits on `/`.
    pub match_term: String,
    /// The parser's current argument is useful even when it produced no rows.
    /// The WebView kept that context visible as a description-only overlay.
    pub current_arg_name: String,
    pub current_arg_description: String,
    pub theme: crate::list::OverlayTheme,
    pub font_family: String,
    /// When false, suggestion titles use the legacy Monaco fallback while
    /// descriptions keep GPUI's system UI font. A configured family applies
    /// to the whole overlay, matching the old CSS variable behavior.
    pub custom_font_family: bool,
    pub font_size: f32,
    pub row_height: f32,
    pub list_width: f32,
    pub max_list_height: f32,
    pub size_scale: f32,
    /// User-selected default. A spec/argument filterStrategy may override it
    /// for one result without mutating this preference.
    pub fuzzy_search: bool,
    /// Matching mode used to produce and highlight the currently displayed
    /// result. This is reset from `fuzzy_search` when settings are applied.
    pub effective_fuzzy_search: bool,
    pub description_popout: bool,
    pub description_on_left: bool,
    pub is_above_cursor: bool,
    pub always_show_description: bool,
    /// Human-readable binding for the description action. This is supplied by
    /// the desktop layer from the active settings and falls back to `⌃k`.
    pub description_hint: String,
    pub shaking: bool,
    pub loading: bool,
    pub history_mode: bool,
    /// Persisted `beta.history.mode`: `show`, `history_only`, or `off`.
    pub history_setting: String,
    pub only_show_on_tab: bool,
    pub first_token_completion: bool,
    pub scroll_wrap_around: bool,
    pub navigate_to_history: bool,
    pub insert_space_automatically: bool,
    /// How a still-overflowing last path component is shown after the typed
    /// directory has been stripped. Scroll only animates the selected row.
    pub title_overflow: crate::list::TitleOverflow,
    pub show_dev_banner: bool,
    pub suppress_until_shown: bool,
    /// Increments when the displayed suggestion content changes. The native
    /// list uses this alongside selection/count/viewport state so a refreshed
    /// result can repair an old scroll offset without treating mouse-wheel
    /// offset changes as a keyboard-navigation event.
    pub(crate) suggestions_revision: u64,
    /// Set after accepting a completion that does not open a new argument.
    /// The shell emits one buffer update for that insertion; hide that update
    /// so the panel does not immediately pop back over the accepted text.
    pub suppress_next_completion: bool,
    /// A no-op acceptance (the selected value already exactly matches the
    /// current token) can also produce an unchanged shell-buffer update. Tie
    /// that suppression to the exact input so it cannot hide the user's next
    /// real keystroke if the terminal emits no acknowledgement.
    suppress_unchanged_completion: Option<(String, u32)>,
    pub has_changed_index: bool,
    /// Only the selected row's identity survives hidden payload retirement.
    retained_selection: Option<crate::list::SelectionIdentity>,
    pub on_click_insert: Option<Arc<dyn Fn(crate::list::ClickInsert) + Send + Sync>>,
    pub on_disable_dev_mode: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_visible_range: Option<VisibleRowsCallback>,
    icon_result_revision: u64,
    pub ai_preview: Option<crate::AiPreview>,
    /// Changes synchronously with UI content so queued acceptance cannot
    /// target a row from before a local click or refresh.
    pub ai_revision: u64,
    /// Original index of the row temporarily promoted by Jev.
    ai_promoted_from: Option<usize>,
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            visible: false,
            selected: 0,
            items: Vec::new(),
            search_term: String::new(),
            match_term: String::new(),
            current_arg_name: String::new(),
            current_arg_description: String::new(),
            theme: crate::list::OverlayTheme::default(),
            font_family: "Monaco".into(),
            custom_font_family: false,
            font_size: DEFAULT_FONT_SIZE,
            row_height: DEFAULT_ROW_HEIGHT,
            list_width: DEFAULT_WIDTH as f32,
            max_list_height: DEFAULT_MAX_LIST_HEIGHT as f32,
            size_scale: 1.0,
            fuzzy_search: true,
            effective_fuzzy_search: true,
            description_popout: false,
            description_on_left: false,
            is_above_cursor: false,
            always_show_description: false,
            description_hint: "⌃k".into(),
            shaking: false,
            loading: false,
            history_mode: false,
            history_setting: "show".into(),
            only_show_on_tab: false,
            first_token_completion: false,
            scroll_wrap_around: false,
            navigate_to_history: false,
            insert_space_automatically: true,
            title_overflow: crate::list::TitleOverflow::Scroll,
            show_dev_banner: false,
            suppress_until_shown: false,
            suggestions_revision: 0,
            suppress_next_completion: false,
            suppress_unchanged_completion: None,
            has_changed_index: false,
            retained_selection: None,
            on_click_insert: None,
            on_disable_dev_mode: None,
            on_visible_range: None,
            icon_result_revision: 0,
            ai_preview: None,
            ai_revision: 0,
            ai_promoted_from: None,
        }
    }

    pub fn effective_font_size(&self) -> f32 {
        self.font_size * self.size_scale
    }

    pub fn icon_request_revision(&self) -> (u64, u64) {
        (self.icon_result_revision, self.suggestions_revision)
    }

    pub fn effective_row_height(&self) -> f32 {
        self.row_height * self.size_scale
    }

    pub fn effective_list_width(&self) -> f32 {
        // The WebView derived its width from the persisted history setting;
        // toggling the temporary Ctrl-R history view did not resize the card.
        self.list_width * self.size_scale
    }

    pub fn effective_max_list_height(&self) -> f32 {
        self.max_list_height * self.size_scale
    }

    pub fn has_current_arg(&self) -> bool {
        !self.current_arg_name.trim().is_empty() || !self.current_arg_description.trim().is_empty()
    }

    /// Number of rows needed by the description-only state. The old WebView
    /// stacked the argument name and description when both were present.
    pub fn current_arg_rows(&self) -> usize {
        if self.current_arg_name.trim().is_empty() || self.current_arg_description.trim().is_empty() {
            usize::from(self.has_current_arg())
        } else {
            2
        }
    }

    pub fn set_current_arg(&mut self, name: impl Into<String>, description: impl Into<String>) {
        self.current_arg_name = name.into();
        self.current_arg_description = description.into();
    }

    /// Update the placement flags only when they actually changed. The
    /// desktop controller receives caret updates at typing frequency; issuing
    /// a GPUI notification for an identical layout needlessly redraws the
    /// native window.
    pub fn set_layout_flags(&mut self, on_left: bool, is_above: bool) -> bool {
        let changed = self.description_on_left != on_left || self.is_above_cursor != is_above;
        self.description_on_left = on_left;
        self.is_above_cursor = is_above;
        changed
    }

    pub fn set_suggestions(&mut self, items: Vec<crate::list::SuggestionItem>, search_term: String) {
        self.set_suggestions_with_match_term(items, search_term.clone(), search_term);
    }

    pub fn set_suggestions_with_match_term(
        &mut self,
        items: Vec<crate::list::SuggestionItem>,
        search_term: String,
        match_term: String,
    ) {
        self.icon_result_revision = self.icon_result_revision.wrapping_add(1);
        self.invalidate_ai();
        let content_changed = self.items.len() != items.len()
            || self
                .items
                .iter()
                .zip(&items)
                .any(|(old, new)| !crate::list::same_selection_identity(old, new));
        // A new query can legitimately return the same rows in the same
        // order. It is still a new completion context: an old mouse-wheel
        // offset may leave the selected row outside the viewport. Do not
        // advance this revision for an identical query/result refresh, or a
        // wheel-only state change would regain the old snap-back behavior.
        let query_changed = self.search_term != search_term || self.match_term != match_term;
        if content_changed || query_changed {
            self.suggestions_revision = self.suggestions_revision.wrapping_add(1);
        }
        let will_be_visible = !self.suppress_until_shown && (!items.is_empty() || self.has_current_arg());
        let keep = if self.has_changed_index && will_be_visible {
            self.selected_item().map(|selected| {
                items
                    .iter()
                    .position(|item| crate::list::same_selection_identity(item, selected))
            })
        } else {
            None
        };
        let restored = self
            .retained_selection
            .take()
            .and_then(|(name, kind, insert_value, description)| {
                items.iter().position(|item| {
                    item.name == name
                        && item.kind == kind
                        && item.insert_value == insert_value
                        && item.description == description
                })
            });
        if restored.is_some() {
            self.has_changed_index = true;
        }
        let keep = keep.or_else(|| restored.map(Some));
        self.items = items;
        self.search_term = search_term;
        self.match_term = match_term;
        self.loading = false;
        if let Some(index) = keep {
            if let Some(index) = index {
                self.selected = index;
            } else {
                self.selected = 0;
                self.has_changed_index = false;
            }
        } else {
            self.selected = 0;
            if !will_be_visible {
                self.has_changed_index = false;
            }
        }
        self.visible = will_be_visible;
    }

    pub fn hide(&mut self) {
        self.invalidate_ai();
        self.visible = false;
        self.loading = false;
        self.shaking = false;
    }

    pub fn hide_until_shown(&mut self) {
        self.hide();
        self.suppress_until_shown = true;
    }

    pub fn mark_suppress_next_completion(&mut self) {
        self.suppress_next_completion = true;
    }

    pub fn take_suppress_next_completion(&mut self) -> bool {
        let suppress = self.suppress_next_completion;
        self.suppress_next_completion = false;
        suppress
    }

    pub fn mark_suppress_unchanged_completion(&mut self, buffer: String, cursor: u32) {
        self.suppress_unchanged_completion = Some((buffer, cursor));
    }

    /// Consume a post-insertion suppression marker.
    ///
    /// Match the acknowledged **buffer**, not the caret. WebView used
    /// `HIDDEN_UNTIL_KEYPRESS` after a full insert with no new argument and
    /// never compared cursors; figterm/IME often report a different caret on
    /// the same text, which would otherwise re-run complete and show the row
    /// that was just accepted. A different buffer still discards the marker
    /// without suppressing, so a dropped ack cannot hide the next keystroke.
    pub fn take_suppress_completion_for(&mut self, buffer: &str, _cursor: u32) -> bool {
        if self.take_suppress_next_completion() {
            self.suppress_unchanged_completion = None;
            return true;
        }
        self.suppress_unchanged_completion
            .take()
            .is_some_and(|(expected_buffer, _expected_cursor)| expected_buffer == buffer)
    }

    pub fn clear_suggestions(&mut self) {
        self.invalidate_ai();
        self.retained_selection = None;
        if !self.items.is_empty() {
            self.suggestions_revision = self.suggestions_revision.wrapping_add(1);
        }
        self.items.clear();
        self.selected = 0;
        self.search_term.clear();
        self.match_term.clear();
        self.current_arg_name.clear();
        self.current_arg_description.clear();
        self.has_changed_index = false;
    }

    /// Retire hidden rows and their decoded PNGs after the existing grace.
    /// Explicit Show regenerates from the controller's current input snapshot.
    pub fn release_hidden_payload(&mut self) {
        if self.visible {
            return;
        }
        self.invalidate_ai();
        let selection = self
            .has_changed_index
            .then(|| self.selected_item())
            .flatten()
            .map(crate::list::selection_identity)
            .or_else(|| self.retained_selection.take());
        self.clear_suggestions();
        self.retained_selection = selection;
        self.release_empty_capacity();
    }

    /// Return empty storage left by an ended input after the idle grace.
    pub fn release_empty_capacity(&mut self) {
        if self.items.is_empty() {
            self.items.shrink_to_fit();
        }
        for text in [
            &mut self.search_term,
            &mut self.match_term,
            &mut self.current_arg_name,
            &mut self.current_arg_description,
        ] {
            if text.is_empty() {
                text.shrink_to_fit();
            }
        }
    }

    pub fn invalidate_ai(&mut self) {
        self.invalidate_ai_request();
        let Some(from) = self.ai_promoted_from.take() else {
            return;
        };
        if let Some(prefix) = self.items.get_mut(..=from) {
            prefix.rotate_left(1);
            self.selected = if !self.has_changed_index {
                0
            } else if self.selected == 0 {
                from
            } else if self.selected <= from {
                self.selected - 1
            } else {
                self.selected
            };
        }
        self.suggestions_revision = self.suggestions_revision.wrapping_add(1);
    }

    /// Cancel late responses without changing the list the user is navigating.
    pub fn invalidate_ai_request(&mut self) {
        self.ai_preview = None;
        self.ai_revision = self.ai_revision.wrapping_add(1);
    }

    pub fn has_ai_promotion(&self) -> bool {
        self.ai_promoted_from.is_some()
    }

    /// Move one existing local row to the front, retaining all of its metadata.
    pub fn promote_ai_suggestion(&mut self, index: usize) -> bool {
        if index >= self.items.len() || self.has_changed_index || self.has_ai_promotion() {
            return false;
        }
        self.items[..=index].rotate_right(1);
        self.ai_promoted_from = Some(index);
        self.selected = 0;
        self.suggestions_revision = self.suggestions_revision.wrapping_add(1);
        self.ai_preview = None;
        true
    }

    pub fn ai_height(&self) -> f32 {
        if self.loading {
            0.
        } else {
            self.ai_preview
                .as_ref()
                // Pending AI shares the existing description footer. Popout
                // descriptions need their own small footer below the list.
                .filter(|preview| !preview.is_loading() || (self.description_popout && !self.items.is_empty()))
                .map_or(0., |preview| preview.height(self.effective_row_height()))
        }
    }

    pub fn dismiss(&mut self) {
        self.hide();
        self.suppress_until_shown = false;
        self.suppress_next_completion = false;
        self.suppress_unchanged_completion = None;
        self.clear_suggestions();
    }

    pub fn move_selection(&mut self, delta: i32) {
        let _ = self.move_selection_with_wrap(delta, true);
    }

    /// Returns `false` only when wrap is off and the selection would move above
    /// the first row (the previous WebView hid on Up-from-top). Down past the
    /// last row stays on the last item.
    pub fn move_selection_with_wrap(&mut self, delta: i32, wrap: bool) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let len = self.items.len() as i32;
        let next = self.selected as i32 + delta;
        let prev = self.selected;
        if wrap {
            self.selected = next.rem_euclid(len) as usize;
        } else if next < 0 {
            return false;
        } else {
            self.selected = next.min(len - 1) as usize;
        }
        if self.selected != prev {
            self.has_changed_index = true;
        }
        true
    }

    pub fn selected_item(&self) -> Option<&crate::list::SuggestionItem> {
        self.items.get(self.selected)
    }

    pub fn change_size(&mut self, increase: bool) {
        if increase {
            self.size_scale *= 1.1;
        } else {
            self.size_scale = (self.size_scale / 1.1).max(0.5);
        }
    }

    pub fn start_shake(&mut self, cx: &mut gpui::Context<'_, Self>) {
        self.shaking = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            this.update(cx, |overlay, cx| {
                overlay.shaking = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::list::SuggestionItem;

    #[test]
    fn hidden_payload_retires_rows_but_restores_selected_identity() {
        let mut overlay = OverlayState::new();
        let rows = vec![
            SuggestionItem {
                name: "checkout".into(),
                ..Default::default()
            },
            SuggestionItem {
                name: "cherry-pick".into(),
                description: "Apply a commit".into(),
                ..Default::default()
            },
        ];
        overlay.set_suggestions(rows.clone(), "ch".into());
        overlay.move_selection(1);
        overlay.hide_until_shown();
        overlay.release_hidden_payload();
        assert_eq!(overlay.items.capacity(), 0);
        assert_eq!(overlay.search_term.capacity(), 0);
        assert!(overlay.suppress_until_shown);
        overlay.suppress_until_shown = false;
        overlay.set_suggestions(rows, "ch".into());
        assert_eq!(overlay.selected_item().unwrap().name, "cherry-pick");
        assert!(overlay.has_changed_index);
    }

    #[test]
    fn hide_keeps_items_and_esc_suppresses_auto_show() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        overlay.hide();
        assert!(!overlay.visible);
        assert_eq!(overlay.items.len(), 1);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "commit".into(),
                ..SuggestionItem::default()
            }],
            "co".into(),
        );
        assert!(overlay.visible);

        overlay.hide_until_shown();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        assert!(!overlay.visible);
        assert_eq!(overlay.items[0].name, "checkout");
    }

    #[test]
    fn dismiss_clears_items() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        overlay.dismiss();
        assert!(!overlay.visible);
        assert!(overlay.items.is_empty());
        assert!(overlay.search_term.is_empty());
    }

    fn two_items() -> OverlayState {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        overlay
    }

    #[test]
    fn ai_loading_shares_the_footer_unless_descriptions_pop_out() {
        let mut overlay = two_items();
        overlay.ai_preview = Some(crate::AiPreview::Loading(overlay.ai_revision));
        assert_eq!(overlay.ai_height(), 0.);

        overlay.description_popout = true;
        assert_eq!(overlay.ai_height(), overlay.effective_row_height());

        overlay.loading = true;
        assert_eq!(overlay.ai_height(), 0.);
        overlay.loading = false;

        overlay.ai_preview = Some(crate::AiPreview::Status("Request timed out".into()));
        overlay.description_popout = false;
        assert_eq!(overlay.ai_height(), overlay.effective_row_height());
    }

    #[test]
    fn ai_loading_does_not_block_local_selection_and_clears_on_cancel() {
        let mut overlay = two_items();
        let revision = overlay.ai_revision;
        overlay.ai_preview = Some(crate::AiPreview::Loading(overlay.ai_revision));
        assert!(!overlay.loading);
        assert!(overlay.move_selection_with_wrap(1, false));
        assert_eq!(overlay.selected_item().unwrap().name, "commit");
        overlay.invalidate_ai_request();
        assert!(overlay.ai_preview.is_none());
        assert!(overlay.ai_revision > revision);
        assert_eq!(overlay.selected_item().unwrap().name, "commit");
    }

    #[test]
    fn ai_promotion_retires_loading_without_changing_candidate_metadata() {
        let mut overlay = two_items();
        overlay.items[1].description = "Record changes".into();
        overlay.ai_preview = Some(crate::AiPreview::Loading(overlay.ai_revision));
        assert!(overlay.promote_ai_suggestion(1));
        assert!(overlay.ai_preview.is_none());
        let item = overlay.selected_item().unwrap();
        assert_eq!(item.name, "commit");
        assert_eq!(item.description, "Record changes");
    }

    #[test]
    fn down_past_last_item_stays_on_last() {
        let mut overlay = two_items();
        overlay.selected = 1;
        assert!(overlay.move_selection_with_wrap(1, false));
        assert_eq!(overlay.selected, 1);
    }

    #[test]
    fn up_from_first_item_leaves_the_list() {
        let mut overlay = two_items();
        assert!(!overlay.move_selection_with_wrap(-1, false));
        assert_eq!(overlay.selected, 0);
    }

    #[test]
    fn wrap_around_moves_from_last_to_first() {
        let mut overlay = two_items();
        overlay.selected = 1;
        assert!(overlay.move_selection_with_wrap(1, true));
        assert_eq!(overlay.selected, 0);
    }

    #[test]
    fn set_suggestions_keeps_the_manually_selected_row() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items[overlay.selected].name, "commit");
        assert!(overlay.has_changed_index);
    }

    #[test]
    fn set_suggestions_resets_when_the_selected_row_disappears() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn set_suggestions_resets_when_the_user_has_not_moved() {
        let mut overlay = two_items();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items[0].name, "commit");
    }

    #[test]
    fn set_suggestions_does_not_keep_selection_while_hidden() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.hide_until_shown();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert!(!overlay.visible);
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn current_arg_keeps_description_only_overlay_visible() {
        let mut overlay = OverlayState::new();
        overlay.set_current_arg("branch", "The branch name");
        overlay.set_suggestions(Vec::new(), "".into());
        assert!(overlay.visible);
        assert!(overlay.has_current_arg());
        assert_eq!(overlay.current_arg_rows(), 2);
    }

    #[test]
    fn keeps_raw_search_for_deletion_and_normalized_match_term_for_highlighting() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions_with_match_term(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "'che".into(),
            "che".into(),
        );
        assert_eq!(overlay.search_term, "'che");
        assert_eq!(overlay.match_term, "che");
    }

    #[test]
    fn selection_identity_does_not_keep_same_name_with_changed_insert_metadata() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "alias".into(),
                    kind: "option".into(),
                    insert_value: Some("--first".into()),
                    description: "first".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "alias".into(),
                    kind: "option".into(),
                    insert_value: Some("--second".into()),
                    description: "second".into(),
                    ..SuggestionItem::default()
                },
            ],
            "a".into(),
        );
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "alias".into(),
                kind: "option".into(),
                insert_value: Some("--first".into()),
                description: "first".into(),
                ..SuggestionItem::default()
            }],
            "a".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn selection_identity_ignores_presentation_and_query_term_changes() {
        let mut first = SuggestionItem {
            name: "alias".into(),
            kind: "arg".into(),
            display_name: Some("Before".into()),
            query_term: Some("before".into()),
            ..SuggestionItem::default()
        };
        let first_identity = crate::list::selection_identity(&first);
        let before = first.clone();
        first.display_name = Some("After".into());
        first.query_term = Some("after".into());
        assert_eq!(first_identity, crate::list::selection_identity(&first));
        assert!(crate::list::same_selection_identity(&before, &first));
    }

    #[test]
    fn replacing_same_shape_suggestions_advances_scroll_content_revision() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        let first_revision = overlay.suggestions_revision;
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "commit".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            }],
            "co".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items.len(), 1);
        assert!(overlay.suggestions_revision > first_revision);
    }

    #[test]
    fn unchanged_suggestions_do_not_turn_mouse_offset_into_a_new_scroll_event() {
        let mut overlay = OverlayState::new();
        let item = SuggestionItem {
            name: "checkout".into(),
            kind: "subcommand".into(),
            ..SuggestionItem::default()
        };
        overlay.set_suggestions(vec![item.clone()], "ch".into());
        let revision = overlay.suggestions_revision;
        overlay.set_suggestions(vec![item], "ch".into());
        assert_eq!(overlay.suggestions_revision, revision);
    }

    #[test]
    fn changed_query_advances_revision_for_same_rows_and_selection() {
        let mut overlay = OverlayState::new();
        let item = SuggestionItem {
            name: "checkout".into(),
            kind: "subcommand".into(),
            ..SuggestionItem::default()
        };
        overlay.set_suggestions_with_match_term(vec![item.clone()], "c".into(), "c".into());
        let revision = overlay.suggestions_revision;
        overlay.set_suggestions_with_match_term(vec![item], "ch".into(), "ch".into());
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items.len(), 1);
        assert!(overlay.suggestions_revision > revision);
    }

    #[test]
    fn changed_match_term_advances_revision_for_same_raw_query() {
        let mut overlay = OverlayState::new();
        let item = SuggestionItem {
            name: "checkout".into(),
            kind: "subcommand".into(),
            ..SuggestionItem::default()
        };
        overlay.set_suggestions_with_match_term(vec![item.clone()], "scope@c".into(), "c".into());
        let revision = overlay.suggestions_revision;
        overlay.set_suggestions_with_match_term(vec![item], "scope@c".into(), "ch".into());
        assert!(overlay.suggestions_revision > revision);
    }

    #[test]
    fn insertion_suppression_is_one_shot_and_separate_from_hide_until_shown() {
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_next_completion();
        assert!(overlay.take_suppress_completion_for("git status", 10));
        assert!(!overlay.take_suppress_completion_for("git status", 10));
        overlay.hide_until_shown();
        assert!(overlay.suppress_until_shown);
    }

    #[test]
    fn unchanged_insertion_suppression_matches_only_the_acknowledged_buffer() {
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion("git add .".into(), 9);
        assert!(overlay.take_suppress_completion_for("git add .", 9));
        assert!(!overlay.take_suppress_completion_for("git add .", 9));

        overlay.mark_suppress_unchanged_completion("git add .".into(), 9);
        assert!(!overlay.take_suppress_completion_for("git add ./", 10));
        assert!(!overlay.take_suppress_completion_for("git add .", 9));
    }

    #[test]
    fn unchanged_insertion_suppression_ignores_caret_drift_on_the_same_buffer() {
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion("git checkout".into(), 12);
        assert!(overlay.take_suppress_completion_for("git checkout", 8));
        assert!(!overlay.take_suppress_completion_for("git checkout", 12));
    }

    #[test]
    fn layout_flags_report_only_real_changes() {
        let mut overlay = OverlayState::new();
        assert!(!overlay.set_layout_flags(false, false));
        assert!(overlay.set_layout_flags(true, false));
        assert!(!overlay.set_layout_flags(true, false));
        assert!(overlay.set_layout_flags(true, true));
    }

    #[test]
    fn temporary_history_mode_does_not_resize_the_list() {
        let mut overlay = OverlayState::new();
        overlay.list_width = 320.0;
        assert_eq!(overlay.effective_list_width(), 320.0);
        overlay.history_mode = true;
        assert_eq!(overlay.effective_list_width(), 320.0);
    }
}

pub type OverlayHandle = WindowHandle<SuggestionList>;

pub fn overlay_window_options(bounds: Bounds<Pixels>, show: bool) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: false,
        show,
        kind: gpui::WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: gpui::WindowBackgroundAppearance::Transparent,
        window_min_size: Some(size(px(1.), px(1.))),
        ..Default::default()
    }
}

pub fn open_overlay_window(cx: &mut App, state: Entity<OverlayState>) -> anyhow::Result<OverlayHandle> {
    // Create shown so AppKit attaches an `NSWindow`, then `orderOut` like the old
    // tao `set_visible(false)` — keep the last size, do not shrink to 1×1.
    let handle = open_overlay_window_with_visibility(cx, state, true)?;
    park_overlay_handle(&handle, cx)?;
    Ok(handle)
}

pub fn open_overlay_window_with_visibility(
    cx: &mut App,
    state: Entity<OverlayState>,
    show: bool,
) -> anyhow::Result<OverlayHandle> {
    let bounds = Bounds {
        origin: point(px(-10_000.), px(-10_000.)),
        size: size(px(DEFAULT_WIDTH as f32), px(DEFAULT_MAX_LIST_HEIGHT as f32)),
    };
    let handle = cx.open_window(overlay_window_options(bounds, show), |_, cx| {
        cx.new(|_cx| SuggestionList::new(state))
    })?;
    handle
        .update(cx, |_list, window, _cx| {
            window.set_window_title(OVERLAY_WINDOW_TITLE);
            harden_overlay_window_handle(window);
            if !show {
                park_overlay_window_handle(window);
            }
        })
        .map(|_| ())?;
    Ok(handle)
}

pub fn park_overlay_handle(handle: &OverlayHandle, cx: &mut App) -> anyhow::Result<()> {
    handle
        .update(cx, |list, window, _cx| {
            // A hidden window may be shown again at the same size. Make GPUI
            // re-validate its viewport before the native frame request orders
            // the window back to the front.
            list.last_requested_size = None;
            park_overlay_window_handle(window);
        })
        .map(|_| ())
}

pub fn position_overlay(
    origin: Point<Pixels>,
    size: Size<Pixels>,
    handle: &OverlayHandle,
    cx: &mut App,
) -> anyhow::Result<bool> {
    handle.update(cx, |list, window, _cx| {
        // Let GPUI resize its own render surface. Its macOS backend already defers
        // `setContentSize` onto the foreground executor, so the resize callback can
        // update `viewport_size` without re-borrowing `App` from inside our dispatch.
        // Only the position/show part still needs the AppKit bridge below.
        let requested = (f32::from(size.width), f32::from(size.height));
        // A native resize callback can be rejected while GPUI's AppCell is
        // already borrowed. Do not let our request cache turn that one
        // missed callback into a permanently stale renderer: an unchanged
        // requested size is retried whenever the live viewport disagrees.
        if list.last_requested_size != Some(requested) || window.viewport_size() != size {
            window.resize(size);
        }
        list.last_requested_size = Some(requested);
        // Resolve the native window on every layout, even when the geometry is
        // unchanged. A stale GPUI handle may be recreated around this same
        // entity, so an entity-level frame cache can falsely report success for
        // a missing or replacement NSWindow. The AppKit queue deduplicates by
        // both window identity and geometry.
        try_set_overlay_frame_handle(
            window,
            f64::from(origin.x),
            f64::from(origin.y),
            f64::from(size.width),
            f64::from(size.height),
        )
    })
}
