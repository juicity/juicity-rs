# Juicity GUI

A [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) based desktop frontend for the Juicity proxy client.

## Implemented

- Main window with Shadowsocks-Windows-like split layout (Servers + Details)
- JSON config persistence in the platform standard config directory
- Self-contained core manager: both protocols run **inside** the GUI process on
  a dedicated Tokio runtime, so no external helper binaries are required
  - Juicity profile -> embedded `juicity-client` QUIC client + local
    SOCKS5/HTTP server (linked as a library)
  - Shadowsocks profile -> embedded `shadowsocks-service` (official
    shadowsocks-rust library) mixed SOCKS5/HTTP local inbound
- Single mixed inbound port per protocol (SOCKS5 and HTTP proxy on the same
  address); the system proxy and PAC both point at it
- Profile/protocol selectors and a server editor with per-field validation
- URL import/export entry for `juicity://` and `ss://` with parser validation
- System tray support:
  - Linux: StatusNotifierItem via `ksni` (background thread)
  - Windows/macOS: native tray via `tray-icon` (polled on the main loop)
- System proxy (Disable / PAC / Global): Linux GNOME/KDE, macOS `networksetup`, Windows registry. The proxy is restored to "Disable" when the app quits.
- Corrupt config files are moved aside as `*.json.bad` and defaults are used instead of failing to start
- Closing the main window keeps the app running in the tray (on by default; a hidden background window keeps GPUI's event loop alive), as long as a tray icon is actually available
- Start/stop and core status polling (300 ms)
- PAC settings dialog and Startup settings dialog
- About dialog (tray menu only) showing the embedded application icon and the
  versions of the GUI, Shadowsocks-Rust and Juicity-RS

## Embedded protocol backends

The GUI does not shell out to `juicity-client` or `sslocal`. Instead it links
them directly:

| Protocol | Implementation | Local listener |
| --- | --- | --- |
| Juicity | `juicity-client` crate (this workspace) | mixed SOCKS5 + HTTP on `mixed_listen` |
| Shadowsocks | `shadowsocks-service` (shadowsocks-rust) | mixed SOCKS5 + HTTP on `mixed_listen` |

Both protocols expose a single **mixed inbound**: one port that accepts SOCKS5
and HTTP proxy traffic, dispatched per connection by inspecting the first byte.
The system proxy settings and the PAC file both point at that single address, so
there is no separate HTTP port to configure.

HTTP proxying covers both `CONNECT` tunnels and plain absolute-form requests
(`GET http://host/path`). The latter are rewritten to origin-form and the
upstream connection is asked to close after the response, so plain HTTP traffic
does not reuse a keep-alive connection through the proxy.

Supported Shadowsocks ciphers follow shadowsocks-rust: AEAD-2022, AEAD and the
deprecated stream ciphers. SIP003 plugins (`plugin`, `plugin_opts`,
`plugin_args`) are passed through to the library unchanged.

Legacy profiles that still point at a full `juicity-client` / `sslocal` JSON
config file via `config_path` are loaded from that file; otherwise the
configuration is generated from the individual profile fields.

## Application icon

`gui/icon.svg` is the single source of truth and is embedded into the binary, so
no icon file has to ship next to the executable. `gui/build.rs` rasterizes it
with `resvg` at 16/32/48/64/128/256/512/1024 px and the results are pulled in
with `include_bytes!`:

- the SVG itself and the 256 px PNG are served through the GPUI asset source
  (`icon::Assets`), which is what the About dialog draws
- 16/32/48 px are additionally converted to raw ARGB for the Linux
  StatusNotifierItem tray icon
- `icon.ico` is assembled from the 16–256 px bitmaps and, on Windows, compiled
  into the executable's resources as `IDI_ICON1` (resource id 1) — GPUI reads
  the window and taskbar icon from there
- `gui/macos/bundle.sh` builds `icon.icns` from the generated PNGs, so no SVG
  converter has to be installed to produce a bundled app

## Config directory

The app uses `directories::ProjectDirs` with:

- Qualifier: `io`
- Organization: `juicity`
- Application: `juicity-gui`

Typical resolved paths:

- Linux: `~/.config/juicity/juicity-gui/`
- macOS: `~/Library/Application Support/io.juicity.juicity-gui/`
- Windows: `%APPDATA%\\io\\juicity\\juicity-gui\\config\\`

JSON files currently used:

- `app.json`
- `profiles.json`
- `runtime.json`

## Build dependencies

The UI is built with [Slint](https://slint.dev). It renders with FemtoVG on
OpenGL by default; Vulkan is not needed. On a machine without working GPU
acceleration (for example a VM without a GPU driver), start the GUI with
`--software-render` to use Slint's software renderer instead:

```bash
juicity-gui --software-render
```

Setting `SLINT_BACKEND=winit-software` has the same effect.

### Linux

Wayland, X11, xkbcommon and OpenGL are loaded at runtime, so only `pkg-config`
and the Fontconfig development files are needed to build (the same packages CI
installs). At runtime the GUI needs a Wayland or X11 session with an OpenGL
driver (Mesa), and Noto Sans CJK for the Chinese UI.

```bash
# Debian/Ubuntu
sudo apt install -y pkg-config libfontconfig1-dev fonts-noto-cjk fonts-noto-core
# Fedora
sudo dnf install -y pkgconf-pkg-config fontconfig-devel google-noto-sans-cjk-fonts
# Arch
sudo pacman -S --needed pkgconf fontconfig noto-fonts-cjk
```

On NixOS, add `wayland`, `libxkbcommon`, `libGL` and the X11 libraries to
`LD_LIBRARY_PATH` (for example in a dev shell), because they are opened with
`dlopen` rather than linked.

### macOS

No extra packages are needed. FemtoVG uses Apple's deprecated OpenGL; if the
window does not render, use `--software-render`.

### Windows

No extra packages or runtimes are needed.

`x86_64` and `aarch64` are supported. The MSVC targets link the CRT statically
and therefore need no extra runtime DLLs; the `*-pc-windows-gnullvm` targets are
built with the MSYS2 CLANG64 (x86_64) or CLANGARM64 (aarch64) toolchain, which is
also what CI uses on the native Windows 11 ARM runner.

## Run

From workspace root:

```bash
cargo run -p juicity-gui
```

On Linux a running StatusNotifierHost (KDE/GNOME) is needed for the tray icon.
