use std::ffi::c_void;
use std::fmt;
use std::time::Instant;

use accessibility::util::ax_call;
use accessibility_sys::{
    _AXUIElementGetWindow, AXError, AXUIElement, AXUIElementCopyAttributeNames, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetAttributeValue, AXUIElementSetMessagingTimeout,
    AXValue, kAXApplicationRole, kAXBrowserRole, kAXChildrenAttribute, kAXDOMClassListAttribute,
    kAXEnhancedUserInterfaceAttribute, kAXErrorAttributeUnsupported, kAXFocusedAttribute, kAXFocusedUIElementAttribute,
    kAXFocusedWindowAttribute, kAXFrameAttribute, kAXFullScreenAttribute, kAXGroupRole,
    kAXManualAccessibilityAttribute, kAXParentAttribute, kAXRoleAttribute, kAXScrollAreaRole, kAXSubroleAttribute,
    kAXTextFieldRole, kAXWebAreaRole, pid_t,
};
use core_foundation::ConcreteCFType;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::boolean::{CFBoolean, kCFBooleanTrue};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{self, CGRect};
use core_graphics::window::{
    CGWindowID, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionAll, kCGWindowListOptionIncludingWindow, kCGWindowListOptionOnScreenOnly, kCGWindowNumber,
    kCGWindowOwnerPID,
};
use tracing::warn;

use crate::util::NSStringRef;

pub const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.25;

pub struct UIElement(AXUIElement);

impl Clone for UIElement {
    fn clone(&self) -> Self {
        UIElement::from(self.get_ref())
    }
}

impl fmt::Debug for UIElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let position = self.frame().map(|CGRect { origin, size }| {
            format!(
                "({:.3}, {:.3}) - ({:.3}, {:.3})",
                origin.x,
                origin.y,
                origin.x + size.width,
                origin.y + size.height
            )
        });
        f.debug_struct("UIElement")
            .field("role", &self.role())
            .field("position", &position)
            .finish()
    }
}

// SAFETY: Pointer AXUIElement is send + sync safe
unsafe impl Send for UIElement {}
unsafe impl Sync for UIElement {}

impl PartialEq for UIElement {
    /// `CFEqual` is the only supported way to compare accessibility elements — two refs to the
    /// same UI element are not necessarily the same pointer.
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl From<AXUIElement> for UIElement {
    fn from(ax_ref: AXUIElement) -> Self {
        let element = UIElement(ax_ref);
        element.set_messaging_timeout();
        element
    }
}

/// Borrows a reference somebody else owns (an observer callback argument, an entry of a
/// `kAXChildren` array): retains it so the wrapper's release on drop balances out.
///
/// Do **not** feed this the result of `AXUIElementCreate*` or any `Copy*` call: those hand
/// over a +1 the caller must release, and the extra retain here would pin the element
/// forever. Wrap those with [`UIElement::application`] / [`AXUIElement::wrap_under_create_rule`].
impl From<AXUIElementRef> for UIElement {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn from(ax_ref: AXUIElementRef) -> Self {
        let element = UIElement(unsafe { AXUIElement::wrap_under_get_rule(ax_ref) });
        element.set_messaging_timeout();
        element
    }
}

static XTERM_ROLES: &[&str] = &[
    kAXScrollAreaRole,
    kAXGroupRole,
    kAXWebAreaRole,
    kAXTextFieldRole,
    kAXApplicationRole,
    kAXBrowserRole,
];

type Result<T> = std::result::Result<T, AXError>;

#[derive(Debug)]
pub struct CGWindowInfo {
    pub window_id: CGWindowID,
    pub bounds: CGRect,
    pub owner_pid: u64,
    pub level: i64,
}

impl UIElement {
    /// The application element for `pid`, owned by the wrapper.
    pub fn application(pid: pid_t) -> Self {
        Self::from(unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateApplication(pid)) })
    }

    /// Bound AX requests against a slow or unresponsive observed application.
    /// The timeout is set on each wrapper because AX does not inherit the
    /// application element's messaging timeout for child elements.
    fn set_messaging_timeout(&self) {
        unsafe {
            AXUIElementSetMessagingTimeout(self.get_ref(), AX_MESSAGING_TIMEOUT_SECONDS);
        }
    }

    pub fn get_ref(&self) -> AXUIElementRef {
        self.0.as_concrete_TypeRef()
    }

    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn get_window_id(&self) -> Result<CGWindowID> {
        // TODO: cache this value on struct?
        ax_call(|window_id: *mut CGWindowID| _AXUIElementGetWindow(self.get_ref(), window_id))
    }

    fn get_attr_ref(&self, attr: &str) -> Result<CFType> {
        unsafe {
            let cf_ref = ax_call(|value_ref: *mut CFTypeRef| {
                let attr = CFString::new(attr);
                let attr_ref = attr.as_concrete_TypeRef();
                AXUIElementCopyAttributeValue(self.get_ref(), attr_ref, value_ref)
            })?;
            // `Copy` rule: the value arrives at +1 and is ours to release. Wrapping it under
            // the *get* rule retained it again, so every attribute read pinned its value for
            // the life of the process. Reading a `kAXChildren` array pins every child with
            // it, and the xterm caret walk reads one per element per keystroke: a Cursor
            // session held 211k `AXUIElement`s (~70 MB) after a few minutes of typing.
            Ok(CFType::wrap_under_create_rule(cf_ref))
        }
    }

    fn get_attr<T: ConcreteCFType>(&self, attr: &str) -> Result<T> {
        self.get_attr_ref(attr)?.downcast::<T>().ok_or(-1)
    }

    fn set_attr<T: ConcreteCFType>(&self, attr: &str, value: T::Ref) -> Result<()> {
        unsafe {
            let attr = CFString::new(attr);
            let err = AXUIElementSetAttributeValue(self.get_ref(), attr.as_concrete_TypeRef(), value.as_void_ptr());

            if err == 0 { Ok(()) } else { Err(err) }
        }
    }

    pub fn enable_screen_reader_accessibility(&self) -> Result<()> {
        unsafe {
            let res = self.set_attr::<CFBoolean>(kAXManualAccessibilityAttribute, kCFBooleanTrue);

            if matches!(res, Err(kAXErrorAttributeUnsupported)) {
                self.set_attr::<CFBoolean>(kAXEnhancedUserInterfaceAttribute, kCFBooleanTrue)
            } else {
                res
            }
        }
    }

    pub fn is_focused(&self) -> Result<bool> {
        let focused = self.get_attr::<CFBoolean>(kAXFocusedAttribute)?;
        Ok(focused.into())
    }

    pub fn focused_element(&self) -> Result<Self> {
        self.get_attr::<AXUIElement>(kAXFocusedUIElementAttribute)
            .map(Self::from)
    }

    pub fn role(&self) -> Result<CFString> {
        self.get_attr::<CFString>(kAXRoleAttribute)
    }

    pub fn subrole(&self) -> Result<CFString> {
        self.get_attr::<CFString>(kAXSubroleAttribute)
    }

    pub fn parent(&self) -> Result<Self> {
        let parent = self.get_attr::<AXUIElement>(kAXParentAttribute)?;
        Ok(parent.into())
    }

    pub fn is_fullscreen(&self) -> Result<bool> {
        self.get_attr::<CFBoolean>(kAXFullScreenAttribute).map(|res| res.into())
    }

    pub fn frame(&self) -> Result<CGRect> {
        self.get_attr::<AXValue>(kAXFrameAttribute)?.as_rect().ok_or(-1)
    }

    pub fn focused_window(&self) -> Result<Self> {
        let window = self.get_attr::<AXUIElement>(kAXFocusedWindowAttribute)?;
        Ok(window.into())
    }

    pub fn dom_class_list(&self) -> Result<Vec<String>> {
        let class_list = self.get_attr::<CFArray>(kAXDOMClassListAttribute)?;
        let filtered: Vec<_> = class_list
            .iter()
            .filter_map(|attr| unsafe {
                let x = NSStringRef::new(*attr as *mut objc::runtime::Object);
                x.as_str().map(|x| x.to_owned())
            })
            .collect();

        Ok(filtered)
    }

    fn attribute_list(&self) -> Result<Vec<String>> {
        let attrs: CFArray<CFString> = unsafe {
            CFArray::wrap_under_create_rule(ax_call(|names: *mut CFArrayRef| {
                AXUIElementCopyAttributeNames(self.get_ref(), names)
            })?)
        };
        Ok(attrs.iter().map(|attr| attr.to_string()).collect())
    }

    #[allow(dead_code)]
    pub fn print_all_attribute_values(&self) {
        if let Ok(attrs) = self.attribute_list() {
            for key in attrs {
                if let Ok(value) = self.get_attr_ref(key.as_str()) {
                    let value_str = if let Some(s) = value.downcast::<CFString>() {
                        format!("{s:?}")
                    } else if let Some(cf_b) = value.downcast::<CFBoolean>() {
                        let b: bool = cf_b.into();
                        format!("{b:?}")
                    } else if let Some(ax) = value.downcast::<AXValue>() {
                        format!("{ax:?}")
                    } else {
                        format!("Unknown {{ type_id: {:?} }}", value.type_of())
                    };

                    warn!("{key}: {value_str}");
                }
            }
        }
    }

    fn children(&self) -> Result<Vec<UIElement>> {
        let children: Vec<_> = self
            .get_attr::<CFArray<*const c_void>>(kAXChildrenAttribute)?
            .iter()
            .map(|child| UIElement::from(unsafe { AXUIElementRef::from_void_ptr(*child) }))
            .collect();

        Ok(children)
    }

    /// One deadline is shared by cached-leaf validation, tree discovery and
    /// frame lookup. Reset the per-element timeout even when the query fails.
    fn query_before<T>(&self, deadline: Instant, read: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(accessibility_sys::kAXErrorCannotComplete);
        }
        let timeout = remaining.as_secs_f32().min(AX_MESSAGING_TIMEOUT_SECONDS);
        let error = unsafe { AXUIElementSetMessagingTimeout(self.get_ref(), timeout) };
        if error != 0 {
            self.set_messaging_timeout();
            return Err(error);
        }
        struct ResetTimeout<'a>(&'a UIElement);
        impl Drop for ResetTimeout<'_> {
            fn drop(&mut self) {
                self.0.set_messaging_timeout();
            }
        }
        let _reset = ResetTimeout(self);
        let result = read(self);
        if Instant::now() >= deadline {
            Err(accessibility_sys::kAXErrorCannotComplete)
        } else {
            result
        }
    }

    pub fn is_xterm_helper_textarea_before(&self, deadline: Instant) -> Result<bool> {
        if self.query_before(deadline, Self::role)? != kAXTextFieldRole
            || !self.query_before(deadline, Self::is_focused)?
        {
            return Ok(false);
        }
        Ok(self
            .query_before(deadline, Self::dom_class_list)?
            .iter()
            .any(|class| class == "xterm-helper-textarea"))
    }

    pub fn frame_before(&self, deadline: Instant) -> Result<CGRect> {
        self.query_before(deadline, Self::frame)
    }

    pub fn focused_window_before(&self, deadline: Instant) -> Result<Self> {
        self.query_before(deadline, Self::focused_window)
    }

    pub fn window_id_before(&self, deadline: Instant) -> Result<CGWindowID> {
        self.query_before(deadline, |element| unsafe { element.get_window_id() })
    }

    pub fn find_x_term_caret_tree_before(&self, deadline: Instant) -> Result<Vec<UIElement>> {
        if self.is_xterm_helper_textarea_before(deadline).unwrap_or(false) {
            return Ok(vec![self.clone()]);
        }
        let mut found = None;
        for child in self.query_before(deadline, Self::children)? {
            // Exhaustion is terminal, not a miss that starts another subtree.
            if Instant::now() >= deadline {
                return Err(accessibility_sys::kAXErrorCannotComplete);
            }
            let Ok(role) = child.query_before(deadline, Self::role) else {
                continue;
            };
            let role: std::borrow::Cow<'_, str> = (&role).into();
            if !XTERM_ROLES.contains(&role.as_ref()) {
                continue;
            }
            if let Ok(tree) = child.find_x_term_caret_tree_before(deadline) {
                if found.is_some() {
                    warn!("Found multiple candidate cursors");
                }
                found = Some(tree);
            }
        }
        if Instant::now() >= deadline {
            return Err(accessibility_sys::kAXErrorCannotComplete);
        }
        let mut tree = found.ok_or(-1)?;
        tree.push(self.clone());
        Ok(tree)
    }

    pub fn window_info(&self, all_windows: bool) -> Option<CGWindowInfo> {
        unsafe {
            let window_id = self.get_window_id().ok()?;
            let windows = if all_windows {
                CFArray::<CFDictionary>::wrap_under_create_rule(display::CGWindowListCopyWindowInfo(
                    kCGWindowListOptionAll,
                    kCGNullWindowID,
                ))
            } else {
                CFArray::<CFDictionary>::wrap_under_create_rule(display::CGWindowListCopyWindowInfo(
                    kCGWindowListOptionOnScreenOnly
                        | kCGWindowListExcludeDesktopElements
                        | kCGWindowListOptionIncludingWindow,
                    window_id,
                ))
            };

            let window = windows.iter().find(|window| {
                get_num(window, kCGWindowNumber)
                    .map(|id| id == (window_id as i64))
                    .unwrap_or(false)
            })?;

            let owner_pid = get_num(&window, kCGWindowOwnerPID)?;
            let bounds = get_value::<CFDictionary>(&window, kCGWindowBounds)?;
            let bounds_rect = CGRect::from_dict_representation(&bounds)?;
            let level = get_num(&window, kCGWindowLayer)?;

            Some(CGWindowInfo {
                owner_pid: owner_pid as u64,
                window_id,
                bounds: bounds_rect,
                level,
            })
        }
    }
}

/// The process that owns window server window `window_id`, while it exists.
/// Owner and number need no Screen Recording grant, unlike window titles.
pub fn window_owner_pid(window_id: CGWindowID) -> Option<pid_t> {
    let windows = unsafe { display::CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window_id) };
    if windows.is_null() {
        return None;
    }
    let windows = unsafe { CFArray::<CFDictionary>::wrap_under_create_rule(windows) };
    let window = windows
        .iter()
        .find(|window| get_num(window, kCGWindowNumber) == Some(i64::from(window_id)))?;
    get_num(&window, kCGWindowOwnerPID).and_then(|pid| pid_t::try_from(pid).ok())
}

fn get_value<T: ConcreteCFType>(dict: &CFDictionary, key: CFStringRef) -> Option<T> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    cf_type.downcast::<T>()
}

fn get_num(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    let num = get_value::<CFNumber>(dict, key)?;
    match num.to_i32() {
        Some(num) => Some(num as i64),
        None => num.to_i64(),
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGWindowLevelForKey(key: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_shared_deadline_never_starts_another_query() {
        let element = UIElement::application(std::process::id() as pid_t);
        let expired = Instant::now();
        let result = element.query_before(expired, |_| -> Result<()> {
            panic!("expired budget must not query AX");
        });
        assert_eq!(result, Err(accessibility_sys::kAXErrorCannotComplete));
        assert!(element.is_xterm_helper_textarea_before(expired).is_err());
        assert!(element.find_x_term_caret_tree_before(expired).is_err());
        assert!(element.frame_before(expired).is_err());
        assert!(element.focused_window_before(expired).is_err());
    }

    #[test]
    fn a_window_that_does_not_exist_has_no_owner() {
        assert_eq!(window_owner_pid(CGWindowID::MAX), None);
    }

    #[test]
    fn query_exhausting_shared_deadline_rejects_result_and_stops_next_query() {
        let element = UIElement::application(std::process::id() as pid_t);
        let deadline = Instant::now() + std::time::Duration::from_millis(20);
        let mut queried = false;
        let result = element.query_before(deadline, |_| {
            queried = true;
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Ok(42)
        });
        assert!(queried, "the first query must actually start");
        assert_eq!(result, Err(accessibility_sys::kAXErrorCannotComplete));
        assert_eq!(
            element.query_before(deadline, |_| -> Result<()> { panic!("shared budget is exhausted") }),
            Err(accessibility_sys::kAXErrorCannotComplete)
        );
    }

    /// `AXUIElementCreateApplication` needs no Accessibility grant, so the two constructors
    /// can be checked against real retain counts: the owning one takes the +1 as is, the
    /// borrowing one adds and later removes its own.
    #[test]
    fn owned_and_borrowed_constructors_balance_their_retains() {
        let owned = UIElement::application(std::process::id() as pid_t);
        assert_eq!(owned.0.retain_count(), 1, "create-rule wrapper must not retain again");

        let borrowed = UIElement::from(owned.get_ref());
        assert_eq!(owned.0.retain_count(), 2, "borrowing wrapper must retain");
        drop(borrowed);
        assert_eq!(owned.0.retain_count(), 1, "borrowing wrapper must release on drop");

        let cloned = owned.clone();
        assert_eq!(owned.0.retain_count(), 2);
        drop(cloned);
        assert_eq!(owned.0.retain_count(), 1);
    }

    fn body_of(source: &str, signature: &str) -> String {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` not found"));
        let rest = &source[start..];
        let end = rest.find("\n    }\n").expect("function end");
        rest[..end].to_string()
    }

    /// Reading an attribute needs a trusted process and a live AX tree, which a test binary
    /// never has, so the ownership of `AXUIElementCopyAttributeValue`'s result is pinned at
    /// the source. It is a `Copy` call: the value is already +1, and re-retaining it leaked
    /// every child element the xterm caret walk touched — 211k of them in one session.
    #[test]
    fn attribute_reads_take_the_copied_value_as_owned() {
        for (source, signature) in [
            (include_str!("ui_element.rs"), "fn get_attr_ref("),
            (include_str!("ui_element.rs"), "fn attribute_list("),
        ] {
            let body = body_of(source, signature);
            let code: Vec<&str> = body
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect();
            assert!(
                code.iter().any(|line| line.contains("wrap_under_create_rule")),
                "{signature} must take ownership of the copied value"
            );
            assert!(
                !code.iter().any(|line| line.contains("wrap_under_get_rule")),
                "{signature} must not retain a value that is already +1"
            );
        }
    }

    /// Same rule for the caret query: both the range it builds (`AXValueCreate`) and the
    /// bounds it copies back are +1 and must be wrapped so the frame releases them.
    #[test]
    fn caret_query_releases_the_values_it_creates_and_copies() {
        let source = include_str!("../caret_position.rs");
        let code: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        assert!(
            !code.iter().any(|line| line.contains("as_void_ptr()")),
            "a created AXValue must not be passed on as a raw pointer with no owner"
        );
        assert!(
            code.iter()
                .filter(|line| line.contains("wrap_under_create_rule"))
                .count()
                >= 2,
            "both the created range and the copied bounds must be owned"
        );
    }
}
