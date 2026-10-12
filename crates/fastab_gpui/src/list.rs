use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, AnyElement, BoxShadow, Context, Entity, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, ListSizingBehavior, ParentElement, Render, RenderImage, Rgba, ScrollHandle,
    ScrollStrategy, StatefulInteractiveElement, Styled, StyledText, UnderlineStyle, UniformListScrollHandle, Window,
    div, hsla, point, px, rgb, uniform_list,
};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use crate::icons::{history_icon_image_element, identifier_icon_element, named_icon_element, png_icon_element};
use crate::overlay::OverlayState;

pub const DEFAULT_FONT_SIZE: f32 = 12.8;
pub const DEFAULT_ROW_HEIGHT: f32 = 20.0;
pub const DESCRIPTION_HEIGHT: f32 = 20.0;
pub const ICON_SIZE: f32 = 15.0;
pub const DEFAULT_WIDTH: f64 = 320.0;
pub const DEFAULT_MAX_LIST_HEIGHT: f64 = 140.0;
pub const POPOUT_WIDTH: f32 = 200.0;
const MAX_ARGS_HINT_ROWS: usize = 3;
/// The legacy overlay set `:root { font-size: var(--font-size, 12.8px) }`, so
/// one CSS rem was the suggestion font size, not the usual 16px. Every Tailwind
/// spacing on the overlay therefore came out 0.8x the value a 16px basis would
/// suggest, and shrank or grew with `autocomplete.fontSize`.
fn rem(font_size: f32, rems: f32) -> f32 {
    font_size * rems
}

/// Transparent inset around the card, legacy `p-1`. Also a positioning
/// constant: the caret offset is applied to the window, so a larger pad
/// visibly pushes the card away from the caret.
pub fn layout_pad(font_size: f32) -> f32 {
    rem(font_size, 0.25)
}

/// Gap between the card and the description popout, legacy `gap-1.5`.
pub fn layout_gap(font_size: f32) -> f32 {
    rem(font_size, 0.375)
}

/// Card corner, legacy `rounded`.
fn card_radius(font_size: f32) -> f32 {
    rem(font_size, 0.25)
}

/// Suggestion row inset, legacy `pl-1.5`. The legacy row had no matching
/// right pad, so the title runs all the way to the card edge before clipping.
fn row_pad_left(font_size: f32) -> f32 {
    rem(font_size, 0.375)
}
pub const DEV_BANNER_HEIGHT: f32 = 88.0;
/// Static loading marker. Keeping this non-animated avoids a perpetual GPUI
/// invalidation loop if the completion worker or IPC stream gets stuck.
pub const LOADING_DOTS: &str = "···";
/// The legacy cards had no layout border; their shadow fits inside the outer
/// transparent padding.
pub const CARD_BORDER: f32 = 0.0;
/// `#description.popout` used a literal `border-radius: 4px`, so unlike the
/// suggestion card it did not scale with the font size.
const POPOUT_RADIUS: f32 = 4.0;
/// CSS `box-shadow` blur maps to twice the gaussian sigma, and GPUI's
/// `blur_radius` *is* that sigma. The legacy `0 0 3px` therefore needs 1.5
/// here; passing 3.0 doubles the halo and reads as a soft grey smear.
const LEGACY_SHADOW_SIGMA: f32 = 1.5;

fn legacy_card_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(0.0, 0.0, 85.0 / 255.0, 1.0),
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(LEGACY_SHADOW_SIGMA),
        spread_radius: px(0.0),
    }]
}

/// Keep the icon box at 75% of the row height, as in the old WebView.
pub fn icon_size_for_row(row_height: f32) -> f32 {
    row_height * (ICON_SIZE / DEFAULT_ROW_HEIGHT)
}

#[derive(Clone, Default)]
pub struct SuggestionItem {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub args_hint: String,
    pub insert_value: Option<String>,
    pub display_name: Option<String>,
    /// Canonical first name from the source suggestion. The legacy recency
    /// index records `name[0]` even when another alias is displayed.
    pub primary_name: Option<String>,
    pub separator_to_add: Option<String>,
    pub should_add_space: bool,
    pub hidden: bool,
    pub priority: i64,
    pub icon_identifier: Option<String>,
    pub original_type: Option<String>,
    /// Per-row query after applying a static `getQueryTerm` rule. This may
    /// differ between generator rows in the same result set.
    pub query_term: Option<String>,
    /// Explicit source signal: argument values never use global recency,
    /// including when an empty cwd prevents a durable scope.
    pub argument_value: bool,
    pub acceptance_scope: Option<String>,
    pub icon_png: Option<Arc<RenderImage>>,
}

/// Match the WebView's `updateSuggestions` identity exactly. Presentation-only
/// fields such as `displayName` and a generator's current query term may
/// change between refreshes without forcing the user's selection back to row
/// zero.
pub type SelectionIdentity = (String, String, Option<String>, String);

pub fn selection_identity(item: &SuggestionItem) -> SelectionIdentity {
    (
        item.name.clone(),
        item.kind.clone(),
        item.insert_value.clone(),
        item.description.clone(),
    )
}

pub(crate) fn same_selection_identity(left: &SuggestionItem, right: &SuggestionItem) -> bool {
    left.name == right.name
        && left.kind == right.kind
        && left.insert_value == right.insert_value
        && left.description == right.description
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClickInsert {
    pub name: String,
    pub description: String,
    pub search: String,
    pub kind: String,
    pub args_hint: String,
    pub insert_value: Option<String>,
    pub display_name: Option<String>,
    pub primary_name: Option<String>,
    pub separator_to_add: Option<String>,
    pub should_add_space: bool,
    pub hidden: bool,
    pub priority: i64,
    pub icon_identifier: Option<String>,
    pub original_type: Option<String>,
    pub query_term: Option<String>,
    pub argument_value: bool,
    pub acceptance_scope: Option<String>,
}

impl std::fmt::Debug for SuggestionItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuggestionItem")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("kind", &self.kind)
            .field("args_hint", &self.args_hint)
            .field("insert_value", &self.insert_value)
            .field("display_name", &self.display_name)
            .field("primary_name", &self.primary_name)
            .field("separator_to_add", &self.separator_to_add)
            .field("should_add_space", &self.should_add_space)
            .field("hidden", &self.hidden)
            .field("priority", &self.priority)
            .field("icon_identifier", &self.icon_identifier)
            .field("original_type", &self.original_type)
            .field("query_term", &self.query_term)
            .field("icon_png", &self.icon_png.is_some())
            .finish()
    }
}

/// Colors match the WebView overlay's `src/index.css`, last present in
/// `packages/autocomplete-app` at v2.2.2.
#[derive(Clone, Copy)]
pub struct OverlayTheme {
    pub background: u32,
    pub border: u32,
    pub text: u32,
    pub muted: u32,
    pub selected: u32,
    pub selected_text: u32,
    pub match_bg: u32,
    pub selected_match_bg: u32,
    pub accent: u32,
}

impl OverlayTheme {
    pub fn dark() -> Self {
        Self {
            background: 0x303030,
            border: 0x414141,
            text: 0xb4b4b4,
            muted: 0xb4b4b4,
            selected: 0x1e5ac7,
            selected_text: 0xfdfdfd,
            match_bg: 0x5f5938,
            selected_match_bg: 0x6a8eda,
            accent: 0x1e5ac7,
        }
    }

    pub fn light() -> Self {
        Self {
            background: 0xfefefe,
            border: 0xc7c7c7,
            text: 0x070707,
            muted: 0x070707,
            selected: 0x2969da,
            selected_text: 0xfdfdfd,
            match_bg: 0xffef98,
            selected_match_bg: 0x6a8eda,
            accent: 0x2969da,
        }
    }
}

impl Default for OverlayTheme {
    fn default() -> Self {
        Self::dark()
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "subcommand" => "sub",
        "option" => "opt",
        "folder" | "dir" => "dir",
        "file" => "file",
        "arg" => "arg",
        "history" => "hist",
        _ => "cmd",
    }
}

/// Byte length of the case-insensitive prefix shared by `name` and `search`.
pub fn match_prefix_bytes(name: &str, search: &str) -> usize {
    if search.is_empty() {
        return 0;
    }
    let mut bytes = 0;
    let mut name_chars = name.chars();
    for needle in search.chars() {
        match name_chars.next() {
            Some(haystack) if haystack.eq_ignore_ascii_case(&needle) => bytes += haystack.len_utf8(),
            _ => return 0,
        }
    }
    bytes
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Text,
    Match,
    Prefix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRun {
    pub text: String,
    pub kind: RunKind,
}

pub fn fuzzy_indexes(name: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let chars: Vec<char> = name.chars().collect();
    let lower: Vec<char> = chars.iter().map(|ch| ch.to_ascii_lowercase()).collect();
    let search: Vec<char> = query.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    let mut indexes = Vec::with_capacity(search.len());
    let mut pos = 0;
    for needle in &search {
        let found = lower
            .iter()
            .enumerate()
            .skip(pos)
            .find(|(_, haystack)| **haystack == *needle)
            .map(|(index, _)| index)?;
        indexes.push(found);
        pos = found + 1;
    }
    // Fuzzysort first finds a simple subsequence, then prefers a strict match
    // that starts at a word boundary and keeps characters consecutive. This
    // matters for repeated letters (`fooBar`/`fb`, `checkout`/`ckt`) because
    // the highlighted indexes are user-visible, not just a ranking detail.
    let beginnings = beginning_indexes(&chars);
    let next_beginning = next_beginning_indexes(chars.len(), &beginnings);
    let mut strict = vec![0usize; search.len()];
    let mut search_i = 0usize;
    let mut target_i = if indexes[0] == 0 {
        0
    } else {
        next_beginning[indexes[0] - 1]
    };
    let mut success = false;
    loop {
        if target_i >= chars.len() {
            if search_i == 0 {
                break;
            }
            // The strict path ran out of target text. Force the previous
            // match forward to the next word boundary and try again, exactly
            // like fuzzysort's small backtracking pass.
            search_i -= 1;
            let last_match = strict[search_i];
            target_i = next_beginning[last_match];
        } else if search[search_i] == lower[target_i] {
            strict[search_i] = target_i;
            search_i += 1;
            if search_i == search.len() {
                success = true;
                break;
            }
            target_i += 1;
        } else {
            let next = next_beginning[target_i];
            if next >= chars.len() {
                target_i = chars.len();
            } else {
                target_i = next;
            }
        }
    }
    if success { Some(strict) } else { Some(indexes) }
}

fn beginning_indexes(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut was_upper = false;
    let mut was_alphanumeric = false;
    for (index, ch) in chars.iter().copied().enumerate() {
        let is_upper = ch.is_ascii_uppercase();
        let is_alphanumeric = ch.is_ascii_alphanumeric();
        if (is_upper && !was_upper) || !was_alphanumeric || !is_alphanumeric {
            out.push(index);
        }
        was_upper = is_upper;
        was_alphanumeric = is_alphanumeric;
    }
    out
}

fn next_beginning_indexes(len: usize, beginnings: &[usize]) -> Vec<usize> {
    let mut out = vec![len; len];
    if beginnings.is_empty() {
        return out;
    }
    let mut beginning_i = 0usize;
    let mut next = beginnings[0];
    for (index, slot) in out.iter_mut().enumerate() {
        if next <= index {
            beginning_i += 1;
            next = beginnings.get(beginning_i).copied().unwrap_or(len);
        }
        *slot = next;
    }
    out
}

pub fn longest_common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = (*first).to_string();
    for name in names.iter().skip(1) {
        while !name.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return prefix;
            }
        }
    }
    prefix
}

/// React-window's `scrollToItem(index, "smart")` keeps an item in place when
/// it is already fully visible, and otherwise moves only the nearest edge into
/// view. GPUI's uniform-list API exposes Top/Center/Bottom strategies but no
/// Smart strategy; choose the matching edge based on the current visible
/// range, leaving a visible selection completely untouched.
pub fn smart_scroll_strategy(selected: usize, first_visible: usize, last_visible: usize) -> Option<ScrollStrategy> {
    if selected < first_visible {
        Some(ScrollStrategy::Top)
    } else if selected > last_visible {
        Some(ScrollStrategy::Bottom)
    } else {
        None
    }
}

/// Visible index span for smart-scroll, from the list's scroll offset.
///
/// `ScrollHandle::{top,bottom}_item` inspect Div `child_bounds`. A
/// `uniform_list` never fills those, so the handle reports `0..=0` (unlaid-out)
/// or a span covering every row. Keyboard Down then never calls
/// `scroll_to_item`, and `cd ~/` looks like five history paths with nothing
/// below. The offset plus the measured viewport is the real range.
pub fn visible_range_for_scroll(offset_y: f32, row_height: f32, visible_rows: usize, count: usize) -> (usize, usize) {
    if count == 0 || visible_rows == 0 {
        return (0, 0);
    }
    let max_index = count.saturating_sub(1);
    let max_span = visible_rows.saturating_sub(1);
    let first = if row_height > 0.0 {
        ((-offset_y) / row_height).floor().max(0.0) as usize
    } else {
        0
    };
    let first = first.min(max_index);
    (first, first.saturating_add(max_span).min(max_index))
}

fn normalize_kind(kind: &str) -> &str {
    match kind {
        "dir" => "folder",
        "cmd" => "subcommand",
        other => other,
    }
}

fn items_match_for_prefix(candidate: &SuggestionItem, selected: &SuggestionItem) -> bool {
    // Keep the old `isMatchingType` asymmetry: a `../` candidate only joins
    // the prefix set when `../` itself is selected. Without this guard the
    // parent-directory row destroys the useful common prefix of ordinary
    // files/folders (for example `src/` and `scripts/`).
    if candidate.name == "../" {
        return selected.name == "../";
    }
    let a = normalize_kind(&candidate.kind);
    let b = normalize_kind(&selected.kind);
    a == b || (is_fileish(a) && is_fileish(b))
}

fn is_fileish(kind: &str) -> bool {
    matches!(kind, "file" | "folder")
}

/// Strip quotes and backslash-escaped spaces so path titles match the unquoted caret token.
pub fn unquote_shell_token(raw: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Single,
        Double,
        AnsiC,
    }

    let mut out = String::with_capacity(raw.len());
    let mut quote: Option<Quote> = None;
    let mut escaped = false;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some(Quote::Single) {
            escaped = true;
            continue;
        }
        // Bash's ANSI-C quote starts with `$'`, but the `$` is syntax rather
        // than part of the token value. `tokenize` applies the same rule when
        // it normalizes the parser token; keep UI matching and insertion on
        // that value too, or a full acceptance can leave a stray `$` behind.
        if quote.is_none() && ch == '$' && chars.peek() == Some(&'\'') {
            chars.next();
            quote = Some(Quote::AnsiC);
            continue;
        }
        if let Some(active) = quote {
            let closer = match active {
                Quote::Single | Quote::AnsiC => '\'',
                Quote::Double => '"',
            };
            if ch == closer {
                quote = None;
            } else {
                out.push(ch);
            }
            continue;
        }
        match ch {
            '\'' => quote = Some(Quote::Single),
            '"' => quote = Some(Quote::Double),
            _ => out.push(ch),
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// Directory already typed in the current token (`src/` from `src/m`).
pub fn typed_directory_prefix(typed: &str) -> &str {
    if typed.ends_with('/') {
        typed
    } else if let Some(slash) = typed.rfind('/') {
        &typed[..=slash]
    } else {
        ""
    }
}

/// Drop the already-typed directory so a long `cd` path shows the last component.
/// `~` is treated as `~/` because the file generator lists `~/Desktop/` while the caret token has no slash.
pub fn strip_typed_directory_prefix<'a>(name: &'a str, typed: &str) -> &'a str {
    let prefix = typed_directory_prefix(typed);
    if !prefix.is_empty()
        && let Some(rest) = name.strip_prefix(prefix).filter(|rest| !rest.is_empty())
    {
        return rest;
    }
    if typed == "~"
        && let Some(rest) = name.strip_prefix("~/").filter(|rest| !rest.is_empty())
    {
        return rest;
    }
    name
}

/// File/folder titles hide the typed directory; other rows keep the full name.
pub fn visible_suggestion_title<'a>(item: &'a SuggestionItem, typed: &str) -> &'a str {
    let title = item.display_name.as_deref().unwrap_or(&item.name);
    if is_fileish(&item.kind) {
        strip_typed_directory_prefix(title, typed)
    } else {
        title
    }
}

/// Residual overflow after the typed directory is hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TitleOverflow {
    /// Clip the end of the last component with an ellipsis.
    Ellipsis,
    /// Marquee the selected row only. Unselected rows still ellipsize.
    /// Default: the last component is what the user is choosing.
    #[default]
    Scroll,
}

impl TitleOverflow {
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("ellipsis") || value.eq_ignore_ascii_case("truncate") {
            Self::Ellipsis
        } else {
            Self::Scroll
        }
    }
}

/// Monaco-ish advance: ASCII ≈ 0.6em, everything else a full em.
pub fn estimated_title_width(text: &str, font_size: f32) -> f32 {
    text.chars()
        .map(|ch| if ch.is_ascii() { 0.6 } else { 1.0 })
        .sum::<f32>()
        * font_size
}

const MARQUEE_PAUSE_SECS: f32 = 0.8;
const MARQUEE_MIN_TRAVEL_SECS: f32 = 1.5;
const MARQUEE_PEAK_SPEED_PX_PER_SEC: f32 = 75.0;

fn marquee_travel_secs(extra: f32) -> f32 {
    // Cosine easing peaks at pi / 2 times the average speed. Keep that peak
    // bounded even for very long titles, instead of capping the whole cycle.
    (extra.max(0.0) * std::f32::consts::FRAC_PI_2 / MARQUEE_PEAK_SPEED_PX_PER_SEC).max(MARQUEE_MIN_TRAVEL_SECS)
}

/// Pause at each end, then ease into and out of each direction. A one-way
/// loop would jump from the tail back to the start every cycle.
pub fn marquee_offset(delta: f32, extra: f32) -> f32 {
    let travel = marquee_travel_secs(extra);
    let cycle = 2.0 * (MARQUEE_PAUSE_SECS + travel);
    let elapsed = delta.clamp(0.0, 1.0) * cycle;
    let half_pause = MARQUEE_PAUSE_SECS * 0.5;
    let ease = |progress: f32| (1.0 - (std::f32::consts::PI * progress.clamp(0.0, 1.0)).cos()) * 0.5;

    if elapsed <= half_pause {
        0.0
    } else if elapsed < half_pause + travel {
        extra * ease((elapsed - half_pause) / travel)
    } else if elapsed <= half_pause + travel + MARQUEE_PAUSE_SECS {
        extra
    } else if elapsed < cycle - half_pause {
        extra * (1.0 - ease((elapsed - half_pause - travel - MARQUEE_PAUSE_SECS) / travel))
    } else {
        0.0
    }
}

pub fn marquee_duration(extra: f32) -> Duration {
    Duration::from_secs_f32(2.0 * (MARQUEE_PAUSE_SECS + marquee_travel_secs(extra)))
}

/// Shared prefix of same-kind rows, matching the old overlay's Tab underline.
/// `typed` is the unquoted full token (quotes and escapes already stripped).
pub fn common_prefix_for(selected: usize, items: &[SuggestionItem], typed: &str) -> String {
    let Some(selected_item) = items.get(selected) else {
        return String::new();
    };
    let mut type_filter = selected_item.clone();
    if selected_item.kind == "auto-execute" {
        type_filter.kind = selected_item.original_type.clone().unwrap_or_default();
        if type_filter.kind == "folder" && !type_filter.name.ends_with('/') {
            type_filter.name.push('/');
        }
    }
    if matches!(type_filter.kind.as_str(), "" | "special" | "auto-execute") {
        return String::new();
    }
    let mut names = items
        .iter()
        .filter(|item| items_match_for_prefix(item, &type_filter))
        .map(|item| visible_suggestion_title(item, typed));
    let Some(first) = names.next() else {
        return String::new();
    };
    let Some(second) = names.next() else {
        return String::new();
    };
    // Only the resulting prefix needs ownership. Re-renders must not copy
    // every candidate title just to decorate the few visible rows.
    let mut prefix = first.to_ascii_lowercase();
    for name in std::iter::once(second).chain(names) {
        let common_bytes = prefix
            .chars()
            .zip(name.chars())
            .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
            .map(|(character, _)| character.len_utf8())
            .sum();
        prefix.truncate(common_bytes);
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabPrefix {
    /// Shared prefix of same-kind, prefix-matching rows (insert like a partial name).
    Partial(String),
    /// Insert the selected row as a full completion.
    Full(String),
}

/// Query used to prefix-match suggestion names for Tab.
///
/// WebView matches against the token's inner text (quotes/escapes stripped).
/// `query_term` is the getQueryTerm tail and wins when it actually prefixes
/// the selected name (`scope@foo` → `foo`). Quoted paths stamp a tail (`fo`
/// from `'src/fo`) that does not prefix `src/foo/`, so fall back to the
/// unquoted full token.
fn tab_prefix_match_search(selected: &SuggestionItem, raw_search: &str) -> String {
    let unquoted_raw = unquote_shell_token(raw_search);
    let query = selected
        .query_term
        .as_deref()
        .map_or_else(|| unquoted_raw.clone(), unquote_shell_token);
    if query.is_empty() || match_prefix_bytes(&selected.name, &query) > 0 {
        query
    } else {
        unquoted_raw
    }
}

/// Tab completion text, matching the old overlay: same-kind rows whose names
/// still prefix-match the query. One match inserts the row; several share a prefix.
pub fn tab_prefix_insertion(selected: usize, items: &[SuggestionItem], search: &str) -> Option<TabPrefix> {
    let selected_item = items.get(selected)?;
    let search = tab_prefix_match_search(selected_item, search);
    // The legacy insertion path accepts the sole row before applying the
    // special/auto-execute common-prefix guard. This matters for generators
    // that directly return one action row; Tab accepts it exactly like a
    // mouse click, while multiple action rows still have no common prefix.
    if items.len() == 1 {
        return Some(TabPrefix::Full(selected_item.name.clone()));
    }
    if matches!(selected_item.kind.as_str(), "special" | "auto-execute") {
        return None;
    }
    let prefix_matches: Vec<&str> = items
        .iter()
        .filter(|item| items_match_for_prefix(item, selected_item))
        .map(|item| item.name.as_str())
        .filter(|name| search.is_empty() || match_prefix_bytes(name, &search) > 0)
        .collect();
    if prefix_matches.len() == 1 {
        return Some(TabPrefix::Full(selected_item.name.clone()));
    }
    if prefix_matches.len() < 2 {
        return None;
    }
    let lower: Vec<String> = prefix_matches.iter().map(|name| name.to_ascii_lowercase()).collect();
    let refs: Vec<&str> = lower.iter().map(String::as_str).collect();
    let lcp = longest_common_prefix(&refs);
    if lcp.is_empty() {
        return None;
    }
    let shared: String = selected_item.name.chars().take(lcp.chars().count()).collect();
    if shared.is_empty() || shared == search {
        return None;
    }
    Some(TabPrefix::Partial(shared))
}

fn merge_runs(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut out: Vec<TextRun> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.kind == run.kind {
                last.text.push_str(&run.text);
                continue;
            }
        }
        out.push(run);
    }
    out
}

pub fn name_runs(name: &str, search: &str, fuzzy: bool, common_prefix: &str) -> Vec<TextRun> {
    let chars: Vec<char> = name.chars().collect();
    let mut runs = if search.is_empty() {
        vec![TextRun {
            text: name.to_string(),
            kind: RunKind::Text,
        }]
    } else if fuzzy {
        match fuzzy_indexes(name, search) {
            Some(indexes) => {
                let marked: std::collections::HashSet<usize> = indexes.into_iter().collect();
                chars
                    .iter()
                    .enumerate()
                    .map(|(index, ch)| TextRun {
                        text: ch.to_string(),
                        kind: if marked.contains(&index) {
                            RunKind::Match
                        } else {
                            RunKind::Text
                        },
                    })
                    .collect()
            },
            None => vec![TextRun {
                text: name.to_string(),
                kind: RunKind::Text,
            }],
        }
    } else if match_prefix_bytes(name, search) > 0 {
        let bytes = match_prefix_bytes(name, search);
        vec![
            TextRun {
                text: name[..bytes].to_string(),
                kind: RunKind::Match,
            },
            TextRun {
                text: name[bytes..].to_string(),
                kind: RunKind::Text,
            },
        ]
    } else {
        vec![TextRun {
            text: name.to_string(),
            kind: RunKind::Text,
        }]
    };
    runs = merge_runs(runs);

    if !common_prefix.is_empty() && common_prefix != "-" {
        let search_len = search.chars().count();
        let prefix_len = common_prefix.chars().count();
        if runs.first().is_some_and(|run| run.kind == RunKind::Text) {
            if let Some(rest) = underline_prefix(&runs, 0, prefix_len) {
                runs = rest;
            }
        } else if runs.len() > 1 && runs[0].kind == RunKind::Match && runs[1].kind == RunKind::Text {
            let remain = prefix_len.saturating_sub(search_len);
            if remain > 0 {
                if let Some(rest) = underline_prefix(&runs, 1, remain) {
                    runs = rest;
                }
            }
        }
        runs = merge_runs(runs);
    }
    runs
}

fn underline_prefix(runs: &[TextRun], index: usize, char_count: usize) -> Option<Vec<TextRun>> {
    if char_count == 0 || index >= runs.len() || runs[index].kind != RunKind::Text {
        return None;
    }
    let text = &runs[index].text;
    let mut bytes = 0;
    for (count, ch) in text.chars().enumerate() {
        if count >= char_count {
            break;
        }
        bytes += ch.len_utf8();
    }
    let bytes = bytes.min(text.len());
    if bytes == 0 {
        return None;
    }
    let prefix = text[..bytes].to_string();
    let rest = text[bytes..].to_string();
    let mut out = Vec::new();
    out.extend_from_slice(&runs[..index]);
    out.push(TextRun {
        text: prefix,
        kind: RunKind::Prefix,
    });
    if !rest.is_empty() {
        out.push(TextRun {
            text: rest,
            kind: RunKind::Text,
        });
    }
    out.extend_from_slice(&runs[index + 1..]);
    Some(out)
}

type ObservedIconViewport = std::rc::Rc<std::cell::RefCell<Option<((u64, u64), Range<usize>)>>>;

pub struct SuggestionList {
    pub state: Entity<OverlayState>,
    scroll_handle: UniformListScrollHandle,
    /// Last `(selected, count, visible_rows, suggestions_revision)` that ran
    /// smart-scroll.
    /// Viewport edges stay out of this tuple: mouse-wheel only changes the
    /// offset, and treating that as a selection change snaps back to row 0.
    scroll_snapshot: Option<(usize, usize, usize, u64)>,
    /// Last size handed to GPUI's native window. Repeating the same resize on
    /// every caret/frame update feeds back into AppKit's resize callbacks.
    pub(crate) last_requested_size: Option<(f32, f32)>,
    last_icon_range: ObservedIconViewport,
}

/// UniformList also calls its row renderer to measure an offscreen sample.
/// Decorations receive only the post-scroll viewport, so measurement must not
/// submit icon work or replace the actual visible batch.
struct VisibleRows {
    callback: Option<crate::overlay::VisibleRowsCallback>,
    revision: (u64, u64),
    previous: ObservedIconViewport,
}

impl gpui::UniformListDecoration for VisibleRows {
    fn compute(
        &self,
        range: Range<usize>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _scroll_offset: gpui::Point<gpui::Pixels>,
        _item_height: gpui::Pixels,
        _item_count: usize,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> AnyElement {
        if let Some(callback) = &self.callback {
            let next = (self.revision, range.clone());
            if self.previous.borrow().as_ref() != Some(&next) {
                *self.previous.borrow_mut() = Some(next);
                callback(range, self.revision);
            }
        }
        gpui::Empty.into_any_element()
    }
}

#[derive(Default)]
struct RowScrollState {
    manual: bool,
    handle: ScrollHandle,
}

impl SuggestionList {
    pub fn new(state: Entity<OverlayState>) -> Self {
        Self {
            state,
            scroll_handle: UniformListScrollHandle::new(),
            scroll_snapshot: None,
            last_requested_size: None,
            last_icon_range: Default::default(),
        }
    }
}

impl Render for SuggestionList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let overlay = self.state.read(cx);
        let has_current_arg = overlay.has_current_arg();
        if !overlay.visible || (overlay.items.is_empty() && !overlay.loading && !has_current_arg) {
            self.scroll_snapshot = None;
            self.last_icon_range.borrow_mut().take();
            return div().id("ec-overlay-hidden").w(px(1.)).h(px(1.));
        }

        let theme = overlay.theme;
        let selected = overlay.selected;
        let count = overlay.items.len();
        let font_family = overlay.font_family.clone();
        let font_size = overlay.effective_font_size();
        let row_height = overlay.effective_row_height();
        let list_width = overlay.effective_list_width();
        let max_list_height = overlay.effective_max_list_height();
        let state = self.state.clone();
        let click = overlay.on_click_insert.clone();
        let visible_range = overlay.on_visible_range.clone();
        let icon_revision = overlay.icon_request_revision();
        let last_icon_range = self.last_icon_range.clone();
        let disable_dev = overlay.on_disable_dev_mode.clone();
        let fuzzy = overlay.effective_fuzzy_search;
        // With no suggestions the WebView renders parser context in the normal
        // description row; a separate popout only exists beside rows.
        let popout = overlay.description_popout && count > 0;
        let on_left = overlay.description_on_left;
        let is_above = overlay.is_above_cursor;
        let shaking = overlay.shaking;
        let loading = overlay.loading;
        let show_hint = !overlay.always_show_description && !loading;
        let show_dev = overlay.show_dev_banner;
        let typed_path = unquote_shell_token(&overlay.search_term);
        let common_prefix = common_prefix_for(selected, &overlay.items, &typed_path);
        // The bottom Description falls back to currentArg, but the old
        // popout deliberately describes only the selected suggestion.
        let selected_item = overlay.selected_item();
        let selected_description = selected_item_description(selected_item);
        let selected_args_hint = selected_item.map_or("", |item| item.args_hint.as_str());
        // The old WebView uses `size.itemSize * 0.75` for both dimensions. Do
        // not clamp this to the default 15px: custom row heights must keep the
        // same icon-to-row ratio as the old overlay.
        let icon_size = icon_size_for_row(row_height);
        let visible_rows = suggestion_visible_rows(count, row_height, max_list_height, popout, loading);
        let list_h = visible_rows as f32 * row_height;
        if count > 0 {
            let snapshot = (selected, count, visible_rows, overlay.suggestions_revision);
            if self.scroll_snapshot != Some(snapshot) {
                let offset_y = f32::from(self.scroll_handle.0.borrow().base_handle.offset().y);
                let (first_visible, last_visible) = visible_range_for_scroll(offset_y, row_height, visible_rows, count);
                if let Some(strategy) = smart_scroll_strategy(selected, first_visible, last_visible) {
                    self.scroll_handle.scroll_to_item(selected, strategy);
                }
                self.scroll_snapshot = Some(snapshot);
            }
        }

        let footer_h = row_height;
        // Built before the rows so they know whether the last one reaches the
        // bottom of the card.
        let footer: Option<AnyElement> = if count == 0 && has_current_arg {
            Some(
                description_only_bar(
                    &overlay.current_arg_name,
                    &overlay.current_arg_description,
                    theme,
                    row_height,
                )
                .into_any_element(),
            )
        } else if !popout {
            Some(
                description_bar(
                    &selected_description,
                    &overlay.current_arg_name,
                    &overlay.current_arg_description,
                    theme,
                    footer_h,
                    show_hint,
                    loading,
                    &overlay.description_hint,
                    overlay.ai_preview.as_ref().filter(|preview| preview.is_loading()),
                )
                .into_any_element(),
            )
        } else if loading && count > 0 {
            Some(loading_icon(theme, footer_h).into_any_element())
        } else {
            None
        };
        let radius = card_radius(font_size);
        let last_row = count.saturating_sub(1);
        let has_footer = footer.is_some() || overlay.ai_preview.is_some();

        let mut column = div()
            .id("ec-list-column")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(list_width))
            .overflow_hidden()
            .rounded(px(card_radius(font_size)))
            .shadow(legacy_card_shadow())
            .bg(rgb(theme.background));
        if count > 0 {
            // A definite-height viewport is load-bearing: Auto-sized
            // `uniform_list` can lay out as tall as every row, so
            // `scroll_to_item` thinks the selection is already on screen and
            // Down never moves. Clip to the measured window; Infer uses that
            // height as available space so the list actually scrolls.
            column = column.child(
                div()
                    .id("ec-suggestions-viewport")
                    .h(px(list_h))
                    .w_full()
                    .overflow_hidden()
                    .child(
                        uniform_list("ec-suggestions", count, {
                            let common_prefix = common_prefix.clone();
                            let typed_path = typed_path.clone();
                            let state = state.clone();
                            let click = click.clone();
                            let suggestion_font_family = font_family.clone();
                            move |range, window, cx| {
                                range
                                    .filter_map(|ix| {
                                        let mut content_key = std::collections::hash_map::DefaultHasher::new();
                                        {
                                            let overlay = state.read(cx);
                                            let item = overlay.items.get(ix)?;
                                            (
                                                ix,
                                                &item.name,
                                                &item.display_name,
                                                &item.args_hint,
                                                overlay.suggestions_revision,
                                            )
                                                .hash(&mut content_key);
                                        }
                                        let content_key = content_key.finish();
                                        let row_scroll =
                                            window.use_keyed_state(("ec-row-scroll-state", content_key), cx, |_, _| {
                                                RowScrollState::default()
                                            });
                                        let overlay = state.read(cx);
                                        let item = overlay.items.get(ix)?;
                                        let search_term = item.query_term.as_deref().unwrap_or(&overlay.match_term);
                                        let corners = row_corner_radii(ix, last_row, radius, has_footer);
                                        Some(suggestion_row(
                                            item,
                                            window,
                                            cx,
                                            row_scroll,
                                            content_key,
                                            ix == overlay.selected,
                                            ix == 0 && overlay.has_ai_promotion(),
                                            search_term,
                                            &typed_path,
                                            &overlay.search_term,
                                            fuzzy,
                                            &common_prefix,
                                            theme,
                                            row_height,
                                            icon_size,
                                            font_size,
                                            list_width,
                                            overlay.title_overflow,
                                            corners,
                                            suggestion_font_family.clone(),
                                            state.clone(),
                                            click.clone(),
                                            ix,
                                        ))
                                    })
                                    .collect()
                            }
                        })
                        .with_decoration(VisibleRows {
                            callback: visible_range,
                            revision: icon_revision,
                            previous: last_icon_range,
                        })
                        .map(|mut list| {
                            list.style().restrict_scroll_to_axis = Some(true);
                            list
                        })
                        .with_sizing_behavior(ListSizingBehavior::Infer)
                        .track_scroll(self.scroll_handle.clone())
                        .h(px(list_h)),
                    ),
            );
        }
        if let Some(footer) = footer {
            column = column.child(footer);
        }
        if let Some(preview) = overlay
            .ai_preview
            .as_ref()
            .filter(|preview| !preview.is_loading() || popout)
        {
            column = column.child(crate::ai::preview(preview, theme, row_height));
        }
        let list_column = div().when(shaking, |this| this.ml(px(3.))).child(column);

        let mut body = div()
            .id("ec-overlay-body")
            .flex()
            .flex_row()
            .items_start()
            .when(is_above, |this| this.items_end())
            .gap(px(layout_gap(font_size)))
            .p(px(layout_pad(font_size)));
        if popout && on_left {
            body = body.child(description_popout(
                &selected_description,
                selected_args_hint,
                theme,
                max_list_height,
                selected_args_popout_height(selected_args_hint, font_size, row_height, max_list_height),
                show_hint,
                &overlay.description_hint,
                selected,
                overlay.suggestions_revision,
            ));
        }
        body = body.child(list_column);
        if popout && !on_left {
            body = body.child(description_popout(
                &selected_description,
                selected_args_hint,
                theme,
                max_list_height,
                selected_args_popout_height(selected_args_hint, font_size, row_height, max_list_height),
                show_hint,
                &overlay.description_hint,
                selected,
                overlay.suggestions_revision,
            ));
        }

        let list_width_for_banner = list_width;

        div()
            .id("ec-overlay")
            .flex()
            .flex_col()
            .font_family(font_family)
            .text_size(px(font_size))
            .overflow_hidden()
            .w_full()
            .h_full()
            .when(!loading && is_above && show_dev, {
                let disable_dev = disable_dev.clone();
                let state = state.clone();
                move |this| this.child(dev_banner(list_width_for_banner, state, disable_dev))
            })
            .child(if loading {
                loading_icon(theme, row_height).into_any_element()
            } else {
                body.into_any_element()
            })
            .when(!loading && !is_above && show_dev, |this| {
                this.child(dev_banner(list_width_for_banner, state, disable_dev))
            })
    }
}

#[allow(clippy::too_many_arguments)]
fn suggestion_row(
    item: &SuggestionItem,
    window: &Window,
    cx: &gpui::App,
    row_scroll: Entity<RowScrollState>,
    content_key: u64,
    is_selected: bool,
    is_ai_promoted: bool,
    search_term: &str,
    typed: &str,
    insertion_search_term: &str,
    fuzzy: bool,
    common_prefix: &str,
    theme: OverlayTheme,
    row_height: f32,
    icon_size: f32,
    font_size: f32,
    list_width: f32,
    title_overflow: TitleOverflow,
    // Top and bottom radii this row must reproduce to keep the card's corners
    // intact, since GPUI's overflow clip does not follow a border radius.
    corners: (f32, f32),
    font_family: String,
    state: Entity<OverlayState>,
    click: Option<Arc<dyn Fn(ClickInsert) + Send + Sync>>,
    ix: usize,
) -> AnyElement {
    // The WebView used brightness filters on the selected row and again on
    // match marks, with a brighter child span. GPUI has no CSS filter stack,
    // so apply the equivalent channel multipliers to the affected colors.
    let row_brightness = if is_selected { 0.95 } else { 1.0 };
    // The legacy row only set a background when active; an unselected row let
    // the card show through, which is also what keeps the rounded corners.
    let bg = is_selected.then(|| brightness(rgb(theme.selected), row_brightness));
    let text = brightness(
        if is_selected {
            rgb(theme.selected_text)
        } else {
            rgb(theme.text)
        },
        row_brightness,
    );
    let mut match_bg = brightness(
        if is_selected {
            rgb(theme.selected_match_bg)
        } else {
            rgb(theme.match_bg)
        },
        row_brightness * 0.95,
    );
    match_bg.a = 0.8;
    let match_text = brightness(text, 0.95 * 1.25);
    let click_item = item.clone();
    let title_name = visible_suggestion_title(item, typed);
    let highlight = if is_fileish(&item.kind) {
        strip_typed_directory_prefix(search_term, typed)
    } else {
        search_term
    };
    let runs = name_runs(title_name, highlight, fuzzy, common_prefix);
    let insertion_search_term = insertion_search_term.to_string();
    let (title_text, title_highlights) = suggestion_title_highlights(&runs, text, match_text, match_bg);
    let available = (list_width - row_pad_left(font_size) - icon_size - 5.0).max(1.0);
    let title_style = gpui::TextStyle {
        font_family: font_family.clone().into(),
        ..Default::default()
    };
    let measured_width = |value: &str| {
        f32::from(
            window
                .text_system()
                .layout_line(value, px(font_size), &[title_style.to_run(value.len())], None)
                .width,
        )
    };
    let name_width = measured_width(&title_text);
    let hint_width = measured_width(&item.args_hint);
    let show_hint = !item.args_hint.is_empty();
    let content_width = name_width + if show_hint { 5.0 + hint_width } else { 0.0 };
    let overflows = content_width > available;
    let manual_scroll = row_scroll.read(cx).manual;
    let extra = name_width - available;
    let marquee = is_selected && title_overflow == TitleOverflow::Scroll && extra > 4.0;
    let styled = StyledText::new(title_text).with_highlights(title_highlights);
    // Preserve the initial overflow presentation, then reveal the full title
    // on manual scroll. The outer name slot always reserves the complete width
    // so the very first wheel event can move, including when args_hint is empty.
    let title = if show_hint || manual_scroll {
        div()
            .w(px(name_width))
            .flex_none()
            .whitespace_nowrap()
            .font_family(font_family)
            .text_color(text)
            .child(styled)
    } else if marquee {
        let duration = marquee_duration(extra);
        div()
            .min_w(px(0.))
            .flex_none()
            .max_w(px(available))
            .overflow_hidden()
            .font_family(font_family)
            .text_color(text)
            .child(div().whitespace_nowrap().child(styled).with_animation(
                ("ec-marquee", ix),
                Animation::new(duration).repeat(),
                move |this, delta| this.ml(px(-marquee_offset(delta, extra))),
            ))
    } else {
        div()
            .min_w(px(0.))
            .flex_none()
            .max_w(px(available))
            .truncate()
            .font_family(font_family)
            .text_color(text)
            .child(styled)
    };
    let title = div().w(px(name_width)).flex_none().child(title);
    let mut hint_color = text;
    hint_color.a *= 0.55;
    let scroll_state = state.clone();
    let mut title_content = div()
        .id(("ec-suggestion-content", content_key))
        .min_w(px(0.))
        .w(px(available))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .when(overflows, |this| this.overflow_x_scroll())
        .track_scroll(&row_scroll.read(cx).handle)
        .on_scroll_wheel(move |event, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height());
            if !overflows || !f32::from(delta.x).is_finite() || delta.x == px(0.) {
                return;
            }
            // GPUI applies the delta before this callback and clamps it during
            // the next layout. An outward gesture at either edge moves nothing.
            let scroll = row_scroll.read(cx);
            let offset = scroll.handle.offset().x;
            let min = -scroll.handle.max_offset().width;
            let moved = offset.clamp(min, px(0.)) != (offset - delta.x).clamp(min, px(0.));
            if moved {
                if !scroll.manual {
                    row_scroll.update(cx, |scroll, cx| {
                        scroll.manual = true;
                        cx.notify();
                    });
                }
                scroll_state.update(cx, |overlay, cx| {
                    // Browsing a row is interaction too: keep the visible AI
                    // promotion, but reject pending recommendations and do not
                    // change which item Enter/Tab would accept.
                    overlay.invalidate_ai_request();
                    cx.notify();
                });
            }
            // The inner native scroller runs first in GPUI's bubble phase.
            // Keep a mostly horizontal gesture from also moving the list.
            if delta.x.abs() > delta.y.abs() {
                cx.stop_propagation();
            }
        })
        .child(title);
    // Neither axis should be remapped: horizontal movement reveals text,
    // vertical movement continues to the uniform list.
    title_content.style().restrict_scroll_to_axis = Some(true);
    if show_hint {
        title_content = title_content.child(
            div()
                .ml(px(5.))
                .w(px(hint_width))
                .flex_none()
                .whitespace_nowrap()
                .text_color(hint_color)
                .child(item.args_hint.clone()),
        );
    }
    let icon = item.icon_png.clone();
    div()
        .id(("ec-suggestion", ix))
        .flex()
        .flex_row()
        .items_center()
        // `uniform_list` lays each row out as a layout root, where an `auto`
        // width shrinks to fit. Without this the selected background would end
        // after the title instead of spanning the card like the legacy row.
        .w_full()
        .overflow_hidden()
        .rounded_t(px(corners.0))
        .rounded_b(px(corners.1))
        .pl(px(row_pad_left(font_size)))
        .h(px(row_height))
        .when_some(bg, |this, bg| this.bg(bg))
        .child(if is_ai_promoted {
            crate::icons::ai_icon_element(icon_size).into_any_element()
        } else {
            row_icon(&item.kind, icon, item.icon_identifier.as_deref(), icon_size).into_any_element()
        })
        .child(
            div()
                .ml(px(5.))
                .min_w(px(0.))
                .w(px(available))
                .flex_none()
                .overflow_hidden()
                .child(title_content),
        )
        // React's Suggestion uses onClick (mouse-up), not mouse-down. Besides
        // matching the old acceptance timing this avoids accepting a row when
        // the user presses and drags out of it before releasing.
        .on_click(move |_event, _window, cx| {
            state.update(cx, |overlay, cx| {
                overlay.invalidate_ai_request();
                overlay.selected = ix;
                overlay.has_changed_index = true;
                cx.notify();
            });
            if let Some(insert) = &click {
                insert(click_insert_for(&click_item, &insertion_search_term));
            }
        })
        .into_any_element()
}

/// GPUI clips `overflow_hidden` to a plain rectangle rather than following the
/// border radius, so a row that paints a background over the card has to
/// reproduce whichever card corners it covers. Only the first row reaches the
/// top, and the last row reaches the bottom only when no footer follows it.
fn row_corner_radii(ix: usize, last_row: usize, radius: f32, has_footer: bool) -> (f32, f32) {
    (
        if ix == 0 { radius } else { 0.0 },
        if ix == last_row && !has_footer { radius } else { 0.0 },
    )
}

fn brightness(mut color: Rgba, factor: f32) -> Rgba {
    color.r = (color.r * factor).clamp(0.0, 1.0);
    color.g = (color.g * factor).clamp(0.0, 1.0);
    color.b = (color.b * factor).clamp(0.0, 1.0);
    color
}

fn suggestion_title_highlights(
    runs: &[TextRun],
    text: Rgba,
    match_text: Rgba,
    match_bg: Rgba,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut title = String::new();
    let mut highlights = Vec::new();
    for run in runs {
        let start = title.len();
        title.push_str(&run.text);
        let end = title.len();
        match run.kind {
            RunKind::Match => highlights.push((
                start..end,
                HighlightStyle {
                    color: Some(match_text.into()),
                    background_color: Some(match_bg.into()),
                    ..HighlightStyle::default()
                },
            )),
            RunKind::Prefix => highlights.push((
                start..end,
                HighlightStyle {
                    color: Some(text.into()),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(text.into()),
                        wavy: false,
                    }),
                    ..HighlightStyle::default()
                },
            )),
            RunKind::Text => {},
        }
    }
    (title, highlights)
}

fn click_insert_for(item: &SuggestionItem, raw_search_term: &str) -> ClickInsert {
    ClickInsert {
        name: item.name.clone(),
        description: item.description.clone(),
        search: raw_search_term.to_string(),
        kind: item.kind.clone(),
        args_hint: item.args_hint.clone(),
        insert_value: item.insert_value.clone(),
        display_name: item.display_name.clone(),
        primary_name: item.primary_name.clone(),
        separator_to_add: item.separator_to_add.clone(),
        should_add_space: item.should_add_space,
        hidden: item.hidden,
        priority: item.priority,
        icon_identifier: item.icon_identifier.clone(),
        original_type: item.original_type.clone(),
        query_term: item.query_term.clone(),
        argument_value: item.argument_value,
        acceptance_scope: item.acceptance_scope.clone(),
    }
}

fn row_icon(kind: &str, png: Option<Arc<RenderImage>>, icon_identifier: Option<&str>, size: f32) -> impl IntoElement {
    if let Some(icon) = icon_identifier.and_then(|identifier| identifier_icon_element(identifier, size)) {
        return icon;
    }
    if kind == "history" && png.is_none() {
        return history_icon(size).into_any_element();
    }
    if let Some(image) = png {
        return png_icon_element(image, size).into_any_element();
    }
    if let Some(icon) = fallback_template_icon(kind, size) {
        return icon;
    }
    named_icon_element(kind, size).into_any_element()
}

/// `SuggestionIcon` used template tiles for these two legacy suggestion
/// types. They are not ordinary `box` icons, so keep the badge and color in
/// the native fallback path when a spec did not provide an explicit icon.
fn fallback_template_icon(kind: &str, size: f32) -> Option<AnyElement> {
    let identifier = match kind {
        "shortcut" => "fig://template?color=3498db&badge=💡",
        "mixin" => "fig://template?color=628dad&badge=➡️",
        _ => return None,
    };
    identifier_icon_element(identifier, size)
}

fn history_icon(size: f32) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .rounded(px(size * 0.25))
        .bg(rgb(0x6b7280))
        .flex()
        .items_center()
        .justify_center()
        .child(history_icon_image_element(size * 0.74))
}

#[cfg(test)]
fn item_description(item: Option<&SuggestionItem>, current_arg_name: &str, current_arg_description: &str) -> String {
    let selected = selected_item_description(item);
    if !selected.is_empty() {
        return selected;
    }
    let name = current_arg_name.trim();
    let description = current_arg_description.trim();
    match (name.is_empty(), description.is_empty()) {
        (false, false) => format!("{name}: {description}"),
        (false, true) => name.to_string(),
        (true, false) => description.to_string(),
        (true, true) => String::new(),
    }
}

/// Description used by the selected-row popout. This intentionally does not
/// fall back to currentArg; the WebView's popout showed `No description` when
/// the selected suggestion itself had no description.
fn selected_item_description(item: Option<&SuggestionItem>) -> String {
    let Some(item) = item else {
        return String::new();
    };
    let trimmed = item.description.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if matches!(item.kind.as_str(), "file" | "folder") {
        return item.kind.clone();
    }
    String::new()
}

fn selected_args_hint_rows(
    args_hint: &str,
    available_width: f32,
    font_size: f32,
    row_height: f32,
    max_list_height: f32,
) -> usize {
    if args_hint.trim().is_empty() {
        return 0;
    }
    let max_rows = ((max_list_height / row_height).floor() as usize)
        .saturating_sub(2)
        .clamp(1, MAX_ARGS_HINT_ROWS);
    let width = available_width.max(font_size);
    let estimated_rows: usize = args_hint
        .lines()
        .map(|line| (estimated_title_width(line, font_size) / width).ceil().max(1.0) as usize)
        .sum();
    estimated_rows.clamp(1, max_rows)
}

fn selected_args_hint_height(
    args_hint: &str,
    available_width: f32,
    font_size: f32,
    row_height: f32,
    max_list_height: f32,
) -> f32 {
    selected_args_hint_rows(args_hint, available_width, font_size, row_height, max_list_height) as f32 * row_height
}

fn selected_args_popout_height(args_hint: &str, font_size: f32, row_height: f32, max_list_height: f32) -> f32 {
    let hint_height = selected_args_hint_height(args_hint, POPOUT_WIDTH - 10.0, font_size, row_height, max_list_height);
    if hint_height > 0.0 {
        // Keep room for the description and the existing action hint. The
        // shared scroll area exposes text beyond the visible three hint rows.
        row_height + hint_height + 28.0
    } else {
        0.0
    }
}

fn suggestion_footer_height(item_count: usize, row_height: f32, popout: bool, loading: bool) -> f32 {
    if item_count == 0 {
        0.0
    } else if popout {
        if loading { row_height } else { 0.0 }
    } else {
        row_height
    }
}

/// Number of full-size rows that fit while keeping the bottom Description in
/// the old card's max-height budget. `react-window` receives the remaining
/// height after the footer; it is not allowed to claim the whole max height
/// and then push the footer below it.
fn suggestion_visible_rows(
    item_count: usize,
    row_height: f32,
    max_list_height: f32,
    popout: bool,
    loading: bool,
) -> usize {
    if item_count == 0 || row_height <= 0.0 {
        return 0;
    }
    let footer = suggestion_footer_height(item_count, row_height, popout, loading);
    let available = (max_list_height - footer).max(row_height);
    ((available / row_height).floor() as usize).clamp(1, item_count)
}

fn description_only_bar(name: &str, description: &str, theme: OverlayTheme, row_height: f32) -> impl IntoElement {
    let name = name.trim();
    let description = description.trim();
    let stacked = !name.is_empty() && !description.is_empty();
    let height = row_height * if stacked { 2.0 } else { 1.0 };
    let mut content = div()
        .id("ec-description-only-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_x_scroll()
        .overflow_y_hidden()
        .whitespace_nowrap();
    if !name.is_empty() {
        let title = if stacked { format!("{name}: ") } else { name.to_string() };
        content = content.child(div().flex_none().font_weight(FontWeight::BOLD).child(title));
    }
    if !description.is_empty() {
        content = content.child(div().flex_none().child(description.to_string()));
    }
    div()
        .id("ec-description-only")
        .flex()
        .items_center()
        .flex_none()
        .h(px(height))
        .px(px(8.))
        // No background: the card already paints this exact colour, and an
        // opaque child would square off the rounded corner it covers.
        .italic()
        .text_color(rgb(theme.muted))
        .overflow_hidden()
        .child(content)
}

#[allow(clippy::too_many_arguments)]
fn description_bar(
    selected_description: &str,
    current_arg_name: &str,
    current_arg_description: &str,
    theme: OverlayTheme,
    height: f32,
    show_hint: bool,
    loading: bool,
    hint: &str,
    ai_loading: Option<&crate::AiPreview>,
) -> impl IntoElement {
    let selected_description = selected_description.trim();
    let current_arg_name = current_arg_name.trim();
    let current_arg_description = current_arg_description.trim();
    let use_current_arg = selected_description.is_empty();
    let empty = use_current_arg && current_arg_name.is_empty() && current_arg_description.is_empty();
    let mut content = div()
        .id("ec-description-scroll")
        .flex()
        .flex_row()
        .flex_1()
        .min_w(px(0.))
        .overflow_x_scroll()
        .overflow_y_hidden()
        .whitespace_nowrap();
    if !selected_description.is_empty() {
        content = content.child(selected_description.to_string());
    } else {
        if !current_arg_name.is_empty() {
            content = content.child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .child(current_arg_name.to_string()),
            );
        }
        if !current_arg_name.is_empty() && !current_arg_description.is_empty() {
            content = content.child(div().flex_none().child(": "));
        }
        if !current_arg_description.is_empty() {
            content = content.child(div().flex_none().child(current_arg_description.to_string()));
        } else if empty {
            content = content.child("No description");
        }
    }
    div()
        .id("ec-description")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(height))
        .px(px(5.))
        .border_t_1()
        .border_color(rgb(theme.border))
        .italic()
        .text_color(rgb(if empty { 0x8c8c8c } else { theme.muted }))
        .overflow_hidden()
        .when_some(ai_loading, |this, preview| {
            this.child(crate::ai::loading_badge(preview, height))
        })
        .child(content)
        .when(loading, |this| this.child(loading_icon(theme, height)))
        .when(show_hint && !loading, |this| this.child(hint_chip(height, hint)))
}

#[allow(clippy::too_many_arguments)]
fn description_popout(
    description: &str,
    args_hint: &str,
    theme: OverlayTheme,
    max_height: f32,
    args_popout_height: f32,
    show_hint: bool,
    hint: &str,
    selected: usize,
    suggestions_revision: u64,
) -> impl IntoElement {
    let empty = description.is_empty();
    let text = if empty {
        "No description".to_string()
    } else {
        description.to_string()
    };
    let mut scroll_key = std::collections::hash_map::DefaultHasher::new();
    (selected, suggestions_revision, description, args_hint).hash(&mut scroll_key);
    div()
        .id("ec-description-popout")
        .flex()
        .flex_col()
        .flex_none()
        .w(px(POPOUT_WIDTH))
        .max_h(px(max_height.max(args_popout_height)))
        .when(args_popout_height > 0.0, |this| this.h(px(args_popout_height)))
        .pt(px(2.))
        .pb(px(4.))
        .rounded(px(POPOUT_RADIUS))
        .shadow(legacy_card_shadow())
        .bg(rgb(theme.background))
        .italic()
        .text_color(rgb(if empty { 0x8c8c8c } else { theme.muted }))
        .overflow_hidden()
        .child(
            div()
                .id(("ec-description-popout-scroll", scroll_key.finish()))
                .flex_1()
                .min_h(px(0.))
                .max_h(px((max_height.max(args_popout_height) - 10.0).max(0.0)))
                .overflow_y_scroll()
                .overflow_x_hidden()
                .pl(px(6.))
                .pr(px(4.))
                .child(div().whitespace_normal().child(text))
                .when(!args_hint.trim().is_empty(), |this| {
                    this.child(div().mt(px(4.)).whitespace_normal().child(args_hint.to_string()))
                }),
        )
        .when(show_hint, |this| {
            this.child(
                div()
                    .flex()
                    .justify_end()
                    .mt(px(4.))
                    .mx(px(4.))
                    .child(div().flex_none().bg(rgb(theme.border)).child(hint_chip(24., hint))),
            )
        })
}

fn hint_chip(height: f32, hint: &str) -> impl IntoElement {
    let hint = if hint.trim().is_empty() { "⌃k" } else { hint }.to_string();
    div()
        .rounded(px(3.))
        .px(px(4.))
        .text_size(px((height * 0.5).max(9.)))
        .italic()
        .child(hint)
}

fn loading_icon(theme: OverlayTheme, _row_height: f32) -> impl IntoElement {
    div()
        .id("ec-loading")
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .w(px(40.0))
        .h(px(24.0))
        .px(px(8.))
        .text_color(rgb(theme.selected_text))
        // This used to be a repeating animation. A stalled completion stream
        // could then keep the GPUI run loop repainting forever; a static marker
        // communicates the same state without keeping the process hot.
        .child(div().child(LOADING_DOTS))
}

fn dev_banner(
    width: f32,
    state: Entity<OverlayState>,
    on_disable: Option<Arc<dyn Fn() + Send + Sync>>,
) -> impl IntoElement {
    div()
        .id("ec-dev-banner")
        .m(px(4.))
        .w(px((width - 20.0).max(120.0)))
        .rounded(px(4.))
        .px(px(10.))
        .py(px(8.))
        .bg(rgb(0xf59e0b))
        .text_color(rgb(0x000000))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().font_weight(FontWeight::BOLD).child("Developer mode enabled!"))
        .child(
            div()
                .text_size(px(11.))
                .child("Loading specs from disk. Disable with either"),
        )
        .child(
            div()
                .text_size(px(10.))
                .pl(px(8.))
                .child("• Ctrl + C in the dev mode process"),
        )
        .child(
            div()
                .text_size(px(10.))
                .pl(px(8.))
                .underline()
                .child("• Click to disable")
                .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                    state.update(cx, |overlay, cx| {
                        overlay.show_dev_banner = false;
                        cx.notify();
                    });
                    if let Some(disable) = &on_disable {
                        disable();
                    }
                }),
        )
}

/// Outer window size that matches the old overlay chrome (padding, popout, banner, footer).
#[allow(clippy::too_many_arguments)]
pub fn overlay_content_size(
    item_count: usize,
    row_height: f32,
    font_size: f32,
    list_width: f32,
    max_list_height: f32,
    popout: bool,
    show_dev_banner: bool,
    loading: bool,
) -> (f32, f32) {
    overlay_content_size_with_context(
        item_count,
        row_height,
        font_size,
        list_width,
        max_list_height,
        popout,
        show_dev_banner,
        loading,
        0,
        "",
    )
}

/// Variant used by the desktop controller when the parser has a current
/// argument but no completion rows. `description_rows` is normally 0; a
/// description-only state uses one or two rows just like the WebView.
/// `selected_args_hint` reserves the extra height only when the detail popout is open.
#[allow(clippy::too_many_arguments)]
pub fn overlay_content_size_with_context(
    item_count: usize,
    row_height: f32,
    font_size: f32,
    list_width: f32,
    max_list_height: f32,
    popout: bool,
    show_dev_banner: bool,
    loading: bool,
    description_rows: usize,
    selected_args_hint: &str,
) -> (f32, f32) {
    // The WebView replaced the entire autocomplete card with its compact
    // loading indicator, even when stale rows were still present in state.
    if loading {
        return (40.0, 24.0);
    }
    let visible_rows = if item_count == 0 {
        if description_rows > 0 { description_rows } else { 0 }
    } else {
        suggestion_visible_rows(item_count, row_height, max_list_height, popout, loading)
    };
    let list_h = visible_rows as f32 * row_height;
    let footer = suggestion_footer_height(item_count, row_height, popout, loading);
    let border = CARD_BORDER * 2.0;
    let column_h = list_h + footer + border;
    let popout_h = if popout {
        (max_list_height.min(column_h) + border).max(selected_args_popout_height(
            selected_args_hint,
            font_size,
            row_height,
            max_list_height,
        ))
    } else {
        0.0
    };
    let inner_h = column_h.max(popout_h);
    let inner_w = list_width
        + border
        + if popout {
            POPOUT_WIDTH + border + layout_gap(font_size)
        } else {
            0.0
        };
    let banner = if show_dev_banner { DEV_BANNER_HEIGHT } else { 0.0 };
    let pad = layout_pad(font_size) * 2.0;
    (inner_w + pad, inner_h + pad + banner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, ScrollDelta, ScrollWheelEvent, TestAppContext, size};

    #[gpui::test]
    fn native_icon_demand_tracks_viewport_without_measurement_rows(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = calls.clone();
        let overlay = cx.new(|_| {
            let mut overlay = OverlayState::default();
            overlay.set_suggestions(
                (0..100)
                    .map(|index| SuggestionItem {
                        name: format!("file-{index}"),
                        kind: "file".into(),
                        ..Default::default()
                    })
                    .collect(),
                String::new(),
            );
            overlay.on_visible_range = Some(Arc::new(move |range, _| requests.lock().unwrap().push(range)));
            overlay
        });
        let view = cx.new(|_| SuggestionList::new(overlay.clone()));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.draw(point(px(0.), px(0.)), size(px(400.), px(220.)), |_, _| view.clone());
        };
        draw(cx);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(
            calls.lock().unwrap()[0].end > 1,
            "row zero measurement is not an icon demand"
        );
        overlay.update(cx, |overlay, _| overlay.selected = 50);
        calls.lock().unwrap().clear();
        draw(cx);
        let ranges = calls.lock().unwrap().clone();
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].contains(&50));
        assert!(!ranges[0].contains(&0), "offscreen measurement must not load row zero");
        draw(cx);
        assert_eq!(calls.lock().unwrap().len(), 1, "repaint must not submit another batch");
    }

    struct ScrollTestRow {
        overlay: Entity<OverlayState>,
        row: Entity<RowScrollState>,
        vertical: ScrollHandle,
    }

    impl Render for ScrollTestRow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let overlay = self.overlay.read(cx);
            let mut viewport = div()
                .id("test-viewport")
                .w(px(160.))
                .h(px(40.))
                .overflow_y_scroll()
                .track_scroll(&self.vertical)
                .text_size(px(DEFAULT_FONT_SIZE))
                .child(suggestion_row(
                    &overlay.items[0],
                    window,
                    cx,
                    self.row.clone(),
                    0,
                    true,
                    overlay.has_ai_promotion(),
                    "",
                    "",
                    "",
                    false,
                    "",
                    overlay.theme,
                    DEFAULT_ROW_HEIGHT,
                    ICON_SIZE,
                    DEFAULT_FONT_SIZE,
                    160.,
                    overlay.title_overflow,
                    (0., 0.),
                    overlay.font_family.clone(),
                    self.overlay.clone(),
                    None,
                    0,
                ))
                .child(div().h(px(200.)));
            viewport.style().restrict_scroll_to_axis = Some(true);
            viewport
        }
    }

    // Exercise GPUI's real layout, hit testing, native scroll listener and
    // bubble order: state-only tests cannot catch an ellipsis with no overflow.
    #[gpui::test]
    fn horizontal_scroll_reveals_titles_and_freezes_pending_ai(cx: &mut TestAppContext) {
        for (name, hint, overflow, promoted) in [
            (
                "very-long-file-or-branch-name-that-exceeds-the-row",
                "",
                TitleOverflow::Scroll,
                false,
            ),
            (
                "very-long-option-name-that-exceeds-the-row",
                "",
                TitleOverflow::Ellipsis,
                false,
            ),
            (
                "cmd",
                "<long-argument-with-a-tail-that-must-remain-readable>",
                TitleOverflow::Scroll,
                false,
            ),
            (
                "very-long-already-promoted-name-that-exceeds-the-row",
                "",
                TitleOverflow::Scroll,
                true,
            ),
        ] {
            let cx = cx.add_empty_window();
            let overlay = cx.new(|_| {
                let mut overlay = OverlayState::default();
                overlay.items = vec![
                    SuggestionItem {
                        name: name.into(),
                        args_hint: hint.into(),
                        ..Default::default()
                    },
                    SuggestionItem {
                        name: "other".into(),
                        ..Default::default()
                    },
                ];
                overlay.title_overflow = overflow;
                if promoted {
                    overlay.items.swap(0, 1);
                    assert!(overlay.promote_ai_suggestion(1));
                } else {
                    overlay.ai_preview = Some(crate::ai::AiPreview::Loading(0));
                }
                overlay
            });
            let row = cx.new(|_| RowScrollState::default());
            let vertical = ScrollHandle::new();
            let view = cx.new(|_| ScrollTestRow {
                overlay: overlay.clone(),
                row: row.clone(),
                vertical: vertical.clone(),
            });
            let draw = |cx: &mut gpui::VisualTestContext| {
                cx.draw(point(px(0.), px(0.)), size(px(160.), px(40.)), |_, _| view.clone());
            };
            let wheel = |cx: &mut gpui::VisualTestContext, x, y| {
                cx.simulate_event(ScrollWheelEvent {
                    position: point(px(60.), px(10.)),
                    delta: ScrollDelta::Pixels(point(px(x), px(y))),
                    ..Default::default()
                });
            };
            draw(cx);
            let revision = overlay.read_with(cx, |s, _| s.suggestions_revision);
            // Outward gestures at the start and pure vertical scrolling do not
            // cancel AI or switch away from the configured title presentation.
            wheel(cx, 40., 0.);
            draw(cx);
            wheel(cx, 0., -5.);
            assert_eq!(vertical.offset().y, px(-5.));
            assert!(!row.read_with(cx, |r, _| r.manual));
            assert_eq!(overlay.read_with(cx, |s, _| s.ai_revision), 0);
            vertical.set_offset(point(px(0.), px(0.)));
            draw(cx);

            wheel(cx, -40., -5.);
            draw(cx);
            let handle = row.read_with(cx, |r, _| {
                assert!(r.manual);
                r.handle.clone()
            });
            assert_eq!(handle.offset().x, px(-40.));
            assert_eq!(vertical.offset().y, px(0.));
            overlay.update(cx, |s, _| {
                assert_eq!(s.ai_revision, 1);
                assert!(s.ai_preview.is_none());
                assert_eq!(s.suggestions_revision, revision);
                assert_eq!(s.selected, 0);
                assert_eq!(s.items[0].name, name);
                assert_eq!(s.has_ai_promotion(), promoted);
                assert!(!s.has_changed_index);
            });
            // Repaint and another gesture retain the offset instead of
            // restarting the marquee or rebuilding the scroll handle.
            draw(cx);
            wheel(cx, -20., 0.);
            draw(cx);
            assert_eq!(handle.offset().x, px(-60.));
            wheel(cx, -1000., 0.);
            draw(cx);
            assert_eq!(handle.offset().x, -handle.max_offset().width);
            let ai_revision = overlay.read_with(cx, |s, _| s.ai_revision);
            wheel(cx, -40., 0.);
            draw(cx);
            assert_eq!(overlay.read_with(cx, |s, _| s.ai_revision), ai_revision);
            wheel(cx, 1000., 0.);
            draw(cx);
            assert_eq!(handle.offset().x, px(0.));
        }
    }

    #[test]
    fn match_prefix_is_case_insensitive() {
        assert_eq!(match_prefix_bytes("checkout", "ch"), 2);
        assert_eq!(match_prefix_bytes("Checkout", "ch"), 2);
        assert_eq!(match_prefix_bytes("src/main.rs", "src/m"), 5);
        assert_eq!(match_prefix_bytes("checkout", "git"), 0);
        assert_eq!(match_prefix_bytes("文件.txt", "文"), "文".len());
    }

    #[test]
    fn icon_box_keeps_the_old_row_ratio() {
        // CSS `0 0 3px` is sigma 1.5, and GPUI blurs by sigma directly.
        assert_eq!(LEGACY_SHADOW_SIGMA, 1.5);
        assert_eq!(icon_size_for_row(DEFAULT_ROW_HEIGHT), ICON_SIZE);
        assert_eq!(icon_size_for_row(16.0), 12.0);
    }

    #[test]
    fn only_rows_touching_a_card_corner_round_themselves() {
        // Footer present: the list owns the top corners only.
        assert_eq!(row_corner_radii(0, 4, 3.2, true), (3.2, 0.0));
        assert_eq!(row_corner_radii(2, 4, 3.2, true), (0.0, 0.0));
        assert_eq!(row_corner_radii(4, 4, 3.2, true), (0.0, 0.0));
        // Popout mode drops the footer, so the last row reaches the bottom.
        assert_eq!(row_corner_radii(4, 4, 3.2, false), (0.0, 3.2));
        // A lone row owns all four corners.
        assert_eq!(row_corner_radii(0, 0, 3.2, false), (3.2, 3.2));
    }

    #[test]
    fn rem_spacings_use_the_font_size_not_a_16px_basis() {
        // `:root { font-size: 12.8px }` made every Tailwind rem 0.8x what a
        // browser default would give: `p-1` is 3.2px, not 4px.
        assert_eq!(layout_pad(DEFAULT_FONT_SIZE), 3.2);
        assert_eq!(layout_gap(DEFAULT_FONT_SIZE), 4.8);
        assert_eq!(card_radius(DEFAULT_FONT_SIZE), 3.2);
        assert_eq!(row_pad_left(DEFAULT_FONT_SIZE), 4.8);
        // The popout radius was a literal `4px` and must not scale.
        assert_eq!(POPOUT_RADIUS, 4.0);
        // A larger font scales the chrome with it, as `rem` did.
        assert_eq!(layout_pad(16.0), 4.0);
    }

    #[test]
    fn smart_scroll_only_moves_when_selection_leaves_viewport() {
        assert_eq!(smart_scroll_strategy(3, 0, 6), None);
        assert_eq!(smart_scroll_strategy(0, 2, 6), Some(ScrollStrategy::Top));
        assert_eq!(smart_scroll_strategy(9, 2, 6), Some(ScrollStrategy::Bottom));
    }

    #[test]
    fn visible_range_uses_the_measured_viewport_not_the_full_list() {
        // `cd ~/` is 56 rows in a 5-row window. Offset 0 must not look like
        // every history/folder row is already on screen.
        assert_eq!(visible_range_for_scroll(0.0, 20.0, 5, 56), (0, 4));
        assert_eq!(smart_scroll_strategy(5, 0, 4), Some(ScrollStrategy::Bottom));
        assert_eq!(visible_range_for_scroll(-20.0, 20.0, 5, 56), (1, 5));
        assert_eq!(visible_range_for_scroll(0.0, 20.0, 5, 3), (0, 2));
    }

    #[test]
    fn loading_marker_is_static() {
        assert_eq!(LOADING_DOTS, "···");
    }

    #[test]
    fn fuzzy_runs_highlight_subsequence() {
        let runs = name_runs("checkout", "ckt", true, "");
        let matched: String = runs
            .iter()
            .filter(|run| run.kind == RunKind::Match)
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(matched, "ckt");
    }

    #[test]
    fn fuzzy_indexes_prefer_word_boundaries_and_consecutive_matches() {
        assert_eq!(fuzzy_indexes("fooBar", "fb"), Some(vec![0, 3]));
        assert_eq!(fuzzy_indexes("aXab", "ab"), Some(vec![0, 3]));
    }

    #[test]
    fn description_falls_back_to_current_arg_name_and_description() {
        assert_eq!(item_description(None, "branch", "Branch name"), "branch: Branch name");
        assert_eq!(item_description(None, "branch", ""), "branch");
        assert_eq!(item_description(None, "", "Branch name"), "Branch name");
    }

    #[test]
    fn popout_description_does_not_use_current_arg_fallback() {
        let item = item("status", "subcommand");
        assert_eq!(selected_item_description(Some(&item)), "");
        assert_eq!(item_description(Some(&item), "current", "context"), "current: context");
    }

    #[test]
    fn visible_rows_reserve_space_for_the_description_footer() {
        // The old card's 140px max contains six 20px rows plus its 20px
        // Description footer. The seventh row must not push that footer out.
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, false, false), 6);
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, true, false), 7);
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, true, true), 6);
    }

    #[test]
    fn shortcut_and_mixin_use_template_icon_fallbacks() {
        assert!(fallback_template_icon("shortcut", ICON_SIZE).is_some());
        assert!(fallback_template_icon("mixin", ICON_SIZE).is_some());
        assert!(fallback_template_icon("arg", ICON_SIZE).is_none());
    }

    #[test]
    fn prefix_runs_highlight_query_then_underline_rest_of_common_prefix() {
        let runs = name_runs("checkout", "ch", false, "check");
        assert_eq!(runs[0].kind, RunKind::Match);
        assert_eq!(runs[0].text, "ch");
        assert_eq!(runs[1].kind, RunKind::Prefix);
        assert_eq!(runs[1].text, "eck");
    }

    #[test]
    fn common_prefix_uses_same_kind_rows() {
        let items = vec![
            SuggestionItem {
                name: "checkout".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            },
            SuggestionItem {
                name: "cherry-pick".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            },
            SuggestionItem {
                name: "--help".into(),
                kind: "option".into(),
                ..SuggestionItem::default()
            },
        ];
        assert_eq!(common_prefix_for(0, &items, ""), "che");
    }

    #[test]
    fn common_prefix_preserves_unicode_boundaries_and_ascii_only_folding() {
        for (names, expected) in [
            (["a💡branch", "A💡commit"], "a💡"),
            (["FÖO", "fÖot"], "fÖo"),
            (["Éclair", "école"], ""),
            (["checkout", "CHECKOUT"], "checkout"),
            (["", "checkout"], ""),
        ] {
            let items = names.map(|name| item(name, "subcommand"));
            assert_eq!(common_prefix_for(0, &items, ""), expected);
        }
        let items = [item("checkout", "subcommand"), item("--help", "option")];
        assert_eq!(common_prefix_for(0, &items, ""), "");
    }

    #[test]
    fn common_prefix_uses_updated_presentation_even_when_identity_is_unchanged() {
        let mut items = [item("alpha", "subcommand"), item("bravo", "subcommand")];
        items[0].display_name = Some("FOÖbar".into());
        items[1].display_name = Some("foÖbaz".into());
        assert_eq!(common_prefix_for(0, &items, ""), "foÖba");
        items[1].display_name = Some("Other".into());
        assert_eq!(common_prefix_for(0, &items, ""), "");
    }

    #[test]
    fn auto_execute_row_uses_original_type_for_prefix_decoration() {
        let items = vec![
            SuggestionItem {
                name: "checkout".into(),
                kind: "auto-execute".into(),
                original_type: Some("subcommand".into()),
                ..SuggestionItem::default()
            },
            item("checkout", "subcommand"),
            item("cherry-pick", "subcommand"),
        ];
        assert_eq!(common_prefix_for(0, &items, ""), "che");
        // Decoration parity must not turn Tab into execution when multiple
        // rows are present.
        assert_eq!(tab_prefix_insertion(0, &items, "ch"), None);
    }

    #[test]
    fn content_size_includes_footer_not_popout() {
        let (w, h) = overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        assert_eq!(w, 320.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
        assert_eq!(h, 60.0 + 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
    }

    #[test]
    fn content_size_adds_popout_width() {
        let (w, _) = overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false);
        assert_eq!(
            w,
            320.0
                + CARD_BORDER * 2.0
                + POPOUT_WIDTH
                + CARD_BORDER * 2.0
                + layout_gap(DEFAULT_FONT_SIZE)
                + layout_pad(DEFAULT_FONT_SIZE) * 2.0
        );
    }

    #[test]
    fn popout_height_tracks_a_short_list() {
        let (_, h) = overlay_content_size(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false);
        assert_eq!(h, 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
    }

    #[test]
    fn content_size_loading_replaces_the_entire_card() {
        assert_eq!(
            overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, true),
            (40.0, 24.0)
        );
        assert_eq!(
            overlay_content_size(3, 22.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, true, true),
            (40.0, 24.0)
        );
    }

    #[test]
    fn content_size_footer_tracks_row_height() {
        let (_, h20) = overlay_content_size(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        let (_, h22) = overlay_content_size(1, 22.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        assert_eq!(h22 - h20, 4.0);
    }

    #[test]
    fn content_size_keeps_current_arg_description_visible_without_rows() {
        let (_, one_line) =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 1, "");
        let (_, two_lines) =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 2, "");
        assert_eq!(one_line, 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
        assert_eq!(
            two_lines,
            40.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0
        );

        let loading =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, true, 2, "");
        assert_eq!(loading, (40.0, 24.0));
    }

    fn item(name: &str, kind: &str) -> SuggestionItem {
        SuggestionItem {
            name: name.into(),
            kind: kind.into(),
            ..SuggestionItem::default()
        }
    }

    #[test]
    fn typed_directory_prefix_is_the_path_up_to_the_last_slash() {
        assert_eq!(typed_directory_prefix("src/m"), "src/");
        assert_eq!(typed_directory_prefix("src/"), "src/");
        assert_eq!(typed_directory_prefix("~/Desktop/foo"), "~/Desktop/");
        assert_eq!(typed_directory_prefix("src"), "");
        assert_eq!(typed_directory_prefix(""), "");
        assert_eq!(typed_directory_prefix("~"), "");
    }

    #[test]
    fn unquote_shell_token_strips_quotes_and_backslash_spaces() {
        assert_eq!(unquote_shell_token("~/De"), "~/De");
        assert_eq!(unquote_shell_token("'src/foo/"), "src/foo/");
        assert_eq!(unquote_shell_token(r"my\ dir/foo"), "my dir/foo");
        assert_eq!(unquote_shell_token(r#""src/foo/""#), "src/foo/");
        assert_eq!(unquote_shell_token("~"), "~");
    }

    #[test]
    fn unquote_shell_token_strips_ansi_c_quote_prefix() {
        assert_eq!(unquote_shell_token("$'src/foo/"), "src/foo/");
        assert_eq!(unquote_shell_token("echo$'foo"), "echofoo");
        // A dollar that is not immediately followed by a single quote remains
        // part of the token (for example, an environment-variable name).
        assert_eq!(unquote_shell_token("$HOME"), "$HOME");
    }

    #[test]
    fn unquote_shell_token_does_not_close_ansi_c_quote_on_an_escaped_quote() {
        assert_eq!(unquote_shell_token(r"$'a\'b'"), "a'b");
    }

    #[test]
    fn path_rows_hide_the_already_typed_directory() {
        let main = item("src/main.rs", "file");
        let nested = item("src/foo/bar/", "folder");
        let parent = item("../", "folder");
        let top = item("src/", "folder");
        assert_eq!(visible_suggestion_title(&main, "src/m"), "main.rs");
        assert_eq!(visible_suggestion_title(&nested, "src/foo/"), "bar/");
        assert_eq!(visible_suggestion_title(&parent, "src/"), "../");
        assert_eq!(visible_suggestion_title(&top, "s"), "src/");
        assert_eq!(visible_suggestion_title(&top, ""), "src/");
        // Callers unquote search_term first. The getQueryTerm tail (`bar` from
        // `src/foo/bar`) is the highlight, not the directory prefix.
        assert_eq!(
            visible_suggestion_title(&nested, &unquote_shell_token("'src/foo/")),
            "bar/"
        );
        let spaced = item("my dir/foo/", "folder");
        assert_eq!(visible_suggestion_title(&spaced, "my dir/"), "foo/");
        assert_eq!(
            visible_suggestion_title(&spaced, &unquote_shell_token(r"my\ dir/")),
            "foo/"
        );
        let home = item("~/Desktop/", "folder");
        assert_eq!(visible_suggestion_title(&home, "~"), "Desktop/");
        assert_eq!(visible_suggestion_title(&home, "~/"), "Desktop/");
        assert_eq!(visible_suggestion_title(&home, "~/De"), "Desktop/");
        let tilde = item("~", "arg");
        assert_eq!(visible_suggestion_title(&tilde, "~"), "~");
        assert_eq!(visible_suggestion_title(&tilde, "~/"), "~");
    }

    #[test]
    fn slash_prefix_is_only_hidden_on_file_and_folder_rows() {
        let branch = item("feat/login", "arg");
        let history = item("feat/login", "history");
        let sub = item("merge", "subcommand");
        assert_eq!(visible_suggestion_title(&branch, "feat/"), "feat/login");
        assert_eq!(visible_suggestion_title(&history, "feat/"), "feat/login");
        assert_eq!(visible_suggestion_title(&sub, "mer"), "merge");
        let branches = vec![item("feat/login", "arg"), item("feat/logout", "arg")];
        assert_eq!(common_prefix_for(0, &branches, "feat/"), "feat/log");
        let siblings = vec![item("feat/foo", "arg"), item("feat/bar", "arg")];
        assert_eq!(common_prefix_for(0, &siblings, "feat/"), "feat/");
    }

    #[test]
    fn get_query_term_tail_must_not_be_used_as_the_directory_prefix() {
        let home = item("~/Desktop/", "folder");
        // `cd`'s getQueryTerm is "/": match_term is `~` or `De`, not `~/De`.
        assert_eq!(visible_suggestion_title(&home, "De"), "~/Desktop/");
        assert_eq!(
            visible_suggestion_title(&home, &unquote_shell_token("~/De")),
            "Desktop/"
        );
    }

    #[test]
    fn path_common_prefix_is_computed_on_the_visible_tail() {
        let items = vec![item("src/main.rs", "file"), item("src/mod.rs", "file")];
        assert_eq!(common_prefix_for(0, &items, "src/m"), "m");
        let folders = vec![item("src/foo/", "folder"), item("src/bar/", "folder")];
        assert_eq!(common_prefix_for(0, &folders, "src/"), "");
        let home = vec![item("~/Desktop/", "folder"), item("~/Documents/", "folder")];
        // Underlining `~/` on every home-folder row was the misleading hint.
        assert_eq!(common_prefix_for(0, &home, "~"), "d");
        assert_eq!(common_prefix_for(0, &home, "~/"), "d");
    }

    #[test]
    fn path_highlight_runs_against_the_visible_tail() {
        let runs = name_runs("main.rs", "m", false, "m");
        assert_eq!(runs[0].kind, RunKind::Match);
        assert_eq!(runs[0].text, "m");
        assert_eq!(runs[1].kind, RunKind::Text);
        assert_eq!(runs[1].text, "ain.rs");
        assert_eq!(strip_typed_directory_prefix("src/m", "src/m"), "m");
    }

    #[test]
    fn title_overflow_defaults_to_scroll() {
        assert_eq!(TitleOverflow::parse(""), TitleOverflow::Scroll);
        assert_eq!(TitleOverflow::parse("scroll"), TitleOverflow::Scroll);
        assert_eq!(TitleOverflow::parse("marquee"), TitleOverflow::Scroll);
        assert_eq!(TitleOverflow::parse("ellipsis"), TitleOverflow::Ellipsis);
        assert_eq!(TitleOverflow::default(), TitleOverflow::Scroll);
    }

    #[test]
    fn marquee_pauses_then_slides_the_overflow() {
        let cycle = marquee_duration(100.0).as_secs_f32();
        let pause = MARQUEE_PAUSE_SECS * 0.5 / cycle;
        let travel = marquee_travel_secs(100.0) / cycle;
        let midpoint = pause + travel * 0.5;
        let forward_end = pause + travel;

        assert_eq!(marquee_offset(0.0, 100.0), 0.0);
        assert_eq!(marquee_offset(pause * 0.5, 100.0), 0.0);
        assert!((marquee_offset(midpoint, 100.0) - 50.0).abs() < 0.01);
        assert_eq!(marquee_offset(0.5, 100.0), 100.0);
        assert!((marquee_offset(1.0 - midpoint, 100.0) - 50.0).abs() < 0.01);
        assert_eq!(marquee_offset(1.0, 100.0), 0.0);
        assert!(marquee_duration(10.0) >= Duration::from_secs(4));
        assert!(marquee_duration(100.0) > marquee_duration(10.0));
        assert!(marquee_duration(1000.0) > Duration::from_secs(30));
        // Endpoints have zero velocity, so reversing direction does not jerk.
        assert!(marquee_offset(pause + 0.002, 100.0) < 0.1);
        assert!(100.0 - marquee_offset(forward_end - 0.002, 100.0) < 0.1);
    }

    #[test]
    fn marquee_speed_stays_bounded_for_long_titles() {
        let extra = 1000.0;
        let cycle = marquee_duration(extra).as_secs_f32();
        let sample_secs = 0.01;
        let sample_delta = sample_secs / cycle;
        for step in 0..1000 {
            let delta = step as f32 / 1000.0;
            let distance = (marquee_offset(delta + sample_delta, extra) - marquee_offset(delta, extra)).abs();
            let speed = distance / sample_secs;
            assert!(
                speed <= MARQUEE_PEAK_SPEED_PX_PER_SEC + 0.1,
                "marquee reached {speed} px/s at delta {delta}"
            );
        }
    }

    #[test]
    fn ascii_titles_are_narrower_than_wide_glyphs() {
        let ascii = estimated_title_width("abcdefghij", 10.0);
        let wide = estimated_title_width("中文目录名", 10.0);
        assert!(ascii < 70.0, "{ascii}");
        assert!(wide > 40.0, "{wide}");
    }

    #[test]
    fn title_highlights_only_include_the_name() {
        let runs = name_runs("main.rs", "m", false, "m");
        let (title, highlights) = suggestion_title_highlights(&runs, rgb(0x111111), rgb(0x222222), rgb(0x333333));
        assert_eq!(title, "main.rs");
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].0, 0.."m".len());
        assert!(highlights[0].1.background_color.is_some());
    }

    #[test]
    fn selected_args_hint_does_not_change_the_inline_footer_height() {
        let hint = "[branch-name] [remote-name] [path-to-long-file] [optional-revision]";
        let (_, without_args) =
            overlay_content_size_with_context(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 0, "");
        let (_, with_args) =
            overlay_content_size_with_context(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 0, hint);
        assert_eq!(with_args, without_args);
    }

    #[test]
    fn selected_args_hint_can_expand_a_short_popout() {
        let hint = "[branch-name] [remote-name] [path-to-long-file] [optional-revision]";
        let (_, base) =
            overlay_content_size_with_context(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false, 0, "");
        let (_, with_args) =
            overlay_content_size_with_context(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false, 0, hint);
        assert!(with_args > base);
        assert_eq!(
            with_args - layout_pad(DEFAULT_FONT_SIZE) * 2.0,
            selected_args_popout_height(hint, DEFAULT_FONT_SIZE, 20.0, 140.0)
        );
    }

    #[test]
    fn explicit_display_name_is_also_stripped_when_it_repeats_the_directory() {
        let mut row = item("src/web", "file");
        row.display_name = Some("src/web (nginx)".into());
        assert_eq!(visible_suggestion_title(&row, "src/"), "web (nginx)");
    }

    #[test]
    fn mouse_payload_keeps_raw_search_for_shell_deletion() {
        let mut suggestion = item("checkout", "subcommand");
        suggestion.query_term = Some("co".into());
        suggestion.primary_name = Some("checkout".into());
        let click = click_insert_for(&suggestion, "'co");
        assert_eq!(click.search, "'co");
        assert_eq!(click.query_term.as_deref(), Some("co"));
        assert_eq!(click.primary_name.as_deref(), Some("checkout"));
    }

    #[test]
    fn tab_prefix_inserts_full_row_when_only_one_suggestion() {
        let items = vec![item("checkout", "subcommand")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Full("checkout".into()))
        );
    }

    #[test]
    fn tab_prefix_accepts_a_single_auto_execute_row() {
        let items = vec![item("↪", "auto-execute")];
        assert_eq!(tab_prefix_insertion(0, &items, ""), Some(TabPrefix::Full("↪".into())));
    }

    #[test]
    fn tab_prefix_uses_same_kind_prefix_matches_and_original_case() {
        let items = vec![
            item("Checkout", "subcommand"),
            item("Cherry-pick", "subcommand"),
            item("--help", "option"),
        ];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Partial("Che".into()))
        );
    }

    #[test]
    fn parent_directory_does_not_join_an_ordinary_file_prefix_set() {
        let items = vec![
            item("src/", "folder"),
            item("scripts/", "folder"),
            item("../", "folder"),
        ];
        assert_eq!(common_prefix_for(0, &items, ""), "s");
        assert_eq!(tab_prefix_insertion(0, &items, "s"), None);
    }

    #[test]
    fn tab_prefix_uses_the_selected_rows_query_term_override() {
        let mut first = item("foobar", "arg");
        first.query_term = Some("foo".into());
        let items = vec![first, item("foobaz", "arg")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "scope@foo"),
            Some(TabPrefix::Partial("fooba".into()))
        );
    }

    #[test]
    fn tab_prefix_inserts_full_row_when_only_one_same_kind_prefix() {
        let items = vec![item("checkout", "subcommand"), item("--help", "option")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Full("checkout".into()))
        );
    }

    #[test]
    fn tab_prefix_returns_none_when_shared_prefix_equals_query() {
        let items = vec![item("checkout", "subcommand"), item("cherry-pick", "subcommand")];
        assert_eq!(tab_prefix_insertion(0, &items, "che"), None);
    }

    #[test]
    fn tab_prefix_of_checkout_history_keeps_the_trailing_space() {
        let items = vec![
            item("checkout main", "history"),
            item("checkout master", "history"),
            item("checkout -b feat", "history"),
            item("checkout", "subcommand"),
        ];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Partial("checkout ".into()))
        );
    }

    #[test]
    fn tab_prefix_unquotes_search_to_match_path_names() {
        let items = vec![item("src/foo/", "folder"), item("src/foo2/", "folder")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "'src/fo"),
            Some(TabPrefix::Partial("src/foo".into()))
        );
        assert_eq!(
            tab_prefix_insertion(0, &items, r#""src/fo"#),
            Some(TabPrefix::Partial("src/foo".into()))
        );
    }

    #[test]
    fn tab_prefix_falls_back_to_unquoted_search_when_query_term_is_a_path_tail() {
        let mut foo = item("src/foo/", "folder");
        foo.query_term = Some("fo".into());
        let mut foo2 = item("src/foo2/", "folder");
        foo2.query_term = Some("fo".into());
        assert_eq!(
            tab_prefix_insertion(0, &[foo, foo2], "'src/fo"),
            Some(TabPrefix::Partial("src/foo".into()))
        );
    }

    #[test]
    fn tab_prefix_matches_an_ansi_c_quoted_token_without_the_dollar() {
        let items = vec![item("foo", "arg"), item("foobar", "arg")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "$'fo"),
            Some(TabPrefix::Partial("foo".into()))
        );
    }
}
