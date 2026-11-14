# Building Crust

Run commands from the workspace root unless a section says otherwise.

## Requirements

- Rust stable toolchain (edition 2021)
- Cargo
- Linux desktop dependencies for `eframe`/`winit` (X11 or Wayland)
- Linux dev dependencies for the embedded Twitch webview sidecar (`crust-webview`): WebKitGTK 4.1, libsoup 3, GTK 3
  - Ubuntu / Debian: `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev libjavascriptcoregtk-4.1-dev pkg-config build-essential`
  - Fedora: `sudo dnf install webkit2gtk4.1-devel libsoup3-devel gtk3-devel pkgconf-pkg-config`
  - Arch: `sudo pacman -S webkit2gtk-4.1 libsoup3 gtk3 pkgconf`
- Windows 10/11 or Debian-based Linux for built-in auto-install updates
- Windows: WebView2 Runtime (preinstalled on Windows 11; Windows 10 may need the Evergreen runtime from Microsoft)

## Build and run

```bash
cargo check
cargo run -p crust
```

Release build:

```bash
cargo build -p crust --release
```

### Windows release binary

Build and package a Windows release zip from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build_windows_release.ps1
```

The script writes:

- `target\\release\\crust.exe`
- `dist\\windows\\crust-v<version>-windows-x64.zip`

### Debian release package

Build and package a Debian release `.deb` from Linux:

```bash
bash ./scripts/build_debian_release.sh
```

Artifacts are produced at:

- `target/release/crust`
- `dist/debian/crust-v<version>-debian-<arch>.deb`
- `dist/debian/crust-v<version>-debian-<arch>.deb.sha256`

### Running on WSL

Requires VcXsrv launched with the `-wgl` flag (or "Native opengl" checked in XLaunch) to expose GLX framebuffer configs. Mesa version overrides are needed to negotiate a valid OpenGL context:

```bash
export DISPLAY=172.17.128.1:0.0  # replace with your host IP - check /etc/resolv.conf nameserver
export MESA_GL_VERSION_OVERRIDE=3.3
export MESA_GLSL_VERSION_OVERRIDE=330
export WINIT_UNIX_BACKEND=x11
unset WAYLAND_DISPLAY
cargo run -p crust --release
```

With WSLg on Windows 11, run Crust through Wayland:

```bash
cargo run -p crust --release
```


## Workspace layout

- `crates/app` - binary entrypoint, runtime wiring, reducer/event loop
- `crates/ui` - `egui` application and widgets
- `crates/core` - shared domain models, events, tokenizer / highlight / filter DSL / hotkeys / sound / state
- `crates/twitch` - IRC parser + Twitch session client / reconnect / rate limiting / EventSub / Helix / GraphQL providers
- `crates/kick` - Kick chat session (beta)
- `crates/emotes` - provider loaders and image cache (memory + disk)
- `crates/storage` - settings / token / log storage and persistence
- `crates/uploader` - image-upload backend (Imgur, Nuuls, ShareX SXCU, custom)
- `crates/webview` / `crates/webview-host` - embedded Twitch webview for auth + auto-claim flows
