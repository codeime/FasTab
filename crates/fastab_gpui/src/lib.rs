//! Shared GPUI overlay window and suggestion list used by the spike binary
//! and by `fastab_desktop`.

mod ai;
mod icons;
mod list;
mod macos;
mod overlay;
mod theme;

#[cfg(test)]
mod idle_cache_tests;
#[cfg(test)]
mod resource_ownership_tests;

/// Install once per application to retire shared UI caches after the last
/// window is destroyed. The overlay's existing hidden grace remains unchanged.
pub fn install_idle_cache_release(cx: &gpui::App) {
    cx.on_window_closed(|cx| {
        // GPUI emits this notification before dropping the removed Window.
        // Defer until its scenes and text buffers have returned their leases,
        // then recheck every window slot in case another window opened meanwhile.
        cx.defer(|cx| {
            if let Some(released) = cx.release_idle_caches() {
                icons::clear_named_icon_cache();
                tracing::debug!(?released, "Released idle UI cache references");
            }
        });
    })
    .detach();
}

pub use ai::AiPreview;
pub use icons::identifier_is_renderable;

pub use list::{
    ClickInsert, DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_ROW_HEIGHT, DEFAULT_WIDTH, DESCRIPTION_HEIGHT,
    DEV_BANNER_HEIGHT, OverlayTheme, POPOUT_WIDTH, SuggestionItem, SuggestionList, TabPrefix, TitleOverflow,
    common_prefix_for, kind_label, layout_gap, layout_pad, longest_common_prefix, match_prefix_bytes,
    overlay_content_size, overlay_content_size_with_context, selection_identity, tab_prefix_insertion,
    unquote_shell_token,
};
pub use macos::{
    OVERLAY_WINDOW_TITLE, harden_overlay_window, harden_overlay_window_handle, harden_overlay_window_titled,
    park_overlay_window_handle, park_overlay_window_titled, polish_overlay_window_titled, quartz_y_to_cocoa_frame_y,
    screens_quartz, set_overlay_frame_handle, set_overlay_frame_titled, set_overlay_visible_handle,
    set_overlay_visible_titled, set_overlay_window_level, set_overlay_window_level_for_title,
    system_appearance_is_dark,
};
pub use overlay::{
    OverlayHandle, OverlayState, VisibleRowsCallback, open_overlay_window, open_overlay_window_with_visibility,
    overlay_window_options, park_overlay_handle, position_overlay,
};
pub use theme::{parse_color, theme_from_json};
