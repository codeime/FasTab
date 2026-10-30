//! The global allocator of the two resident processes: fterm, and the desktop
//! app, which compiles this file by `#[path]`.
//!
//! One fterm stays resident per terminal tab, so its allocator overhead is
//! paid once per tab. The desktop's Rust allocations are mostly transient
//! (completion engine, GPUI frames); under the system allocator they shared
//! pages with AppKit and CoreText caches that live forever, and about half the
//! malloc zone stayed dirty as fragmentation after each session. mimalloc
//! keeps each thread's allocations on its own pages and returns pages that
//! empty out. mimalloc is the default; each crate's `system-allocator` feature
//! swaps in the system allocator so both builds can be measured side by side
//! (`scripts/fterm-footprint-ab.py` for fterm).

#[cfg(not(feature = "system-allocator"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// mimalloc reserves 1 GiB per arena and keeps the arena's bitmaps in its
/// first pages. A handful of set bits per 16 KiB page dirtied ~170 KB of every
/// fterm, whose whole heap is ~3 MB. 32 MiB arenas keep that to a page or two;
/// mimalloc reserves another arena if one ever fills, which the desktop's
/// peaks may.
#[cfg(all(target_os = "macos", not(feature = "system-allocator")))]
mod arena_reserve {
    use std::ffi::{c_int, c_long};

    /// `mi_option_arena_reserve` in mimalloc v3's `mi_option_e`. Pinned by
    /// `option_indices_match_the_bundled_mimalloc`.
    pub(super) const MI_OPTION_ARENA_RESERVE: c_int = 23;
    /// In KiB, the unit mimalloc keeps this option in.
    pub(super) const ARENA_RESERVE_KIB: c_long = 32 * 1024;

    unsafe extern "C" {
        fn mi_option_set(option: c_int, value: c_long);
        #[cfg(test)]
        pub(super) fn mi_option_get(option: c_int) -> c_long;
    }

    extern "C" fn shrink() {
        // SAFETY: options may be set at any time, including before mimalloc
        // has initialized; this one only sizes arenas reserved afterwards.
        unsafe { mi_option_set(MI_OPTION_ARENA_RESERVE, ARENA_RESERVE_KIB) };
    }

    /// Runs as an image initializer: the first Rust allocation reserves the
    /// first arena, and that can happen before `main`.
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static SHRINK: extern "C" fn() = shrink;
}

#[cfg(all(test, target_os = "macos", not(feature = "system-allocator")))]
mod tests {
    use super::arena_reserve::{ARENA_RESERVE_KIB, MI_OPTION_ARENA_RESERVE, mi_option_get};

    #[test]
    fn option_indices_match_the_bundled_mimalloc() {
        // Neighbours with distinctive defaults fix the enum layout around the
        // option we set, so a mimalloc upgrade that renumbers it fails here.
        // SAFETY: reading options has no preconditions.
        let get = |option| unsafe { mi_option_get(option) };
        assert_eq!(get(18), 100, "mi_option_os_tag");
        assert_eq!(get(19), 32, "mi_option_max_errors");
        assert_eq!(get(20), 32, "mi_option_max_warnings");
        assert_eq!(get(27), 400, "mi_option_retry_on_oom");
    }

    #[test]
    fn the_arena_reserve_is_set_before_main() {
        // SAFETY: reading options has no preconditions.
        assert_eq!(unsafe { mi_option_get(MI_OPTION_ARENA_RESERVE) }, ARENA_RESERVE_KIB);
    }
}
