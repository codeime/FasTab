#![allow(unused, non_upper_case_globals)]

use crate::{FontFallbacks, FontFeatures};
use cocoa::appkit::CGFloat;
use core_foundation::{
    array::{
        CFArray, CFArrayAppendValue, CFArrayCreateMutable, CFArrayRef, CFMutableArrayRef,
        kCFTypeArrayCallBacks,
    },
    base::{CFType, TCFType, kCFAllocatorDefault},
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_foundation_sys::locale::CFLocaleCopyPreferredLanguages;
use core_graphics::geometry::CGAffineTransform;
use core_text::{
    font::{CTFont, CTFontRef},
    font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCreateWithAttributes,
        CTFontDescriptorCreateWithNameAndSize, CTFontDescriptorRef, kCTFontCascadeListAttribute,
        kCTFontFeatureSettingsAttribute,
    },
};
use font_kit::font::Font as FontKitFont;

pub fn apply_features_and_fallbacks(
    font: &mut FontKitFont,
    features: &FontFeatures,
    fallbacks: Option<&FontFallbacks>,
) -> anyhow::Result<()> {
    unsafe {
        let attrs = font_attributes(
            features,
            fallbacks,
            font.native_font().as_concrete_TypeRef(),
        );
        let new_descriptor = CTFontDescriptorCreateWithAttributes(attrs.as_concrete_TypeRef());
        let new_descriptor = CTFontDescriptor::wrap_under_create_rule(new_descriptor);
        let new_font = CTFontCreateCopyWithAttributes(
            font.native_font().as_concrete_TypeRef(),
            0.0,
            std::ptr::null(),
            new_descriptor.as_concrete_TypeRef(),
        );
        let new_font = CTFont::wrap_under_create_rule(new_font);
        *font = font_kit::font::Font::from_native_font(&new_font);

        Ok(())
    }
}

fn font_attributes(
    features: &FontFeatures,
    fallbacks: Option<&FontFallbacks>,
    font_ref: CTFontRef,
) -> CFDictionary<CFString, CFType> {
    unsafe {
        let mut attributes = vec![(
            CFString::wrap_under_get_rule(kCTFontFeatureSettingsAttribute),
            generate_feature_array(features).as_CFType(),
        )];
        if let Some(fallbacks) = fallbacks
            && !fallbacks.fallback_list().is_empty()
        {
            attributes.push((
                CFString::wrap_under_get_rule(kCTFontCascadeListAttribute),
                generate_fallback_array(fallbacks, font_ref).as_CFType(),
            ));
        }
        // The dictionary retains its values; the builders' initial +1 owners
        // must also retire, including the empty feature array.
        CFDictionary::from_CFType_pairs(&attributes)
    }
}

fn generate_feature_array(features: &FontFeatures) -> CFArray<CFDictionary<CFString, CFType>> {
    unsafe {
        let feature_array = CFArray::wrap_under_create_rule(CFArrayCreateMutable(
            kCFAllocatorDefault,
            0,
            &kCFTypeArrayCallBacks,
        ));
        for (tag, value) in features.tag_value_list() {
            let dict = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::wrap_under_get_rule(kCTFontOpenTypeFeatureTag),
                    CFString::new(tag).as_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kCTFontOpenTypeFeatureValue),
                    CFNumber::from(*value as i32).as_CFType(),
                ),
            ]);
            CFArrayAppendValue(
                feature_array.as_concrete_TypeRef() as CFMutableArrayRef,
                dict.as_CFTypeRef(),
            );
        }
        feature_array
    }
}

fn generate_fallback_array(
    fallbacks: &FontFallbacks,
    font_ref: CTFontRef,
) -> CFArray<CTFontDescriptor> {
    unsafe {
        let fallback_array = CFArray::wrap_under_create_rule(CFArrayCreateMutable(
            kCFAllocatorDefault,
            0,
            &kCFTypeArrayCallBacks,
        ));
        for user_fallback in fallbacks.fallback_list() {
            let name = CFString::from(user_fallback.as_str());
            let fallback_desc = CTFontDescriptor::wrap_under_create_rule(
                CTFontDescriptorCreateWithNameAndSize(name.as_concrete_TypeRef(), 0.0),
            );
            CFArrayAppendValue(
                fallback_array.as_concrete_TypeRef() as CFMutableArrayRef,
                fallback_desc.as_CFTypeRef(),
            );
        }
        append_system_fallbacks(
            fallback_array.as_concrete_TypeRef() as CFMutableArrayRef,
            font_ref,
        );
        fallback_array
    }
}

fn append_system_fallbacks(fallback_array: CFMutableArrayRef, font_ref: CTFontRef) {
    unsafe {
        let preferred_languages: CFArray<CFString> =
            CFArray::wrap_under_create_rule(CFLocaleCopyPreferredLanguages());

        let default_fallbacks = CTFontCopyDefaultCascadeListForLanguages(
            font_ref,
            preferred_languages.as_concrete_TypeRef(),
        );
        let default_fallbacks: CFArray<CTFontDescriptor> =
            CFArray::wrap_under_create_rule(default_fallbacks);

        default_fallbacks
            .iter()
            .filter(|desc| desc.font_path().is_some())
            .map(|desc| {
                CFArrayAppendValue(fallback_array, desc.as_concrete_TypeRef() as _);
            });
    }
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontOpenTypeFeatureTag: CFStringRef;
    static kCTFontOpenTypeFeatureValue: CFStringRef;

    fn CTFontCreateCopyWithAttributes(
        font: CTFontRef,
        size: CGFloat,
        matrix: *const CGAffineTransform,
        attributes: CTFontDescriptorRef,
    ) -> CTFontRef;
    fn CTFontCopyDefaultCascadeListForLanguages(
        font: CTFontRef,
        languagePrefList: CFArrayRef,
    ) -> CFArrayRef;
}

#[cfg(feature = "test-support")]
#[allow(missing_docs)]
pub(crate) mod fastab_resource_fixtures {
    use super::*;
    use std::sync::Arc;

    fn array_attribute<T>(
        attributes: &CFDictionary<CFString, CFType>,
        key: CFStringRef,
    ) -> CFArray<T> {
        let value = attributes.get(key);
        assert!(value.instance_of::<CFArray>());
        unsafe { CFArray::wrap_under_get_rule(value.as_CFTypeRef() as CFArrayRef) }
    }

    fn feature_values(features: &CFArray<CFDictionary<CFString, CFType>>) -> Vec<(String, i32)> {
        features
            .iter()
            .map(|feature| unsafe {
                let tag = feature
                    .get(kCTFontOpenTypeFeatureTag)
                    .downcast::<CFString>()
                    .unwrap()
                    .to_string();
                let value = feature
                    .get(kCTFontOpenTypeFeatureValue)
                    .downcast::<CFNumber>()
                    .unwrap()
                    .to_i32()
                    .unwrap();
                (tag, value)
            })
            .collect()
    }

    pub fn empty_feature_array_has_no_abandoned_create_retain() {
        for _ in 0..32 {
            let attributes = font_attributes(&FontFeatures::default(), None, std::ptr::null());
            let features: CFArray<CFDictionary<CFString, CFType>> =
                array_attribute(&attributes, unsafe { kCTFontFeatureSettingsAttribute });
            assert!(features.is_empty());
            drop(attributes);
            // This uniquely-created mutable array has not been sent to
            // CoreText. The test's borrowed-then-retained handle is its only
            // remaining owner; an abandoned builder +1 makes this two.
            assert_eq!(features.retain_count(), 1);
        }
    }

    pub fn feature_and_fallback_arrays_outlive_inputs_without_extra_retains() {
        let font = core_text::font::new_from_name("Helvetica", 12.0).unwrap();
        let attributes = font_attributes(
            &FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 1)])),
            Some(&FontFallbacks::from_fonts(vec![
                "Menlo".into(),
                "Helvetica".into(),
            ])),
            font.as_concrete_TypeRef(),
        );
        let features = array_attribute(&attributes, unsafe { kCTFontFeatureSettingsAttribute });
        let fallbacks: CFArray<CTFontDescriptor> =
            array_attribute(&attributes, unsafe { kCTFontCascadeListAttribute });
        drop(attributes);

        assert_eq!(features.retain_count(), 1);
        assert_eq!(fallbacks.retain_count(), 1);
        assert_eq!(
            feature_values(&features),
            [("calt".into(), 0), ("liga".into(), 1)]
        );
        assert_eq!(
            fallbacks
                .iter()
                .map(|font| font.font_name())
                .collect::<Vec<_>>(),
            ["Menlo", "Helvetica"],
        );
    }

    pub fn native_font_keeps_requested_features_after_attribute_owners_drop() {
        let native = core_text::font::new_from_name("Helvetica", 12.0).unwrap();
        // The native handle is a live CTFont created immediately above.
        let mut font = unsafe { FontKitFont::from_native_font(&native) };
        apply_features_and_fallbacks(
            &mut font,
            &FontFeatures(Arc::new(vec![("liga".into(), 0)])),
            None,
        )
        .unwrap();

        let attributes = font.native_font().copy_descriptor().attributes();
        let features = array_attribute(&attributes, unsafe { kCTFontFeatureSettingsAttribute });
        assert_eq!(feature_values(&features), [("liga".into(), 0)]);
    }
}
