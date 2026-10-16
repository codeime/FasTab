// The fixtures exercise private GPUI paths. GPUI is a vendored dependency,
// so run them through this workspace crate's existing test-support dependency.
use gpui::{TestAppContext, fastab_resource_fixtures as fixtures};

#[cfg(target_os = "macos")]
#[test]
fn copied_input_source_releases_its_owned_reference() {
    fixtures::copied_input_source_is_released_after_metadata_reads_and_failures();
}

#[cfg(target_os = "macos")]
#[test]
fn empty_font_feature_array_has_no_abandoned_reference() {
    fixtures::empty_feature_array_has_no_abandoned_create_retain();
}

#[cfg(target_os = "macos")]
#[test]
fn font_attributes_keep_values_alive_without_extra_array_references() {
    fixtures::feature_and_fallback_arrays_outlive_inputs_without_extra_retains();
}

#[cfg(target_os = "macos")]
#[test]
fn native_font_keeps_requested_features() {
    fixtures::native_font_keeps_requested_features_after_attribute_owners_drop();
}

#[cfg(target_os = "macos")]
#[test]
fn native_layout_scratch_retires_without_changing_fonts_or_layout() {
    fixtures::native_text_scratch_retires_without_changing_fonts_or_layout();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a Metal-capable macOS host"]
fn metal_surface_cache_borrows_the_device() {
    fixtures::surface_cache_creation_borrows_device_on_success_and_error();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a Metal-capable macOS host"]
fn metal_surface_cache_is_lazy_and_releases_its_owner() {
    fixtures::surface_cache_initializes_lazily_reuses_and_releases_its_owner();
}

#[test]
fn arena_retirement_preserves_live_elements() {
    fixtures::retirement_refuses_live_elements_without_dropping_or_invalidating_them();
}

#[test]
fn arena_retirement_releases_backing_and_can_allocate_again() {
    fixtures::retirement_releases_all_chunks_and_metadata_and_reallocates_lazily();
}

#[test]
#[should_panic(expected = "attempted to dereference an ArenaRef after its Arena was cleared")]
fn arena_retirement_cannot_revive_a_stale_box() {
    fixtures::retired_and_reallocated_storage_does_not_revive_an_old_box();
}

#[gpui::test]
fn window_and_entity_lifetimes_retire_only_their_tracking(cx: &mut TestAppContext) {
    fixtures::closing_windows_and_retiring_reads_remove_only_their_entity_tracking(cx);
}

#[gpui::test]
fn idle_release_handles_arena_borrows_and_reopening(cx: &mut TestAppContext) {
    fixtures::idle_cache_release_skips_borrowed_arena_then_retires_and_rebuilds_on_draw(cx);
}

#[gpui::test]
fn idle_release_preserves_another_apps_live_element(cx: &mut TestAppContext) {
    fixtures::idle_app_cannot_retire_another_apps_live_element_on_the_same_thread(cx);
}
