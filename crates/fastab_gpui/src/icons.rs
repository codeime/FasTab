//! Named autocomplete icons — the same PNGs the WebView served as `fig://icon?type=…`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{
    AnyElement, Image, ImageFormat, IntoElement, ObjectFit, ParentElement, PathBuilder, SharedString, Styled,
    StyledImage, canvas, div, img, point, px, rgb,
};

// Overlay tiles are 15px. These used to be 512² (~1 MB decoded RGBA each).
const FOLDER: &[u8] = include_bytes!("../../fastab_desktop/icons/autocomplete/folder.png");
const FILE: &[u8] = include_bytes!("../../fastab_desktop/icons/autocomplete/file.png");
const SYMLINK: &[u8] = include_bytes!("../../fastab_desktop/icons/autocomplete/symlink.png");

/// Complete icon catalog copied into `bundle/specs` by the spec sync. Keep
/// this list explicit so every image is compiled into the native overlay and
/// cannot disappear merely because its first use happens after installation.
const BUNDLED_ICONS: &[(&str, &[u8])] = &[
    ("alert", include_bytes!("../../../bundle/specs/icons/alert.png")),
    ("android", include_bytes!("../../../bundle/specs/icons/android.png")),
    ("apple", include_bytes!("../../../bundle/specs/icons/apple.png")),
    ("asterisk", include_bytes!("../../../bundle/specs/icons/asterisk.png")),
    ("aws", include_bytes!("../../../bundle/specs/icons/aws.png")),
    ("azure", include_bytes!("../../../bundle/specs/icons/azure.png")),
    ("box", include_bytes!("../../../bundle/specs/icons/box.png")),
    ("carrot", include_bytes!("../../../bundle/specs/icons/carrot.png")),
    (
        "characters",
        include_bytes!("../../../bundle/specs/icons/characters.png"),
    ),
    ("command", include_bytes!("../../../bundle/specs/icons/command.png")),
    (
        "commandkey",
        include_bytes!("../../../bundle/specs/icons/commandkey.png"),
    ),
    ("commit", include_bytes!("../../../bundle/specs/icons/commit.png")),
    ("cpu", include_bytes!("../../../bundle/specs/icons/cpu.png")),
    ("database", include_bytes!("../../../bundle/specs/icons/database.png")),
    ("discord", include_bytes!("../../../bundle/specs/icons/discord.png")),
    ("docker", include_bytes!("../../../bundle/specs/icons/docker.png")),
    ("firebase", include_bytes!("../../../bundle/specs/icons/firebase.png")),
    ("flag", include_bytes!("../../../bundle/specs/icons/flag.png")),
    ("gcloud", include_bytes!("../../../bundle/specs/icons/gcloud.png")),
    ("gear", include_bytes!("../../../bundle/specs/icons/gear.png")),
    ("git", include_bytes!("../../../bundle/specs/icons/git.png")),
    ("github", include_bytes!("../../../bundle/specs/icons/github.png")),
    ("gitlab", include_bytes!("../../../bundle/specs/icons/gitlab.png")),
    ("gradle", include_bytes!("../../../bundle/specs/icons/gradle.png")),
    ("heroku", include_bytes!("../../../bundle/specs/icons/heroku.png")),
    ("invite", include_bytes!("../../../bundle/specs/icons/invite.png")),
    (
        "kubernetes",
        include_bytes!("../../../bundle/specs/icons/kubernetes.png"),
    ),
    ("netlify", include_bytes!("../../../bundle/specs/icons/netlify.png")),
    ("node", include_bytes!("../../../bundle/specs/icons/node.png")),
    ("npm", include_bytes!("../../../bundle/specs/icons/npm.png")),
    ("okteto", include_bytes!("../../../bundle/specs/icons/okteto.png")),
    ("option", include_bytes!("../../../bundle/specs/icons/option.png")),
    ("package", include_bytes!("../../../bundle/specs/icons/package.png")),
    ("slack", include_bytes!("../../../bundle/specs/icons/slack.png")),
    ("string", include_bytes!("../../../bundle/specs/icons/string.png")),
    ("template", include_bytes!("../../../bundle/specs/icons/template.png")),
    ("twitter", include_bytes!("../../../bundle/specs/icons/twitter.png")),
    ("vercel", include_bytes!("../../../bundle/specs/icons/vercel.png")),
    ("yarn", include_bytes!("../../../bundle/specs/icons/yarn.png")),
];

// Keep the history glyph identical to the WebView's lucide
// `rotate-ccw-clock` icon. A text glyph such as `↺` has font-dependent
// ascender/descender metrics and visibly sits too high/low in a 15px tile.
const HISTORY: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/></svg>"##;

const AI: &str = include_str!("icons/ai.svg");

static NAMED_IMAGE_CACHE: OnceLock<Mutex<HashMap<&'static str, Arc<Image>>>> = OnceLock::new();

fn named_bytes(name: &str) -> Option<&'static [u8]> {
    match name {
        "folder" => Some(FOLDER),
        "file" => Some(FILE),
        "symlink" => Some(SYMLINK),
        "history" => Some(HISTORY),
        "ai" | "ai-black" => Some(AI.as_bytes()),
        _ => BUNDLED_ICONS
            .iter()
            .find_map(|(icon_name, bytes)| (*icon_name == name).then_some(*bytes)),
    }
}

pub fn icon_for_kind(kind: &str) -> SharedString {
    SharedString::from(match kind {
        "subcommand" | "cmd" => "command",
        "option" => "option",
        "folder" | "dir" => "folder",
        "file" => "file",
        "arg" => "box",
        // These kinds normally go through the template-tile path in
        // `list::row_icon`; keep a safe image fallback for callers that only
        // have the named icon API.
        "mixin" => "box",
        "shortcut" => "box",
        "auto-execute" => "carrot",
        "symlink" => "symlink",
        _ => "box",
    })
}

fn cached_image(name: &str) -> Option<Arc<Image>> {
    let key = canonical_icon_name(name)?;
    let cache = NAMED_IMAGE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(image) = map.get(key) {
        return Some(image.clone());
    }
    let bytes = named_bytes(key)?;
    let format = if matches!(key, "history" | "ai" | "ai-black") {
        ImageFormat::Svg
    } else {
        ImageFormat::Png
    };
    // Both AI variants come from one static shape. Recolor only once when the
    // fixed cache entry is created, never for a theme RGB or rendered size.
    let bytes = if key == "ai-black" {
        AI.replace("#fff", "#000").into_bytes()
    } else {
        bytes.to_vec()
    };
    let image = Arc::new(Image::from_bytes(format, bytes));
    map.insert(key, image.clone());
    Some(image)
}

/// Release copied source bytes after the host has retired every GPUI window.
/// The host also clears GPUI's decoded asset cache at that idle boundary.
pub(crate) fn clear_named_icon_cache() {
    let Some(cache) = NAMED_IMAGE_CACHE.get() else {
        return;
    };
    let retired = {
        let mut map = cache.lock().unwrap_or_else(|err| err.into_inner());
        std::mem::take(&mut *map)
    };
    drop(retired);
}

fn canonical_icon_name(name: &str) -> Option<&'static str> {
    let canonical = match name {
        "cmd" | "subcommand" => "command",
        "dir" => "folder",
        "arg" | "mixin" => "box",
        "auto-execute" => "carrot",
        // There is no dedicated pnpm asset in the legacy catalog.
        "pnpm" => "package",
        other => other,
    };
    match canonical {
        "folder" => Some("folder"),
        "file" => Some("file"),
        "symlink" => Some("symlink"),
        "history" => Some("history"),
        "ai" => Some("ai"),
        "ai-black" => Some("ai-black"),
        _ => BUNDLED_ICONS
            .iter()
            .find_map(|(icon_name, _)| (*icon_name == canonical).then_some(*icon_name)),
    }
}

/// Resolve any bundled Fig `fig://icon?type=…` image. Unknown types
/// intentionally return `None`, allowing the row to fall back to its
/// suggestion kind (or an emoji supplied by the spec) instead of displaying a
/// misleading icon.
pub fn icon_type_from_identifier(identifier: &str) -> Option<&str> {
    let query = identifier.strip_prefix("fig://icon?")?;
    let icon_type = query.split('&').find_map(|part| part.strip_prefix("type="))?.trim();
    canonical_icon_name(icon_type)
}

pub fn uri_icon_element(identifier: &str, size: f32) -> Option<gpui::Img> {
    let icon_type = icon_type_from_identifier(identifier)?;
    let image = cached_image(icon_type)?;
    Some(
        img(image)
            .w(px(size))
            .h(px(size))
            .min_w(px(size))
            .min_h(px(size))
            .flex_shrink_0()
            .object_fit(ObjectFit::Contain),
    )
}

pub fn is_short_text_icon(identifier: &str) -> bool {
    !identifier.contains("://") && !identifier.is_empty() && identifier.chars().count() < 4
}

/// Match the renderer's accepted forms without allocating an image or doing I/O.
pub fn identifier_is_renderable(identifier: &str) -> bool {
    icon_type_from_identifier(identifier).is_some()
        || identifier.starts_with("fig://template?")
        || is_short_text_icon(identifier)
}

fn template_icon_parts(identifier: &str) -> Option<(u32, String)> {
    let query = identifier.strip_prefix("fig://template?")?;
    let mut color = 0x628dad;
    let mut badge = String::new();
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("color=") {
            if value.len() == 6 {
                if let Ok(parsed) = u32::from_str_radix(value, 16) {
                    color = parsed;
                }
            }
        } else if let Some(value) = part.strip_prefix("badge=") {
            badge = decode_query_component(value);
        }
    }
    Some((color, badge))
}

fn decode_query_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = bytes[index + 1] as char;
            let low = bytes[index + 2] as char;
            if let (Some(high), Some(low)) = (high.to_digit(16), low.to_digit(16)) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' { b' ' } else { bytes[index] });
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

fn template_icon_element(identifier: &str, size: f32) -> Option<AnyElement> {
    let (color, badge) = template_icon_parts(identifier)?;
    Some(
        div()
            .w(px(size))
            .h(px(size))
            .min_w(px(size))
            .min_h(px(size))
            .flex_shrink_0()
            .rounded(px(size * 0.25))
            .bg(rgb(color))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(size * 0.5))
            .child(badge)
            .into_any_element(),
    )
}

/// Render the common Fig emoji/text icon form using the same two boxes as the
/// WebView: a fixed icon slot and a separately offset glyph. Applying the
/// legacy bottom padding to the fixed slot itself makes flex layout compress
/// the whole slot and visibly moves neighboring content.
pub fn identifier_icon_element(identifier: &str, size: f32) -> Option<AnyElement> {
    if let Some(image) = uri_icon_element(identifier, size) {
        return Some(image.into_any_element());
    }
    if let Some(template) = template_icon_element(identifier, size) {
        return Some(template);
    }
    if !is_short_text_icon(identifier) {
        return None;
    }
    Some(
        div()
            .w(px(size))
            .h(px(size))
            .min_w(px(size))
            .min_h(px(size))
            .flex_shrink_0()
            .flex()
            .child(
                div()
                    // Match the legacy SuggestionIcon span. Both offsets were
                    // rem-based Tailwind utilities (`right-[0.0625rem]` and
                    // `pb-2.5`). The WebView root font size stayed at 16px,
                    // so these remain fixed at 1px and 10px even when the
                    // configured suggestion font/row size changes.
                    .relative()
                    .right(px(1.0))
                    .pb(px(10.0))
                    .text_size(px(size * 0.8))
                    .child(identifier.to_string()),
            )
            .into_any_element(),
    )
}

pub fn named_icon_element(kind: &str, size: f32) -> gpui::Img {
    let key = icon_for_kind(kind);
    let image = cached_image(key.as_ref()).unwrap_or_else(|| cached_image("box").expect("box icon"));
    img(image)
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .object_fit(ObjectFit::Contain)
}

pub fn png_icon_element(image: Arc<gpui::RenderImage>, size: f32) -> gpui::Img {
    img(image)
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .object_fit(ObjectFit::Contain)
}

/// The history tile's inner glyph, sized like the old SVG icon (74% of the
/// 15px icon box). The caller supplies the neutral rounded tile background.
pub fn history_icon_image_element(size: f32) -> gpui::Img {
    let image = cached_image("history").expect("history icon");
    img(image)
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .object_fit(ObjectFit::Contain)
}

/// One primary sparkle and one accent stay legible in the 13–15px icon slot.
/// Recommendations and pending/status indicators share the same gold tile.
/// Native paths avoid GPUI 0.2.2's SVG image RGBA/BGRA channel mismatch.
pub fn ai_icon_element(size: f32) -> gpui::Div {
    div()
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .rounded(px(size * 0.25))
        .bg(rgb(0x342d21))
        .border(px(0.5))
        .border_color(rgb(0x655132))
        .flex()
        .items_center()
        .justify_center()
        .child(
            canvas(
                |_, _, _| (),
                |bounds, _, window, _| {
                    let mut path = PathBuilder::fill();
                    for (x, y, rx, ry) in [(9., 14., 7., 8.), (18., 5.5, 3.5, 3.5)] {
                        let at = |dx: f32, dy: f32| {
                            bounds.origin
                                + point(
                                    bounds.size.width * ((x + dx * rx) / 24.),
                                    bounds.size.height * ((y + dy * ry) / 24.),
                                )
                        };
                        path.move_to(at(0., -1.));
                        path.cubic_bezier_to(at(1., 0.), at(0.18, -0.36), at(0.36, -0.18));
                        path.cubic_bezier_to(at(0., 1.), at(0.36, 0.18), at(0.18, 0.36));
                        path.cubic_bezier_to(at(-1., 0.), at(-0.18, 0.36), at(-0.36, 0.18));
                        path.cubic_bezier_to(at(0., -1.), at(-0.36, -0.18), at(-0.18, -0.36));
                        path.close();
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(0xffd56a));
                    }
                },
            )
            .size(px(size * 0.92)),
        )
}

#[cfg(test)]
mod tests {
    use super::{BUNDLED_ICONS, cached_image, icon_type_from_identifier, is_short_text_icon, template_icon_parts};

    #[test]
    fn parses_fig_icon_type_and_ignores_other_query_parameters() {
        assert_eq!(icon_type_from_identifier("fig://icon?type=folder"), Some("folder"));
        assert_eq!(
            icon_type_from_identifier("fig://icon?color=ff0000&type=command&badge=x"),
            Some("command")
        );
        assert_eq!(icon_type_from_identifier("fig://template?type=folder"), None);
        assert_eq!(icon_type_from_identifier("⭐"), None);
    }

    #[test]
    fn resolves_bundled_product_icons_and_safely_rejects_unknown_types() {
        assert_eq!(icon_type_from_identifier("fig://icon?type=git"), Some("git"));
        assert_eq!(
            icon_type_from_identifier("fig://icon?badge=x&color=fff&type=docker"),
            Some("docker")
        );
        assert_eq!(icon_type_from_identifier("fig://icon?type=github"), Some("github"));
        assert!(cached_image("git").is_some());
        assert!(cached_image("docker").is_some());
        assert_eq!(
            icon_type_from_identifier("fig://icon?color=fff&type=not-a-real-icon"),
            None
        );
        assert!(cached_image("not-a-real-icon").is_none());
    }

    #[test]
    fn overlay_file_icons_stay_small_enough_to_decode_cheaply() {
        for name in ["folder", "file", "symlink"] {
            let bytes = super::named_bytes(name).expect(name);
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{name}");
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert!(width <= 64 && height <= 64, "{name} is {width}x{height}");
        }
        for (name, bytes) in BUNDLED_ICONS {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{name}");
        }
    }

    #[test]
    fn every_bundled_png_is_registered_in_the_native_cache() {
        assert_eq!(BUNDLED_ICONS.len(), 39);
        for (name, bytes) in BUNDLED_ICONS {
            assert!(!bytes.is_empty(), "{name}");
            assert_eq!(
                icon_type_from_identifier(&format!("fig://icon?type={name}")),
                Some(*name)
            );
            assert!(cached_image(name).is_some(), "{name}");
        }
        // Preserve the old aliases while allowing catalog-specific art.
        assert_eq!(
            icon_type_from_identifier("fig://icon?type=commandkey"),
            Some("commandkey")
        );
        assert_eq!(icon_type_from_identifier("fig://icon?type=string"), Some("string"));
        assert_eq!(icon_type_from_identifier("fig://icon?type=npm"), Some("npm"));
        assert_eq!(icon_type_from_identifier("fig://icon?type=yarn"), Some("yarn"));
        assert_eq!(icon_type_from_identifier("fig://icon?type=pnpm"), Some("package"));
    }

    #[test]
    fn recognizes_short_text_icons_without_treating_urls_as_glyphs() {
        assert!(is_short_text_icon("⭐"));
        assert!(is_short_text_icon("ab"));
        assert!(!is_short_text_icon("http"));
        assert!(!is_short_text_icon("https://example.com/icon.png"));
    }

    #[test]
    fn parses_template_tile_color_and_badge() {
        assert_eq!(
            template_icon_parts("fig://template?color=3498db&badge=%F0%9F%92%A1"),
            Some((0x3498db, "💡".into()))
        );
    }
}
