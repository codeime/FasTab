# GPUI vendoring

This directory contains the crates.io `gpui` 0.2.2 release, licensed under
Apache-2.0 (see `LICENSE-APACHE`). The original published, normalized
`Cargo.toml` (except the two dependency entries below), source, build inputs,
examples and tests are retained. Cargo's
`.cargo-ok`, `.cargo_vcs_info.json` and nested `Cargo.lock` are omitted.

Provenance:

- Crate: <https://crates.io/crates/gpui/0.2.2>
- Published archive SHA-256:
  `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`
- Upstream: <https://github.com/zed-industries/zed>
- Published VCS metadata: commit
  `69e2130295c2649963eb639fc70b4f2ee8ea1624`, path `crates/gpui`, `dirty: true`.
  The release archive, rather than that Git commit alone, is the baseline.

Fastab changes `src/platform/mac/metal_renderer.rs`:

- Destroying a Metal renderer trims the application-wide idle instance-buffer
  pool. The configured buffer size is preserved, including prior growth.
- Each acquired buffer records a pool generation. GPU completion returns a
  buffer to the pool only if its generation and size still match. A renderer
  closing therefore cannot be undone by a late completion, while outstanding
  buffers remain owned until their GPU work finishes.
- Retired Metal buffers are returned from pool operations and dropped after
  releasing the pool mutex, including the existing buffer-growth reset path.
- Four tests use real Metal allocations and command completion to cover late
  returns, reopening after buffer growth, and another renderer surviving a close.
  A shared-event gate keeps an already committed command in flight
  while its renderer is dropped; releasing the gate verifies the GPU result and
  prevents a late return from repopulating the retired pool.
- Path intermediate textures are allocated only for a scene containing paths.
  Drawable-size changes release both the resolved target and its 4x MSAA target;
  unchanged sizes reuse them, and a window containing only text, quads and sprites
  never allocates them. Nonpositive dimensions cannot allocate a texture. After a
  path disappears, same-size targets stay until resize or window destruction to
  avoid allocation churn for intermittent icons. Submitted Metal commands retain
  their old resources during resize; no GPU wait is added.
- Two additional hardware tests cover real texture allocation/reuse, empty scenes,
  repeated sizes, resize and invalid dimensions, plus a gated production path
  rasterization that survives resize and reads back its MSAA-resolved pixel.
  This reduces unnecessary texture allocation and potential peak usage; it does
  not establish a reduction in the installed app's idle physical footprint.
- CoreVideo texture-cache creation passes a borrowed `DeviceRef` through a
  private raw FFI binding. The locked `core-video` 0.4.3 wrapper instead moves an
  owned `metal::Device` into the non-consuming C argument, abandoning the cloned
  device's release. The returned cache still follows the CoreFoundation Create
  rule. Cache creation is now lazy on the first nonempty surface batch, so normal
  Settings and autocomplete frames do not create an unused video cache. Once
  created, it is reused and dropped with its renderer; submitted texture work
  keeps the existing Metal command ownership.
- `build.rs` excludes this host-only CoreVideo binding from cbindgen's shader
  header. Its export include list is additive, so without this exclusion the
  private extern declaration enters the runtime Metal source and aborts the
  first window's shader compilation. A CPU regression checks the actual generated
  header and embedded runtime source, without requiring a Metal device.

Fastab also changes `src/platform/mac/window.rs`:

- Logical window destruction takes the renderer and callbacks out of window
  state, stops frame delivery, detaches the native view's layer, and drops GPU
  resources without holding the state mutex. AppKit can retain a closed view;
  GPU teardown must not wait for that view's `dealloc`.
- Closed views reject late native callbacks. Callbacks and input handlers taken
  out before a close are not restored afterward, and synthetic drag loops exit
  when their window closes or its state disappears. Reusable hidden windows
  keep their renderer until they are actually destroyed.
- Layer/view cleanup can synchronously reenter AppKit. The closed state is set
  first, superclass frame-size changes happen outside the state mutex, and the
  detached view is retained through deferred native close so a callback that
  closes its own window cannot free its receiver midway through execution.
- The spurious `windowDidBecomeKey:` workaround retains the native window and
  releases the window-state mutex before calling `resignKeyWindow`. AppKit can
  synchronously deliver `windowDidResignKey:` from that call, which reenters the
  same callback and otherwise deadlocks on the mutex. The retained window stays
  alive through the call; normal activation and frame-request paths are unchanged.

Additional lifecycle fixes:

- `display_link.rs` reuses one process-lifetime CoreVideo clock per encountered
  display. This follows the lifetime constraint documented in upstream
  [#32116](https://github.com/zed-industries/zed/pull/32116) and
  [#60696](https://github.com/zed-industries/zed/pull/60696): stopping a link does
  not join its IO thread, so releasing each retired link can race that thread.
  Window subscriptions are removed under the producer's lock before their GCD
  sources are cancelled and released. The source finalizer owns the callback
  context until handlers finish. Start/stop are idempotent; CoreVideo calls
  happen outside the registry lock. Five tests exercise real GCD finalization,
  callback self-close and production registry failure/retry behavior. The unused
  `dispatch_suspend` binding is removed from `build.rs`.
- `metal_atlas.rs` removes the key mapping before decrementing its texture's
  reference count, so repeated removal cannot free another key's texture and a
  later lookup rebuilds removed content. Three real Metal tests cover shared
  textures, repeated slot reuse, and committed GPU reads surviving deletion and
  replacement. Individual tile regions are not reused during a texture's life.
- `window.rs` autoreleases native attributed-substring results and checks nullable
  text-query output pointers. Two Foundation tests exercise UTF-16 output and
  autorelease-pool ownership. Screen-number strings in `window.rs`, `display.rs`
  and `screen_capture.rs`, plus tabbing identifiers, use the existing autoreleased
  string helper.

Application cache reclamation in `src/app.rs` and `src/text_system.rs`:

- `App::release_idle_caches` releases cached asset task references, derived font
  metrics and glyph bounds, and idle text scratch buffers only when the window
  slot map is empty. A window temporarily taken out for an update still blocks
  reclamation. Fastab calls this from a deferred last-window-close handler.
- Retired maps and vectors are replaced with empty containers so their capacity
  is released. Borrowed line wrappers and font-run buffers carry a pool generation;
  returning a pre-reclamation lease drops it instead of repopulating the pool.
  Buffer destruction happens outside the pool mutex.
- Font lookup identities and the platform font table remain intact because
  external wrappers and layouts may outlive their window. Outstanding asset
  consumers may finish independently; completion does not reinsert a cache entry.
- The return value reports counts of retired cache entries and font-run capacity,
  not allocator or physical-memory savings. Four public-API regression tests live
  in `crates/fastab_gpui/src/idle_cache_tests.rs`, within the existing crate test
  target. They cover wrapper reuse, retained font/layout identities, old asset
  completion, and the actual deferred close hook with an intervening window open.
  The GPUI test platform uses its synthetic text backend; these tests do not
  establish CoreText rendering or installed-app memory savings.
- The application child-layout ID buffer also retires after the last window.
  The existing idle fixture builds its capacity through real Div child layouts,
  verifies that another live window prevents retirement, and reopens to rebuild
  it. `layout_id_buffer_bytes` reports backing capacity, not footprint savings.
- `PlatformTextSystem::release_idle_caches` defaults to no work. The macOS
  implementation takes the ZWNJ layout scratch under its existing write lock
  and drops it outside the lock. All native fonts and FontIds remain registered;
  live layouts own their glyph indices. `TextCacheRelease.platform_scratch_bytes`
  reports only the discarded scratch capacity. A native CoreText fixture checks
  mixed Unicode/fallback shaping, old layouts across retirement, stable font
  identities, and lazy scratch rebuilding.

Window tracking, element arena and native menus:

- `app.rs` removes a closed window's entity tracking before its close observers
  run. `arena.rs` retires the element arena's storage once no element is live;
  a handle from before the last clear panics on use instead of reading freed
  storage.
- `platform/mac/platform.rs` keeps menu actions in a registry keyed by
  monotonically increasing tags that are never reused, so a stale tag resolves
  to nothing. The platform lock is released before an action runs, which also
  removes upstream's self-deadlock when `handle_menu_item` met an unknown tag.
  The old dock menu is released outside the lock. An empty `set_menus` detaches
  the main, Windows and Services menus and reports no menus, which Fastab's
  idempotent retire/restore in `fastab_desktop` relies on; the Services owner is
  now the submenu's `NSMenu` rather than upstream's `NSMenuItem`.

CoreFoundation ownership and fonts:

- `keyboard.rs` wraps `TISCopy…` input sources under the Create rule and copies
  their borrowed property strings before release, including on unwind.
- `open_type.rs` releases the initial +1 of the feature and cascade arrays once
  the attribute dictionary retains them. Upstream built the system cascade with
  an unconsumed lazy iterator, so a font with explicit fallbacks never received
  the per-language system cascade; it is now appended after the requested
  fallbacks. Fastab itself sets no `FontFallbacks`.
- `text_system.rs` registers font faces as candidates and adds a face to the
  font table only when it is selected or reached through fallback. Menlo and
  Helvetica in their default configuration defer even the per-face query. Other
  families still create a temporary configured `CTFont` per face while loading,
  then keep only its copied descriptor until selection. Faces added with
  `add_fonts` keep their configured font instead, because a descriptor without a
  file URL can resolve to a same-named system font. FontIds and PostScript
  fallback precedence are stable across retirement.

The root `[patch.crates-io]` applies this copy to all GPUI consumers. Other
`gpui_*` crates retain their registry dependencies. Font identities, rendering
primitives, MSAA quality and instance-buffer sizing are unchanged.

This does not unregister private AppKit notification observers or promise that
all native views and application-wide caches disappear on close. It separates
their lifetime from the resources needed only by a live window.

The new CoreVideo ownership and lazy-cache fixtures are exported through
`test-support` to the existing `fastab_gpui` workspace test target, alongside
the CPU-only ownership fixtures. CI requires 21 CPU-only cases to pass (and
names the menu, font-attribute, font-candidate and shader cases individually)
and both Metal cases to be collected as ignored. On a Metal-capable macOS
validation host (not the local development machine, where builds remain
prohibited), run:

```sh
cargo test --locked -p fastab_gpui resource_ownership_tests::metal_surface_cache_ -- --ignored --test-threads=1
```

The explicit `Native CoreVideo cache regression` workflow uses this workspace
bridge on a registered `fastab-metal` runner and requires both named cases to
pass. It does not run on push or claim coverage of the older vendor-only GPU
fixtures below.

These fixtures use a real device and CoreVideo cache, checking the borrowed
argument at the FFI boundary, deterministic error handling, empty-batch laziness,
cache reuse and its owned-reference release. Repeated create/drop cycles must
also leave the device's retain count settled, which is what catches a consumed
device argument. They require hardware explicitly;
missing Metal never silently turns a test into a pass. Their execution and a
new installed build's footprint comparison remain separate acceptance steps.

Fastab trims two dependencies in `Cargo.toml` to cut code it never runs
(measured with `scripts/binary-size-report.sh`, `.text` of the `fastab` binary):

- `image` drops its default features and keeps `png`, `gif` and `webp`. GPUI
  names `GifDecoder` and `WebPDecoder` directly; the app only decodes PNG. The
  exr, jpeg, tiff, bmp and other decoders were ~0.7 MB. Those formats still
  parse as `ImageFormat` variants and now fail to decode at runtime.
- `resvg` no longer enables `text`, `system-fonts` or `memmap-fonts`, which
  removes rustybuzz, fontdb and ttf-parser. `src/svg_renderer.rs` therefore
  builds `usvg::Options::default()` instead of installing a lazily loaded
  system font database. The app's SVG icons contain no `<text>`; a `<text>`
  element is now not drawn.

Run the hardware tests on a Metal-capable macOS host from a temporary verification
copy of this repository for the older vendor-only fixtures described above.
In that copy only, add `"vendor/gpui"` to the root
`workspace.members`, then run:

```sh
cargo test -p gpui --features runtime_shaders,test-support --lib instance_buffer_pool_tests -- --ignored --test-threads=1
```

Keep that workspace-members edit out of the delivered repository. Cargo cannot
test this dependency outside the workspace because GPUI has its own
dev-dependencies; selecting `fastab_gpui` alongside it does not remove this
restriction. `runtime_shaders` matches Fastab's renderer build, and
`test-support` enables the supporting crates' test features needed by GPUI's
library test harness. No change to GPUI's published manifest is required.

These tests intentionally require an explicit run because headless macOS CI
hosts may lack a Metal device. Also validate closing/reopening Fastab Settings
while using autocomplete, and measure the installed app's idle physical
footprint; the unit tests do not establish UI behavior or memory savings.
