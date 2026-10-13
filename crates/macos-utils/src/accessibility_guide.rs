//! Codex-style Accessibility grant: open System Settings and float a
//! draggable app-icon card beside the list. Never raises the system TCC sheet.

#![allow(unexpected_cfgs)]

use std::ffi::CStr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::context::CGContext;
use core_graphics::display::{CGPoint, CGRect, CGSize, CGWindowListCopyWindowInfo};
use core_graphics::window::{
    kCGNullWindowID, kCGWindowBounds, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc::declare::ClassDecl;
use objc::rc::autoreleasepool;
use objc::runtime::{BOOL, Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use tracing::debug;

use crate::accessibility::{accessibility_is_enabled, open_accessibility};
use crate::applications::running_application_pids;
use crate::bundle::{get_bundle_identifier, get_bundle_path};

const CARD_WIDTH: f64 = 300.0;
const CARD_HEIGHT: f64 = 188.0;
const CARD_GAP: f64 = 20.0;
const CARD_MARGIN: f64 = 20.0;
const ARROW_TAG: isize = 7101;
const ARROW_SIZE: f64 = 22.0;
const ARROW_GAP: f64 = 10.0;
const CHIP_Y: f64 = 16.0;
const CHIP_HEIGHT: f64 = 56.0;
const CHIP_ICON: f64 = 36.0;
const TITLE_HEIGHT: f64 = 22.0;
const CLOSE_SIZE: f64 = 22.0;
const SETTINGS_GONE_TICKS: u8 = 25;
const NS_DRAG_OPERATION_COPY: usize = 1;
const DRAG_CHIP_RADIUS: f64 = 12.0;
const NS_IMAGE_ALIGN_CENTER: u64 = 0;
const NS_IMAGE_SCALE_PROPORTIONALLY: u64 = 3;
/// 0 unknown, 1 arrow points right, -1 arrow points left.
static ARROW_DIRECTION: AtomicI8 = AtomicI8::new(0);
static CHIP: Mutex<Option<usize>> = Mutex::new(None);

/// Same mask as the overlay: click/drag must not activate FasTab.
const NS_WINDOW_STYLE_NONACTIVATING_PANEL: u64 = 1 << 7;
const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: i64 = 2;
const NS_MODAL_PANEL_WINDOW_LEVEL: i64 = 8;
const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: u64 = 1 << 4;
const NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE: u64 = 1 << 6;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
const NS_VISUAL_EFFECT_MATERIAL_POPOVER: i64 = 6;
const NS_VISUAL_EFFECT_BLENDING_BEHIND_WINDOW: i64 = 0;
const NS_VISUAL_EFFECT_STATE_ACTIVE: i64 = 1;
const NS_FONT_WEIGHT_SEMIBOLD: f64 = 0.3;
const NS_FONT_WEIGHT_REGULAR: f64 = 0.0;

/// Odd epochs are active; closing advances to an even epoch. Delayed callbacks
/// keep their epoch so reopening cannot revive a previous guide's polling loop.
struct GuideLifecycle(AtomicU64);

impl GuideLifecycle {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn is_active(&self) -> bool {
        self.0.load(Ordering::SeqCst) & 1 != 0
    }

    fn begin(&self) -> Option<u64> {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                (epoch & 1 == 0).then(|| epoch.wrapping_add(1))
            })
            .ok()
            .map(|epoch| epoch.wrapping_add(1))
    }

    fn is_current(&self, epoch: u64) -> bool {
        epoch & 1 != 0 && self.0.load(Ordering::SeqCst) == epoch
    }

    fn dismiss(&self) {
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
            (epoch & 1 != 0).then(|| epoch.wrapping_add(1))
        });
    }
}

static GUIDE: GuideLifecycle = GuideLifecycle::new();
static DRAGGING: AtomicBool = AtomicBool::new(false);
static SETTINGS_MISSING: AtomicU8 = AtomicU8::new(0);
/// -1 system locale, 0 English, 1 Chinese.
static PREFER_ZH: AtomicI8 = AtomicI8::new(-1);
static PANEL: Mutex<Option<usize>> = Mutex::new(None);
static REGISTER_CLASSES: Once = Once::new();

pub fn accessibility_guide_is_active() -> bool {
    GUIDE.is_active()
}

/// Open the Accessibility pane and dock a drag-to-grant card beside it.
/// Safe to call from a background thread. Does nothing if already granted.
///
/// `prefer_zh` follows the settings page (`dashboard.language`): `Some(true)`
/// Chinese, `Some(false)` English, `None` the system locale.
pub fn begin_accessibility_guide(prefer_zh: Option<bool>) {
    if on_main_thread() {
        start_guide(prefer_zh);
    } else {
        dispatch::Queue::main().exec_async(move || start_guide(prefer_zh));
    }
}

fn start_guide(prefer_zh: Option<bool>) {
    if accessibility_is_enabled() {
        dismiss_guide();
        return;
    }
    let Some(epoch) = GUIDE.begin() else {
        if let Some(panel) = panel_ptr() {
            unsafe {
                let _: () = msg_send![panel, orderFrontRegardless];
            }
        }
        return;
    };

    PREFER_ZH.store(
        match prefer_zh {
            Some(false) => 0,
            Some(true) => 1,
            None => -1,
        },
        Ordering::SeqCst,
    );
    // A cdhash-stale grant stays in the list with the switch on, but this
    // process is not trusted. Drop our row so the current binary can be dragged in.
    clear_stale_accessibility_row();
    open_accessibility();
    wait_for_settings(epoch, 0, None);
}

fn wait_for_settings(epoch: u64, attempt: u8, last: Option<(f64, f64, f64, f64)>) {
    if !GUIDE.is_current(epoch) {
        return;
    }
    if DRAGGING.load(Ordering::SeqCst) {
        dispatch::Queue::main().exec_after(Duration::from_millis(50), move || {
            wait_for_settings(epoch, attempt, last);
        });
        return;
    }
    if accessibility_is_enabled() {
        dismiss_guide();
        return;
    }
    let current = settings_window_cocoa();
    if let Some(settings) = settings_ready_to_dock(current, last, attempt) {
        show_card_beside_settings(epoch, settings);
        return;
    }
    if current.is_none() && last.is_none() && attempt >= 120 {
        dismiss_guide();
        return;
    }
    dispatch::Queue::main().exec_after(Duration::from_millis(50), move || {
        wait_for_settings(epoch, attempt.saturating_add(1), current.or(last));
    });
}

fn settings_ready_to_dock(
    current: Option<(f64, f64, f64, f64)>,
    last: Option<(f64, f64, f64, f64)>,
    attempt: u8,
) -> Option<(f64, f64, f64, f64)> {
    if let Some(now) = current {
        let stable = last.is_some_and(|previous| frames_close(previous, now));
        if stable || (attempt >= 60 && last.is_some()) {
            return Some(now);
        }
        return None;
    }
    if attempt >= 120 { last } else { None }
}

fn show_card_beside_settings(epoch: u64, settings: (f64, f64, f64, f64)) {
    if !GUIDE.is_current(epoch) {
        return;
    }
    let screen = screen_containing(settings.0 + settings.2 / 2.0, settings.1 + settings.3 / 2.0);
    let docked = docked_card_frame(settings, screen);
    let points_right = card_is_left_of(settings, docked);
    if panel_ptr().is_none() {
        present_card_at(NSPoint::new(docked.0, docked.1), points_right);
    } else if let Some(panel) = panel_ptr() {
        let frame = NSRect::new(NSPoint::new(docked.0, docked.1), NSSize::new(CARD_WIDTH, CARD_HEIGHT));
        unsafe {
            let _: () = msg_send![panel, setFrame: frame display: YES];
        }
        update_arrow(points_right);
    }
    schedule_tick(epoch);
}

fn present_card_at(origin: NSPoint, points_right: bool) {
    if !GUIDE.is_active() {
        return;
    }
    if panel_ptr().is_some() {
        return;
    }
    register_classes();

    let screen = screen_containing(origin.x, origin.y);
    let x = origin.x.clamp(screen.0 + 12.0, screen.0 + screen.2 - CARD_WIDTH - 12.0);
    let y = origin
        .y
        .clamp(screen.1 + 12.0, screen.1 + screen.3 - CARD_HEIGHT - 12.0);
    let start = NSRect::new(NSPoint::new(x, y), NSSize::new(CARD_WIDTH, CARD_HEIGHT));

    let Some(cls) = Class::get("ECAccessibilityGuidePanel") else {
        return;
    };
    unsafe {
        let panel: id = msg_send![cls, alloc];
        let panel: id = msg_send![
            panel,
            initWithContentRect: start
            styleMask: NS_WINDOW_STYLE_NONACTIVATING_PANEL
            backing: 2u64
            defer: NO
        ];
        let _: () = msg_send![panel, setReleasedWhenClosed: NO];
        let _: () = msg_send![panel, setOpaque: NO];
        let _: () = msg_send![panel, setHasShadow: YES];
        let _: () = msg_send![panel, setHidesOnDeactivate: NO];
        let _: () = msg_send![panel, setFloatingPanel: YES];
        let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: YES];
        let _: () = msg_send![panel, setLevel: NS_MODAL_PANEL_WINDOW_LEVEL];
        let behavior: u64 = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
            | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY;
        let _: () = msg_send![panel, setCollectionBehavior: behavior];
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![panel, setBackgroundColor: clear];
        let _: () = msg_send![panel, setAnimationBehavior: NS_WINDOW_ANIMATION_BEHAVIOR_NONE];
        let _: () = msg_send![panel, setIgnoresMouseEvents: NO];

        let content: id = msg_send![panel, contentView];
        autoreleasepool(|| build_card_content(content));

        *PANEL.lock().unwrap_or_else(|err| err.into_inner()) = Some(panel as usize);
        update_arrow(points_right);
        let _: () = msg_send![panel, orderFrontRegardless];
    }
    debug!("accessibility guide card shown");
}

fn build_card_content(content: id) {
    unsafe {
        let bounds: NSRect = msg_send![content, bounds];
        let effect_cls = Class::get("ECAccessibilityCardView").unwrap_or_else(|| class!(NSVisualEffectView));
        let effect: id = msg_send![effect_cls, alloc];
        let effect: id = msg_send![effect, initWithFrame: bounds];
        let _: () = msg_send![effect, setMaterial: NS_VISUAL_EFFECT_MATERIAL_POPOVER];
        let _: () = msg_send![effect, setBlendingMode: NS_VISUAL_EFFECT_BLENDING_BEHIND_WINDOW];
        let _: () = msg_send![effect, setState: NS_VISUAL_EFFECT_STATE_ACTIVE];
        let _: () = msg_send![effect, setAutoresizingMask: 18u64];
        let _: () = msg_send![effect, setWantsLayer: YES];
        let layer: id = msg_send![effect, layer];
        let _: () = msg_send![layer, setCornerRadius: 16.0f64];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        adopt_subview(content, effect);
        let _: () = msg_send![content, setWantsLayer: YES];
        let content_layer: id = msg_send![content, layer];
        let _: () = msg_send![content_layer, setCornerRadius: 16.0f64];
        let _: () = msg_send![content_layer, setMasksToBounds: YES];

        let zh = prefers_zh();
        let title = if zh {
            "授予辅助功能"
        } else {
            "Grant Accessibility"
        };
        let body = if zh {
            "把下面的图标拖进旁边的应用列表，然后打开开关。"
        } else {
            "Drag the icon into the app list beside this card, then turn it on."
        };

        let title_y = CARD_HEIGHT - CARD_MARGIN - TITLE_HEIGHT;
        let close_x = CARD_WIDTH - CARD_MARGIN - CLOSE_SIZE;
        let title_label = add_label(
            effect,
            title,
            15.0,
            true,
            NSRect::new(
                NSPoint::new(CARD_MARGIN, title_y),
                NSSize::new(close_x - CARD_MARGIN - 8.0, TITLE_HEIGHT),
            ),
        );
        let body_y = CHIP_Y + CHIP_HEIGHT + 12.0;
        let body_height = (title_y - 8.0 - body_y).max(36.0);
        add_label(
            effect,
            body,
            12.0,
            false,
            NSRect::new(
                NSPoint::new(CARD_MARGIN, body_y),
                NSSize::new(CARD_WIDTH - CARD_MARGIN * 2.0, body_height),
            ),
        );
        let close = add_close_button(
            effect,
            NSRect::new(NSPoint::new(close_x, title_y), NSSize::new(CLOSE_SIZE, CLOSE_SIZE)),
        );
        // The title's frame is only a placeholder. Center its text on the
        // close button so the two sit on one line.
        pin_center_y(title_label, close);
        pin_anchor(
            msg_send![title_label, leadingAnchor],
            msg_send![effect, leadingAnchor],
            CARD_MARGIN,
        );
        pin_constant(msg_send![title_label, widthAnchor], close_x - CARD_MARGIN - 8.0);
        let _: () = msg_send![title_label, setUsesSingleLineMode: YES];
        let _: () = msg_send![title_label, setMaximumNumberOfLines: 1i64];
        let _: () = msg_send![title_label, setLineBreakMode: 4i64];
        add_arrow(effect);
        let row = guide_row_frames(true);
        add_drag_row(effect, rect_from_tuple(row.chip));
        let _: () = msg_send![content, layoutSubtreeIfNeeded];
    }
}

fn add_label(parent: id, text: &str, size: f64, bold: bool, frame: NSRect) -> id {
    unsafe {
        let label: id = msg_send![class!(NSTextField), labelWithString: ns_string(text)];
        let _: () = msg_send![label, setFrame: frame];
        let weight = if bold {
            NS_FONT_WEIGHT_SEMIBOLD
        } else {
            NS_FONT_WEIGHT_REGULAR
        };
        let font: id = msg_send![class!(NSFont), systemFontOfSize: size weight: weight];
        if !font.is_null() {
            let _: () = msg_send![label, setFont: font];
        }
        let color: id = if bold {
            msg_send![class!(NSColor), labelColor]
        } else {
            msg_send![class!(NSColor), secondaryLabelColor]
        };
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let _: () = msg_send![label, setLineBreakMode: 0i64];
        let _: () = msg_send![label, setUsesSingleLineMode: NO];
        let _: () = msg_send![label, setMaximumNumberOfLines: 3i64];
        let cell: id = msg_send![label, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setWraps: YES];
        }
        let _: () = msg_send![parent, addSubview: label];
        label
    }
}

fn add_close_button(parent: id, frame: NSRect) -> id {
    let button_cls = Class::get("ECAccessibilityCloseButton").unwrap_or_else(|| class!(NSButton));
    unsafe {
        let button: id = msg_send![button_cls, new];
        let symbol: id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("xmark")
            accessibilityDescription: nil
        ];
        if symbol.is_null() {
            let _: () = msg_send![button, setTitle: ns_string("✕")];
        } else {
            let _: () = msg_send![button, setImage: symbol];
            let _: () = msg_send![button, setTitle: ns_string("")];
            let _: () = msg_send![button, setImageScaling: NS_IMAGE_SCALE_PROPORTIONALLY];
        }
        let _: () = msg_send![button, setBezelStyle: 1u64];
        let _: () = msg_send![button, setBordered: NO];
        let _: () = msg_send![button, setImagePosition: 1u64];
        let _: () = msg_send![button, setFrame: frame];
        let target = close_target();
        let _: () = msg_send![button, setTarget: target];
        let _: () = msg_send![button, setAction: sel!(closeGuide:)];
        adopt_subview(parent, button);
        button
    }
}

fn add_arrow(parent: id) {
    unsafe {
        let view: id = msg_send![class!(NSImageView), new];
        let _: () = msg_send![view, setTag: ARROW_TAG];
        let _: () = msg_send![view, setHidden: YES];
        let _: () = msg_send![view, setEditable: NO];
        let _: () = msg_send![view, setImageAlignment: NS_IMAGE_ALIGN_CENTER];
        let _: () = msg_send![view, setImageScaling: NS_IMAGE_SCALE_PROPORTIONALLY];
        let _: () = msg_send![view, setWantsLayer: YES];
        let tint: id = msg_send![class!(NSColor), secondaryLabelColor];
        let _: () = msg_send![view, setContentTintColor: tint];
        let _: () = msg_send![parent, addSubview: view];
        let _: () = msg_send![view, release];
    }
}

fn update_arrow(points_right: bool) {
    let Some(panel) = panel_ptr() else {
        return;
    };
    let direction = if points_right { 1 } else { -1 };
    let direction_changed = ARROW_DIRECTION.swap(direction, Ordering::SeqCst) != direction;
    let frames = guide_row_frames(points_right);
    autoreleasepool(|| unsafe {
        if let Some(chip) = chip_ptr() {
            let _: () = msg_send![chip, setFrame: rect_from_tuple(frames.chip)];
        }
        let content: id = msg_send![panel, contentView];
        if content.is_null() {
            return;
        }
        let arrow: id = msg_send![content, viewWithTag: ARROW_TAG];
        if arrow.is_null() {
            return;
        }
        let _: () = msg_send![arrow, setFrame: rect_from_tuple(frames.arrow)];
        let _: () = msg_send![arrow, setHidden: NO];
        if direction_changed {
            let symbol = arrow_symbol(points_right);
            if !symbol.is_null() {
                let _: () = msg_send![arrow, setImage: symbol];
            }
            animate_guide_arrow(arrow, points_right);
        }
    });
}

/// Nudge the arrow toward the Accessibility list, then back, for as long as
/// the card is up. The chip itself stays still so the drag gesture does not
/// start from a moving target.
fn animate_guide_arrow(view: id, points_right: bool) {
    let Some(cls) = Class::get("CABasicAnimation") else {
        return;
    };
    unsafe {
        let layer: id = msg_send![view, layer];
        if layer.is_null() {
            return;
        }
        let key = ns_string("nudge");
        let _: () = msg_send![layer, removeAnimationForKey: key];
        let anim: id = msg_send![cls, animationWithKeyPath: ns_string("transform.translation.x")];
        if anim.is_null() {
            return;
        }
        let distance = if points_right { 6.0 } else { -6.0 };
        let from: id = msg_send![class!(NSNumber), numberWithDouble: 0.0f64];
        let to: id = msg_send![class!(NSNumber), numberWithDouble: distance];
        let _: () = msg_send![anim, setFromValue: from];
        let _: () = msg_send![anim, setToValue: to];
        let _: () = msg_send![anim, setDuration: 0.85f64];
        let _: () = msg_send![anim, setAutoreverses: YES];
        let _: () = msg_send![anim, setRepeatCount: 1.0e9_f32];
        if let Some(timing) = Class::get("CAMediaTimingFunction") {
            let ease: id = msg_send![timing, functionWithName: ns_string("easeInEaseOut")];
            if !ease.is_null() {
                let _: () = msg_send![anim, setTimingFunction: ease];
            }
        }
        let _: () = msg_send![layer, addAnimation: anim forKey: key];
    }
}

fn arrow_symbol(points_right: bool) -> id {
    let name = if points_right { "arrow.right" } else { "arrow.left" };
    unsafe {
        let image: id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string(name)
            accessibilityDescription: nil
        ];
        if image.is_null() {
            return nil;
        }
        let Some(config_cls) = Class::get("NSImageSymbolConfiguration") else {
            return image;
        };
        let config: id = msg_send![
            config_cls,
            configurationWithPointSize: 16.0f64
            weight: NS_FONT_WEIGHT_SEMIBOLD
        ];
        if config.is_null() {
            return image;
        }
        let styled: id = msg_send![image, imageWithSymbolConfiguration: config];
        if styled.is_null() { image } else { styled }
    }
}

fn add_drag_row(parent: id, frame: NSRect) {
    let Some(cls) = Class::get("ECAccessibilityDragView") else {
        return;
    };
    let Some(bundle) = get_bundle_path() else {
        return;
    };
    let name = app_display_name();
    unsafe {
        let view: id = msg_send![cls, alloc];
        let view: id = msg_send![view, initWithFrame: frame];
        let path = owned_ns_string(&bundle.to_string_lossy());
        (*view).set_ivar("bundlePath", path);
        let _: () = msg_send![view, setWantsLayer: YES];
        let _: () = msg_send![view, setOpaque: NO];
        let layer: id = msg_send![view, layer];
        let _: () = msg_send![layer, setCornerRadius: DRAG_CHIP_RADIUS];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        style_drag_row_layer(layer, false);

        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let shared_icon: id = msg_send![workspace, iconForFile: path];
        // `iconForFile` hands back a cached image. Resizing that cache makes
        // every later icon in this process draw at the chip size.
        let owned_icon: id = msg_send![shared_icon, copy];
        let icon = if owned_icon.is_null() { shared_icon } else { owned_icon };
        if !owned_icon.is_null() {
            let _: () = msg_send![icon, setSize: NSSize::new(CHIP_ICON, CHIP_ICON)];
        }
        let image_view: id = msg_send![class!(NSImageView), new];
        let _: () = msg_send![image_view, setImage: icon];
        let _: () = msg_send![image_view, setEditable: NO];
        let _: () = msg_send![image_view, setImageAlignment: NS_IMAGE_ALIGN_CENTER];
        let _: () = msg_send![image_view, setImageScaling: NS_IMAGE_SCALE_PROPORTIONALLY];
        if !owned_icon.is_null() {
            let _: () = msg_send![owned_icon, release];
        }
        adopt_subview(view, image_view);

        let label: id = msg_send![class!(NSTextField), labelWithString: ns_string(&name)];
        let font: id = msg_send![class!(NSFont), systemFontOfSize: 13.0f64 weight: NS_FONT_WEIGHT_SEMIBOLD];
        if !font.is_null() {
            let _: () = msg_send![label, setFont: font];
        }
        let color: id = msg_send![class!(NSColor), labelColor];
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let _: () = msg_send![label, setLineBreakMode: 4i64];
        let _: () = msg_send![label, setUsesSingleLineMode: YES];
        let _: () = msg_send![label, setMaximumNumberOfLines: 1i64];
        // labelWithString: is already autoreleased. adopt_subview would release
        // it again, and the card's pool then frees the label while it is still
        // in the window. That aborts the process as soon as the grant card is built.
        let _: () = msg_send![view, addSubview: label];

        pin_center_y(image_view, view);
        pin_center_y(label, view);
        pin_anchor(
            msg_send![image_view, leadingAnchor],
            msg_send![view, leadingAnchor],
            10.0,
        );
        pin_constant(msg_send![image_view, widthAnchor], CHIP_ICON);
        pin_constant(msg_send![image_view, heightAnchor], CHIP_ICON);
        pin_anchor(
            msg_send![label, leadingAnchor],
            msg_send![image_view, trailingAnchor],
            8.0,
        );
        pin_anchor(msg_send![label, trailingAnchor], msg_send![view, trailingAnchor], -12.0);

        *CHIP.lock().unwrap_or_else(|err| err.into_inner()) = Some(view as usize);
        adopt_subview(parent, view);
    }
}

fn register_classes() {
    REGISTER_CLASSES.call_once(|| {
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityGuidePanel", class!(NSPanel)) {
            unsafe {
                decl.add_method(
                    sel!(canBecomeKeyWindow),
                    can_become_key as extern "C" fn(&Object, Sel) -> BOOL,
                );
                decl.add_method(
                    sel!(canBecomeMainWindow),
                    can_become_main as extern "C" fn(&Object, Sel) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityCardView", class!(NSVisualEffectView)) {
            unsafe {
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityCloseButton", class!(NSButton)) {
            unsafe {
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityDragView", class!(NSView)) {
            decl.add_ivar::<id>("bundlePath");
            decl.add_ivar::<BOOL>("didStartDrag");
            unsafe {
                decl.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(&mut Object, Sel, id));
                decl.add_method(
                    sel!(mouseDragged:),
                    mouse_dragged as extern "C" fn(&mut Object, Sel, id),
                );
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
                decl.add_method(sel!(hitTest:), hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id);
                decl.add_method(
                    sel!(resetCursorRects),
                    reset_cursor_rects as extern "C" fn(&Object, Sel),
                );
                decl.add_method(
                    sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                    source_operation_mask as extern "C" fn(&Object, Sel, id, isize) -> usize,
                );
                decl.add_method(
                    sel!(draggingSession:endedAtPoint:operation:),
                    drag_ended as extern "C" fn(&Object, Sel, id, NSPoint, usize),
                );
                decl.add_method(sel!(dealloc), drag_dealloc as extern "C" fn(&mut Object, Sel));
            }
            decl.register();
        }
    });
}

extern "C" fn can_become_key(_this: &Object, _sel: Sel) -> BOOL {
    YES
}

extern "C" fn can_become_main(_this: &Object, _sel: Sel) -> BOOL {
    NO
}

fn view_id(this: &Object) -> id {
    this as *const Object as id
}

extern "C" fn mouse_down(this: &mut Object, _sel: Sel, _event: id) {
    unsafe {
        this.set_ivar("didStartDrag", NO);
    }
}

extern "C" fn mouse_dragged(this: &mut Object, _sel: Sel, event: id) {
    unsafe {
        let path: id = *this.get_ivar("bundlePath");
        if path.is_null() {
            return;
        }
        let started: BOOL = *this.get_ivar("didStartDrag");
        if started == YES {
            return;
        }
        this.set_ivar("didStartDrag", YES);
        let this_id = this as *mut Object as id;
        DRAGGING.store(true, Ordering::SeqCst);
        if !begin_url_drag(this_id, path, event) {
            let bounds: NSRect = msg_send![this_id, bounds];
            let _: BOOL = msg_send![this_id, dragFile: path fromRect: bounds slideBack: YES event: event];
            finish_drag();
        }
    }
}

fn begin_url_drag(view: id, path: id, event: id) -> bool {
    let Some(_) = Class::get("NSDraggingItem") else {
        return false;
    };
    autoreleasepool(|| unsafe {
        let url: id = msg_send![class!(NSURL), fileURLWithPath: path];
        if url.is_null() {
            return false;
        }
        let item: id = msg_send![class!(NSDraggingItem), alloc];
        let item: id = msg_send![item, initWithPasteboardWriter: url];
        if item.is_null() {
            return false;
        }
        let bounds: NSRect = msg_send![view, bounds];
        let preview = drag_preview_image(view);
        if preview.is_null() {
            let _: () = msg_send![item, release];
            return false;
        }
        let _: () = msg_send![item, setDraggingFrame: bounds contents: preview];
        let _: () = msg_send![preview, release];
        let items: id = msg_send![class!(NSArray), arrayWithObject: item];
        let _: () = msg_send![item, release];
        let session: id = msg_send![view, beginDraggingSessionWithItems: items event: event source: view];
        !session.is_null()
    })
}

fn style_drag_row_layer(layer: id, for_preview: bool) {
    if layer.is_null() {
        return;
    }
    unsafe {
        let dark = is_dark_appearance();
        let fill: id = if for_preview {
            if dark {
                msg_send![class!(NSColor), colorWithWhite: 0.22f64 alpha: 0.94f64]
            } else {
                msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.94f64]
            }
        } else if dark {
            msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.10f64]
        } else {
            msg_send![class!(NSColor), colorWithWhite: 0.0f64 alpha: 0.06f64]
        };
        let cg: id = msg_send![fill, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
        if for_preview {
            let stroke: id = if dark {
                msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.22f64]
            } else {
                msg_send![class!(NSColor), colorWithWhite: 0.0f64 alpha: 0.12f64]
            };
            let stroke_cg: id = msg_send![stroke, CGColor];
            let _: () = msg_send![layer, setBorderColor: stroke_cg];
            let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        } else {
            let stroke: id = if dark {
                msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.16f64]
            } else {
                msg_send![class!(NSColor), colorWithWhite: 0.0f64 alpha: 0.10f64]
            };
            let stroke_cg: id = msg_send![stroke, CGColor];
            let _: () = msg_send![layer, setBorderColor: stroke_cg];
            let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        }
    }
}

fn backing_scale_for_view(view: id) -> f64 {
    unsafe {
        let window: id = msg_send![view, window];
        if !window.is_null() {
            let screen: id = msg_send![window, screen];
            if !screen.is_null() {
                let scale: f64 = msg_send![screen, backingScaleFactor];
                if scale > 0.0 {
                    return scale;
                }
            }
        }
        let screens: id = msg_send![class!(NSScreen), screens];
        if !screens.is_null() {
            let count: usize = msg_send![screens, count];
            if count > 0 {
                let screen: id = msg_send![screens, objectAtIndex: 0usize];
                let scale: f64 = msg_send![screen, backingScaleFactor];
                if scale > 0.0 {
                    return scale;
                }
            }
        }
    }
    2.0
}

/// Snapshot the live icon+name row via the layer tree (rounded fill included).
fn drag_preview_image(view: id) -> id {
    unsafe {
        let bounds: NSRect = msg_send![view, bounds];
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            return nil;
        }
        let layer: id = msg_send![view, layer];
        if layer.is_null() {
            return nil;
        }
        style_drag_row_layer(layer, true);
        let _: () = msg_send![view, layoutSubtreeIfNeeded];
        let _: () = msg_send![class!(CATransaction), flush];
        let image = render_layer_preview(layer, view, bounds.size);
        style_drag_row_layer(layer, false);
        image
    }
}

fn render_layer_preview(layer: id, view: id, size: NSSize) -> id {
    let scale = backing_scale_for_view(view).max(1.0);
    let px_w = (size.width * scale).round().max(1.0) as i64;
    let px_h = (size.height * scale).round().max(1.0) as i64;
    unsafe {
        let rep: id = msg_send![class!(NSBitmapImageRep), alloc];
        let color_space = ns_string("NSCalibratedRGBColorSpace");
        let rep: id = msg_send![
            rep,
            initWithBitmapDataPlanes: nil
            pixelsWide: px_w
            pixelsHigh: px_h
            bitsPerSample: 8i64
            samplesPerPixel: 4i64
            hasAlpha: YES
            isPlanar: NO
            colorSpaceName: color_space
            bytesPerRow: 0i64
            bitsPerPixel: 0i64
        ];
        if rep.is_null() {
            return nil;
        }
        let nsctx: id = msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        if nsctx.is_null() {
            let _: () = msg_send![rep, release];
            return nil;
        }
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: nsctx];
        let cg_ptr: core_graphics::sys::CGContextRef = msg_send![nsctx, CGContext];
        if cg_ptr.is_null() {
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
            let _: () = msg_send![rep, release];
            return nil;
        }
        let cg = CGContext::from_existing_context_ptr(cg_ptr);
        cg.clear_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(px_w as f64, px_h as f64),
        ));
        // Bitmap and macOS CALayer are both bottom-left. Map pixels to points; do not Y-flip.
        cg.scale(scale, scale);
        let _: () = msg_send![layer, renderInContext: cg_ptr];
        drop(cg);
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
        let _: () = msg_send![rep, setSize: size];
        let image: id = msg_send![class!(NSImage), alloc];
        let image: id = msg_send![image, initWithSize: size];
        if image.is_null() {
            let _: () = msg_send![rep, release];
            return nil;
        }
        let _: () = msg_send![image, addRepresentation: rep];
        let _: () = msg_send![rep, release];
        image
    }
}

fn tcc_bundle_id_is_safe(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && bundle_id.len() <= 128
        && bundle_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn tccutil_reset_args(bundle_id: &str) -> Option<[&str; 3]> {
    tcc_bundle_id_is_safe(bundle_id).then_some(["reset", "Accessibility", bundle_id])
}

fn clear_stale_accessibility_row() {
    let Some(bundle_id) = get_bundle_identifier() else {
        return;
    };
    let Some(args) = tccutil_reset_args(&bundle_id) else {
        return;
    };
    match Command::new("/usr/bin/tccutil")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => debug!("cleared stale Accessibility TCC row"),
        Ok(status) => debug!(?status, "tccutil reset Accessibility returned non-zero"),
        Err(err) => debug!(%err, "tccutil reset Accessibility failed"),
    }
}

extern "C" fn source_operation_mask(_this: &Object, _sel: Sel, _session: id, _context: isize) -> usize {
    NS_DRAG_OPERATION_COPY
}

extern "C" fn drag_ended(_this: &Object, _sel: Sel, _session: id, _point: NSPoint, _op: usize) {
    finish_drag();
}

fn finish_drag() {
    DRAGGING.store(false, Ordering::SeqCst);
    if GUIDE.is_active() && accessibility_is_enabled() {
        dismiss_guide();
    }
}

extern "C" fn accepts_first_mouse(_this: &Object, _sel: Sel, _event: id) -> BOOL {
    YES
}

extern "C" fn hit_test(this: &Object, _sel: Sel, point: NSPoint) -> id {
    unsafe {
        let this_id = view_id(this);
        let superview: id = msg_send![this_id, superview];
        let local: NSPoint = msg_send![this_id, convertPoint: point fromView: superview];
        let bounds: NSRect = msg_send![this_id, bounds];
        let inside: BOOL = msg_send![this_id, mouse: local inRect: bounds];
        if inside == YES { this_id } else { nil }
    }
}

extern "C" fn drag_dealloc(this: &mut Object, _sel: Sel) {
    unsafe {
        let path: id = *this.get_ivar("bundlePath");
        if !path.is_null() {
            this.set_ivar("bundlePath", nil);
            let _: () = msg_send![path, release];
        }
        let _: () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

extern "C" fn reset_cursor_rects(this: &Object, _sel: Sel) {
    unsafe {
        let this_id = view_id(this);
        let bounds: NSRect = msg_send![this_id, bounds];
        let cursor: id = msg_send![class!(NSCursor), openHandCursor];
        let _: () = msg_send![this_id, addCursorRect: bounds cursor: cursor];
    }
}

fn close_target() -> id {
    static TARGET: OnceLock<usize> = OnceLock::new();
    let ptr = *TARGET.get_or_init(|| {
        let Some(mut decl) = ClassDecl::new("ECAccessibilityGuideTarget", class!(NSObject)) else {
            return 0;
        };
        unsafe {
            decl.add_method(sel!(closeGuide:), close_guide as extern "C" fn(&Object, Sel, id));
        }
        let cls = decl.register();
        let obj: id = unsafe { msg_send![cls, new] };
        obj as usize
    });
    ptr as id
}

extern "C" fn close_guide(_this: &Object, _sel: Sel, _sender: id) {
    dismiss_guide();
}

fn schedule_tick(epoch: u64) {
    dispatch::Queue::main().exec_after(Duration::from_millis(40), move || {
        if !GUIDE.is_current(epoch) {
            return;
        }
        if DRAGGING.load(Ordering::SeqCst) {
            schedule_tick(epoch);
            return;
        }
        if accessibility_is_enabled() {
            dismiss_guide();
            return;
        }
        match settings_window_cocoa() {
            Some(settings) => {
                SETTINGS_MISSING.store(0, Ordering::SeqCst);
                let screen = screen_containing(settings.0 + settings.2 / 2.0, settings.1 + settings.3 / 2.0);
                let docked = docked_card_frame(settings, screen);
                update_arrow(card_is_left_of(settings, docked));
                let target = NSRect::new(NSPoint::new(docked.0, docked.1), NSSize::new(docked.2, docked.3));
                if let Some(panel) = panel_ptr() {
                    unsafe {
                        let current: NSRect = msg_send![panel, frame];
                        if !rects_close(current, target) {
                            let _: () = msg_send![panel, setFrame: target display: YES];
                        }
                    }
                }
            },
            None => {
                let gone = SETTINGS_MISSING.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                if gone >= SETTINGS_GONE_TICKS {
                    dismiss_guide();
                    return;
                }
            },
        }
        schedule_tick(epoch);
    });
}

fn dismiss_guide() {
    GUIDE.dismiss();
    DRAGGING.store(false, Ordering::SeqCst);
    SETTINGS_MISSING.store(0, Ordering::SeqCst);
    ARROW_DIRECTION.store(0, Ordering::SeqCst);
    *CHIP.lock().unwrap_or_else(|err| err.into_inner()) = None;
    dismiss_panel_only();
}

fn dismiss_panel_only() {
    let panel = PANEL.lock().unwrap_or_else(|err| err.into_inner()).take();
    if let Some(panel) = panel {
        let panel = panel as id;
        unsafe {
            let _: () = msg_send![panel, orderOut: nil];
            let _: () = msg_send![panel, close];
            let _: () = msg_send![panel, release];
        }
    }
}

fn panel_ptr() -> Option<id> {
    (*PANEL.lock().unwrap_or_else(|err| err.into_inner())).map(|ptr| ptr as id)
}

fn on_main_thread() -> bool {
    unsafe { msg_send![class!(NSThread), isMainThread] }
}

fn prefers_zh() -> bool {
    match PREFER_ZH.load(Ordering::SeqCst) {
        0 => false,
        1 => true,
        _ => system_prefers_zh(),
    }
}

fn system_prefers_zh() -> bool {
    unsafe {
        let langs: id = msg_send![class!(NSLocale), preferredLanguages];
        if langs.is_null() {
            return false;
        }
        let count: usize = msg_send![langs, count];
        if count == 0 {
            return false;
        }
        let first: id = msg_send![langs, objectAtIndex: 0usize];
        let utf8: *const i8 = msg_send![first, UTF8String];
        if utf8.is_null() {
            return false;
        }
        CStr::from_ptr(utf8).to_string_lossy().starts_with("zh")
    }
}

fn is_dark_appearance() -> bool {
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return false;
        }
        let appearance: id = msg_send![app, effectiveAppearance];
        if appearance.is_null() {
            return false;
        }
        let name: id = msg_send![appearance, name];
        if name.is_null() {
            return false;
        }
        let utf8: *const i8 = msg_send![name, UTF8String];
        if utf8.is_null() {
            return false;
        }
        CStr::from_ptr(utf8).to_string_lossy().contains("Dark")
    }
}

fn app_display_name() -> String {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        for key in ["CFBundleDisplayName", "CFBundleName"] {
            let name: id = msg_send![bundle, objectForInfoDictionaryKey: ns_string(key)];
            if !name.is_null() {
                let utf8: *const i8 = msg_send![name, UTF8String];
                if !utf8.is_null() {
                    let value = CStr::from_ptr(utf8).to_string_lossy().into_owned();
                    if !value.is_empty() {
                        return value;
                    }
                }
            }
        }
    }
    "FasTab".into()
}

/// `addSubview:` retains. Drop the extra `alloc`/`new` retain so releasing
/// the panel can actually `dealloc` the card.
///
/// Only call this with an owned (+1) object (`alloc`/`new`). An autoreleased
/// object such as `labelWithString:` must be added with `addSubview:` alone.
fn adopt_subview(parent: id, child: id) {
    if child.is_null() {
        return;
    }
    unsafe {
        let _: () = msg_send![parent, addSubview: child];
        let _: () = msg_send![child, release];
    }
}

fn ns_string(text: &str) -> id {
    unsafe {
        let string = NSString::alloc(nil).init_str(text);
        msg_send![string, autorelease]
    }
}

fn owned_ns_string(text: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(text) }
}

fn settings_window_cocoa() -> Option<(f64, f64, f64, f64)> {
    let pids: Vec<i32> = ["com.apple.systempreferences", "com.apple.Settings"]
        .iter()
        .flat_map(|id| running_application_pids(id))
        .collect();
    let info = unsafe {
        CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
    };
    if info.is_null() {
        return None;
    }
    let windows = unsafe { core_foundation::array::CFArray::<CFDictionary>::wrap_under_create_rule(info) };
    let primary_h = primary_screen_height();
    let mut best: Option<(f64, CGRect)> = None;
    for window in windows.iter() {
        let owner = unsafe { dict_i64(&window, kCGWindowOwnerPID) }.unwrap_or(0) as i32;
        let name = unsafe { dict_string(&window, kCGWindowOwnerName) }.unwrap_or_default();
        let looks_like_settings = pids.contains(&owner)
            || name.contains("Settings")
            || name.contains("Preferences")
            || name.contains("系统设置")
            || name.contains("系统偏好");
        if !looks_like_settings {
            continue;
        }
        let Some(bounds) = (unsafe { dict_rect(&window, kCGWindowBounds) }) else {
            continue;
        };
        let area = bounds.size.width * bounds.size.height;
        if area < 20_000.0 {
            continue;
        }
        if best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, bounds));
        }
    }
    best.map(|(_, bounds)| quartz_to_cocoa(bounds, primary_h))
}

pub(crate) fn docked_card_frame(settings: (f64, f64, f64, f64), screen: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let right_x = settings.0 + settings.2 + CARD_GAP;
    let left_x = settings.0 - CARD_GAP - CARD_WIDTH;
    // The Accessibility app list is on the left of System Settings, so sit
    // there when there is room — dragging across the whole window is worse.
    let x = if left_x >= screen.0 + 12.0 {
        left_x
    } else if right_x + CARD_WIDTH <= screen.0 + screen.2 - 12.0 {
        right_x
    } else {
        left_x.max(screen.0 + 12.0)
    };
    let top = settings.1 + settings.3;
    let y = (top - CARD_HEIGHT - 52.0).max(screen.1 + 12.0);
    (x, y, CARD_WIDTH, CARD_HEIGHT)
}

fn card_is_left_of(settings: (f64, f64, f64, f64), docked: (f64, f64, f64, f64)) -> bool {
    docked.0 + docked.2 / 2.0 < settings.0 + settings.2 / 2.0
}

struct GuideRowFrames {
    chip: (f64, f64, f64, f64),
    arrow: (f64, f64, f64, f64),
}

/// The arrow sits in the margin facing the Accessibility list. The chip takes
/// the rest of the row so the two never overlap, and both share one midline.
fn guide_row_frames(points_right: bool) -> GuideRowFrames {
    let chip_w = CARD_WIDTH - CARD_MARGIN - ARROW_SIZE - ARROW_GAP - CARD_MARGIN;
    let (chip_x, arrow_x) = if points_right {
        (CARD_MARGIN, CARD_MARGIN + chip_w + ARROW_GAP)
    } else {
        (CARD_MARGIN + ARROW_SIZE + ARROW_GAP, CARD_MARGIN)
    };
    let arrow_y = CHIP_Y + (CHIP_HEIGHT - ARROW_SIZE) / 2.0;
    GuideRowFrames {
        chip: (chip_x, CHIP_Y, chip_w, CHIP_HEIGHT),
        arrow: (arrow_x, arrow_y, ARROW_SIZE, ARROW_SIZE),
    }
}

fn rect_from_tuple(frame: (f64, f64, f64, f64)) -> NSRect {
    NSRect::new(NSPoint::new(frame.0, frame.1), NSSize::new(frame.2, frame.3))
}

fn chip_ptr() -> Option<id> {
    (*CHIP.lock().unwrap_or_else(|err| err.into_inner())).map(|ptr| ptr as id)
}

fn pin_center_y(view: id, container: id) {
    unsafe {
        let _: () = msg_send![view, setTranslatesAutoresizingMaskIntoConstraints: NO];
        let anchor: id = msg_send![view, centerYAnchor];
        let other: id = msg_send![container, centerYAnchor];
        let constraint: id = msg_send![anchor, constraintEqualToAnchor: other];
        let _: () = msg_send![constraint, setActive: YES];
    }
}

fn pin_anchor(anchor: id, other: id, constant: f64) {
    unsafe {
        let constraint: id = msg_send![anchor, constraintEqualToAnchor: other constant: constant];
        let _: () = msg_send![constraint, setActive: YES];
    }
}

fn pin_constant(anchor: id, value: f64) {
    unsafe {
        let constraint: id = msg_send![anchor, constraintEqualToConstant: value];
        let _: () = msg_send![constraint, setActive: YES];
    }
}

pub(crate) fn quartz_to_cocoa(bounds: CGRect, primary_h: f64) -> (f64, f64, f64, f64) {
    (
        bounds.origin.x,
        primary_h - bounds.origin.y - bounds.size.height,
        bounds.size.width,
        bounds.size.height,
    )
}

/// `hitTest:` receives a point in the superview. Subtract the view's frame
/// origin to get the same space as `bounds`.
#[cfg(test)]
fn local_point_from_superview(point: (f64, f64), frame: (f64, f64, f64, f64)) -> (f64, f64) {
    (point.0 - frame.0, point.1 - frame.1)
}

#[cfg(test)]
fn point_in_size(point: (f64, f64), size: (f64, f64)) -> bool {
    point.0 >= 0.0 && point.0 <= size.0 && point.1 >= 0.0 && point.1 <= size.1
}

fn frames_close(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1.0 && (a.1 - b.1).abs() < 1.0 && (a.2 - b.2).abs() < 1.0 && (a.3 - b.3).abs() < 1.0
}

fn rects_close(a: NSRect, b: NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 1.0
        && (a.origin.y - b.origin.y).abs() < 1.0
        && (a.size.width - b.size.width).abs() < 1.0
        && (a.size.height - b.size.height).abs() < 1.0
}

fn primary_screen_height() -> f64 {
    primary_screen_frame().3
}

fn primary_screen_frame() -> (f64, f64, f64, f64) {
    screen_at(0)
}

fn screen_containing(x: f64, y: f64) -> (f64, f64, f64, f64) {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        for index in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: index];
            let frame: NSRect = msg_send![screen, frame];
            if x >= frame.origin.x
                && x <= frame.origin.x + frame.size.width
                && y >= frame.origin.y
                && y <= frame.origin.y + frame.size.height
            {
                return (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height);
            }
        }
    }
    primary_screen_frame()
}

fn screen_at(index: usize) -> (f64, f64, f64, f64) {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return (0.0, 0.0, 1440.0, 900.0);
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return (0.0, 0.0, 1440.0, 900.0);
        }
        let screen: id = msg_send![screens, objectAtIndex: index.min(count - 1)];
        let frame: NSRect = msg_send![screen, frame];
        (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height)
    }
}

fn dict_i64(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let num = cf_type.downcast::<CFNumber>()?;
    num.to_i64().or_else(|| num.to_i32().map(i64::from))
}

fn dict_string(dict: &CFDictionary, key: CFStringRef) -> Option<String> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let string = cf_type.downcast::<CFString>()?;
    Some(string.to_string())
}

fn dict_rect(dict: &CFDictionary, key: CFStringRef) -> Option<CGRect> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let bounds = cf_type.downcast::<CFDictionary>()?;
    CGRect::from_dict_representation(&bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::geometry::{CGPoint, CGSize};

    #[test]
    fn guide_reopening_does_not_revive_previous_delayed_callbacks() {
        let guide = GuideLifecycle::new();
        assert!(!guide.is_active());
        let first = guide.begin().unwrap();
        assert!(guide.is_current(first));
        assert_eq!(guide.begin(), None, "repeat Grant must keep one polling loop");
        assert!(guide.is_current(first));

        guide.dismiss();
        assert!(!guide.is_active());
        assert!(!guide.is_current(first));
        let second = guide.begin().unwrap();
        assert!(guide.is_active());
        assert!(guide.is_current(second));
        assert!(!guide.is_current(first), "old wait/tick must stop after reopening");

        guide.dismiss();
        guide.dismiss();
        assert!(!guide.is_current(second));
        let third = guide.begin().unwrap();
        assert!(guide.is_current(third));
        assert!(!guide.is_current(first));
        assert!(!guide.is_current(second));
    }

    #[test]
    fn docks_to_the_right_when_the_list_side_does_not_fit() {
        let settings = (100.0, 80.0, 700.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!((frame.0 - (100.0 + 700.0 + CARD_GAP)).abs() < 0.1);
        assert_eq!(frame.2, CARD_WIDTH);
        assert_eq!(frame.3, CARD_HEIGHT);
        assert!(!card_is_left_of(settings, frame));
    }

    #[test]
    fn prefers_the_list_side_when_both_edges_fit() {
        let settings = (400.0, 80.0, 700.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!(card_is_left_of(settings, frame));
        assert!((frame.0 - (400.0 - CARD_GAP - CARD_WIDTH)).abs() < 0.1);
    }

    #[test]
    fn docks_to_the_left_when_the_right_edge_overflows() {
        let settings = (1100.0, 80.0, 320.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!(frame.0 < settings.0);
        assert!(frame.0 + frame.2 <= settings.0);
    }

    #[test]
    fn quartz_origin_flips_against_the_menu_bar_display() {
        let bounds = CGRect::new(&CGPoint::new(10.0, 20.0), &CGSize::new(100.0, 50.0));
        assert_eq!(quartz_to_cocoa(bounds, 900.0), (10.0, 830.0, 100.0, 50.0));
    }

    #[test]
    fn drag_pill_right_edge_hits_after_converting_out_of_superview() {
        let frame = (20.0, 16.0, 248.0, 52.0);
        let click = (260.0, 40.0);
        assert!(!point_in_size(click, (frame.2, frame.3)));
        let local = local_point_from_superview(click, frame);
        assert!(point_in_size(local, (frame.2, frame.3)));
    }

    #[test]
    fn grant_card_is_nonactivating_and_holds_still_during_drag() {
        assert_eq!(NS_WINDOW_STYLE_NONACTIVATING_PANEL, 1 << 7);
        let src = include_str!("accessibility_guide.rs");
        let tick = src
            .split("fn schedule_tick")
            .nth(1)
            .and_then(|rest| rest.split("fn dismiss_guide").next())
            .expect("schedule_tick");
        let drag = tick.find("DRAGGING").expect("schedule_tick must honor DRAGGING");
        let granted = tick
            .find("accessibility_is_enabled")
            .expect("schedule_tick checks grant");
        assert!(drag < granted);
        let wait = src
            .split("fn wait_for_settings")
            .nth(1)
            .and_then(|rest| rest.split("fn show_card_beside_settings").next())
            .expect("wait_for_settings");
        let drag = wait.find("DRAGGING").expect("wait_for_settings must honor DRAGGING");
        let granted = wait
            .find("accessibility_is_enabled")
            .expect("wait_for_settings checks grant");
        assert!(drag < granted);
    }

    #[test]
    fn grant_card_waits_for_settings_and_ignores_repeat_clicks() {
        let src = include_str!("accessibility_guide.rs");
        let start = src
            .split("fn start_guide")
            .nth(1)
            .and_then(|rest| rest.split("fn wait_for_settings").next())
            .expect("start_guide");
        assert!(
            !start.contains("present_card_at") && !start.contains("show_card_beside_settings"),
            "card must not appear until Settings has loaded"
        );
        assert!(start.contains("wait_for_settings"));
        let reentry = start
            .split("GUIDE.begin()")
            .nth(1)
            .and_then(|rest| rest.split("PREFER_ZH").next())
            .expect("already-active path");
        assert!(
            !reentry.contains("open_accessibility"),
            "repeat Grant while waiting must not reopen Settings"
        );
        let wait = src
            .split("fn wait_for_settings")
            .nth(1)
            .and_then(|rest| rest.split("fn show_card_beside_settings").next())
            .expect("wait_for_settings");
        let window = wait.find("settings_window_cocoa").expect("waits for the pane");
        let show = wait.find("show_card_beside_settings").expect("then shows the card");
        assert!(window < show);
        assert!(
            !wait.contains("fallback_settings_frame"),
            "must not flash a guessed frame when Settings never appeared"
        );
    }

    #[test]
    fn settings_ready_to_dock_needs_a_stable_or_previously_seen_window() {
        let frame = (100.0, 80.0, 700.0, 600.0);
        let moved = (120.0, 80.0, 700.0, 600.0);
        assert_eq!(settings_ready_to_dock(None, None, 0), None);
        assert_eq!(settings_ready_to_dock(Some(frame), None, 0), None);
        assert_eq!(settings_ready_to_dock(Some(frame), None, 60), None);
        assert_eq!(settings_ready_to_dock(Some(frame), Some(frame), 2), Some(frame));
        assert_eq!(settings_ready_to_dock(Some(moved), Some(frame), 60), Some(moved));
        assert_eq!(settings_ready_to_dock(None, None, 120), None);
        assert_eq!(settings_ready_to_dock(None, Some(frame), 120), Some(frame));
        assert_eq!(settings_ready_to_dock(None, Some(frame), 10), None);
    }

    #[test]
    fn tccutil_reset_requires_a_safe_bundle_id() {
        assert_eq!(
            tccutil_reset_args("app.fastab"),
            Some(["reset", "Accessibility", "app.fastab"])
        );
        assert_eq!(tccutil_reset_args(""), None);
        assert_eq!(tccutil_reset_args("foo;rm"), None);
        assert_eq!(tccutil_reset_args("a b"), None);
        assert!(tccutil_reset_args("app.fastab").unwrap().len() == 3);
    }

    #[test]
    fn grant_clears_a_stale_tcc_row_before_opening_settings() {
        let start = include_str!("accessibility_guide.rs")
            .split("fn start_guide")
            .nth(1)
            .and_then(|rest| rest.split("fn wait_for_settings").next())
            .expect("start_guide");
        let enabled = start
            .find("accessibility_is_enabled")
            .expect("start_guide bails when already granted");
        let clear = start
            .find("clear_stale_accessibility_row")
            .expect("start_guide must drop a stale list row");
        assert!(enabled < clear);
        assert!(
            start[clear..].contains("open_accessibility"),
            "reset must run before the first-start open_accessibility"
        );
    }

    #[test]
    fn drag_preview_is_the_icon_and_name_chip() {
        let drag = include_str!("accessibility_guide.rs")
            .split("fn begin_url_drag")
            .nth(1)
            .and_then(|rest| rest.split("fn style_drag_row_layer").next())
            .expect("begin_url_drag");
        assert!(drag.contains("drag_preview_image"));
        assert!(drag.contains("setDraggingFrame: bounds"));
        assert!(!drag.contains("36.0, 36.0"));
        assert_eq!(DRAG_CHIP_RADIUS, 12.0);
    }

    #[test]
    fn drag_preview_snapshots_the_live_row() {
        let src = include_str!("accessibility_guide.rs");
        let preview = src
            .split("fn drag_preview_image")
            .nth(1)
            .and_then(|rest| rest.split("fn render_layer_preview").next())
            .expect("drag_preview_image");
        let solid = preview
            .find("style_drag_row_layer(layer, true)")
            .expect("solid for snapshot");
        let flush = preview.find("CATransaction").expect("flush layer style");
        let rest = preview
            .find("style_drag_row_layer(layer, false)")
            .expect("restore rest style");
        assert!(solid < flush);
        assert!(flush < rest);
        assert!(preview.contains("flush"));
        let render = src
            .split("fn render_layer_preview")
            .nth(1)
            .and_then(|rest| rest.split("fn tcc_bundle_id_is_safe").next())
            .expect("render_layer_preview");
        assert!(render.contains("renderInContext"));
        assert!(render.contains("clear_rect"));
        assert!(!render.contains("cacheDisplayInRect"));
        assert!(!render.contains("mainScreen"));
        assert!(!render.contains("scale(1.0, -1.0)"));
        let scale = render.find("cg.scale(scale, scale)").expect("point mapping");
        let paint = render.find("renderInContext").expect("paint");
        assert!(scale < paint);
    }

    #[test]
    fn dock_icon_is_the_painted_icns() {
        let build = include_str!("../../../scripts/build-app.sh");
        assert!(build.contains("CFBundleIconFile"));
        assert!(
            !build.contains("CFBundleIconName"),
            "a layered icon name hides icon.icns on macOS 26"
        );
        assert!(!build.contains("Assets.car"));
        let dmg = include_str!("../../../scripts/make-dmg.sh");
        assert!(!dmg.contains("--volicon"));
        let logo = include_str!("../../../assets/logo.svg");
        assert!(logo.contains("#343637"));
        assert!(logo.contains("#39DB68"));
        assert!(!logo.contains("#D97757"));
    }

    #[test]
    fn guide_arrow_shares_the_chip_midline_and_switches_sides() {
        let mid = |frame: (f64, f64, f64, f64)| frame.1 + frame.3 / 2.0;
        let right = guide_row_frames(true);
        let left = guide_row_frames(false);
        assert!((mid(right.chip) - mid(right.arrow)).abs() < 0.01);
        assert!((mid(left.chip) - mid(left.arrow)).abs() < 0.01);
        assert!(right.chip.0 + right.chip.2 <= right.arrow.0);
        assert!(left.arrow.0 + left.arrow.2 <= left.chip.0);
        assert!(right.arrow.0 + right.arrow.2 <= CARD_WIDTH - CARD_MARGIN + 0.01);
        assert!(left.chip.0 + left.chip.2 <= CARD_WIDTH - CARD_MARGIN + 0.01);
        assert_eq!(TITLE_HEIGHT, CLOSE_SIZE);
    }

    #[test]
    fn drag_chip_centers_the_icon_with_the_name_and_the_title_with_the_close_button() {
        let src = include_str!("accessibility_guide.rs");
        let row = src
            .split("fn add_drag_row")
            .nth(1)
            .and_then(|rest| rest.split("fn register_classes").next())
            .expect("add_drag_row");
        assert!(row.contains("pin_center_y(image_view, view)"));
        assert!(row.contains("pin_center_y(label, view)"));
        assert!(
            !row.contains("adopt_subview(view, label)"),
            "labelWithString is autoreleased; an extra release frees it when the card pool drains"
        );
        assert!(row.contains("addSubview: label"));
        assert!(row.contains("setImageAlignment: NS_IMAGE_ALIGN_CENTER"));
        let copy = row.find("shared_icon, copy").expect("copy the shared icon");
        let resize = row.find("setSize:").expect("resize the copy");
        assert!(copy < resize);
        assert!(!row.contains("NSPoint::new(54.0, 15.0)"));
        let card = src
            .split("fn build_card_content")
            .nth(1)
            .and_then(|rest| rest.split("fn add_label").next())
            .expect("build_card_content");
        assert!(card.contains("pin_center_y(title_label, close)"));
        assert!(card.contains("NSPoint::new(close_x, title_y)"));
    }

    #[test]
    fn guide_arrow_nudges_toward_the_list_without_restarting_every_tick() {
        let src = include_str!("accessibility_guide.rs");
        let update = src
            .split("fn update_arrow")
            .nth(1)
            .and_then(|rest| rest.split("fn animate_guide_arrow").next())
            .expect("update_arrow");
        let changed = update.find("if direction_changed").expect("side change");
        let animate = update.find("animate_guide_arrow").expect("animate");
        assert!(changed < animate);
        let anim = src
            .split("fn animate_guide_arrow")
            .nth(1)
            .and_then(|rest| rest.split("fn arrow_symbol").next())
            .expect("animate");
        assert!(anim.contains("transform.translation.x"));
        assert!(anim.contains("setAutoreverses: YES"));
        assert!(anim.contains("setRepeatCount"));
        let dismiss = src
            .split("fn dismiss_guide")
            .nth(1)
            .and_then(|rest| rest.split("fn dismiss_panel_only").next())
            .expect("dismiss");
        assert!(dismiss.contains("ARROW_DIRECTION.store(0"));
    }
}
