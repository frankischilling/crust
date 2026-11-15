# Vendored dependencies

`eframe/` contains the published eframe 0.31.1 crate from crates.io. Its recorded
upstream revision is
[`1669e52a7ccfc3489c1b0999b9ed48894a0b3887`](https://github.com/emilk/egui/tree/1669e52a7ccfc3489c1b0999b9ed48894a0b3887/crates/eframe).
The original crate archive has SHA256
`d0dfe0859f3fb1bc6424c57d41e10e9093fe938f426b691e42272c2f336d915c`.

The upstream manifests, README, assets, and source are retained. The MIT and
Apache 2.0 license texts come from the same upstream revision. Cargo's local
registry markers, `.cargo-ok` and `.cargo_vcs_info.json`, are omitted.

Crust patches `src/native/glow_integration.rs` and
`src/native/wgpu_integration.rs` to initialize AccessKit for every native window.
Version 0.31.1 initializes only the main window, leaving detached chats without
native accessibility controls. The patch creates an adapter for each new window,
restores the requested visibility after initialization, and replaces the adapter
when a window is recreated. Existing event routing delivers accessibility actions
to the corresponding viewport.

The Glow patch also preserves requested transparency when creating a Windows
child window with an alpha-capable GL configuration. WGL's legacy transparency
flag can be false for a working DWM surface. In that case, `glutin-winit` clears
the window's transparency request before creation, preventing winit from enabling
DWM composition. Creating that window directly lets winit enable composition.
Other platforms and configurations without alpha retain normal finalization.
See the [WGL pixel format specification](https://registry.khronos.org/OpenGL/extensions/ARB/WGL_ARB_pixel_format.txt)
and [DWM alpha composition documentation](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmenableblurbehindwindow).

The workspace selects this copy through `[patch.crates-io]`. Remove the override
when an upstream release provides the same behavior and child-window accessibility
and Windows transparency have been checked.
