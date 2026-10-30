//! fterm's global allocator.
//!
//! One fterm stays resident per terminal tab, so its allocator overhead is
//! paid once per tab. mimalloc is the default; the `system-allocator` feature
//! swaps in the system allocator so CI can measure both builds side by side
//! (`scripts/fterm-footprint-ab.py`).

#[cfg(not(feature = "system-allocator"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// mimalloc reserves 1 GiB per arena and keeps the arena's bitmaps in its
/// first pages. A handful of set bits per 16 KiB page dirtied ~170 KB of every
/// fterm, whose whole heap is ~3 MB. 32 MiB arenas keep that to a page or two;
/// mimalloc reserves another arena if one ever fills.
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
