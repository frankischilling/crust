# Building Crust

Run commands from the workspace root unless a section says otherwise.

## Requirements

- Rust stable toolchain (edition 2021)
- Cargo
- Linux desktop dependencies for `eframe`/`winit` (X11 or Wayland)
- Linux dev dependencies for the embedded Twitch webview sidecar (`crust-webview`): WebKitGTK 4.1, libsoup 3, GTK 3
  - Ubuntu / Debian: `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev libjavascriptcoregtk-4.1-dev libasound2-dev libxkbcommon-dev libwayland-dev libssl-dev pkg-config build-essential dpkg-dev python3`
  - Fedora: `sudo dnf install webkit2gtk4.1-devel libsoup3-devel gtk3-devel pkgconf-pkg-config`
  - Arch: `sudo pacman -S webkit2gtk-4.1 libsoup3 gtk3 pkgconf`
- Windows 10/11 or Debian-based Linux for built-in auto-install updates
- Windows: WebView2 Runtime (preinstalled on Windows 11; Windows 10 may need the Evergreen runtime from Microsoft)

## Build and run

```bash
cargo check --locked
cargo run -p crust --locked
```

Release build:

```bash
cargo build -p crust -p crust-webview-host --release --locked
```

### Windows release binary

Build and package a Windows release zip from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build_windows_release.ps1
```

The script writes:

- `target\release\crust.exe`
- `target\release\crust-webview.exe`
- `dist\windows\crust-v<version>-windows-x64.zip`
- `dist\windows\crust-v<version>-windows-x64.zip.sha256`

The script stops if compilation or version lookup fails, so a failed build cannot
package an older executable left in `target`.

Both release scripts use the executable paths reported by Cargo, including when
`CARGO_TARGET_DIR` changes the output directory. The Windows package requires x64
PE executables; the Debian package checks that both ELF files match the package
architecture.

### Debian release package

Build and package a Debian release `.deb` from Linux:

```bash
bash ./scripts/build_debian_release.sh
```

Artifacts are produced at:

- `target/release/crust`
- `target/release/crust-webview`
- `dist/debian/crust-v<version>-debian-<arch>.deb`
- `dist/debian/crust-v<version>-debian-<arch>.deb.sha256`

The script needs Python 3 to read Cargo's build results and `dpkg-dev` to derive
dependencies from both executables. The
package records the required library versions from the build host, including
WebKitGTK 4.1, and the X11 keyboard library loaded at runtime. Build on the oldest
distribution you intend to support.

The Linux workflow uses Ubuntu 24.04. It runs workspace tests and correctness
lints before packaging. Under a virtual X11 display, it checks the webview with
a local page and JavaScript response, then checks desktop startup and webview
initialization. It keeps the package and checksum as a workflow artifact for
seven days.

### Running on WSL

With VcXsrv, enable WGL with the `-wgl` flag or "Native opengl" in XLaunch. The
following Mesa overrides request an OpenGL 3.3 context:

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
