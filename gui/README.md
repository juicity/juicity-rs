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

GPUI requires a Vulkan-capable display server (Wayland or X11) at runtime and
the following native libraries at build time: X11, xcb, xkbcommon, wayland.

### Linux (Debian/Ubuntu)

```bash
sudo apt update
sudo apt install -y pkg-config libx11-dev libxcb1-dev libxkbcommon-dev \
  libwayland-dev libvulkan-dev mesa-vulkan-drivers
```

### Fedora

```bash
sudo dnf install -y pkgconf-pkg-config libX11-devel libxcb-devel \
  libxkbcommon-devel wayland-devel vulkan-loader-devel mesa-vulkan-drivers
```

### Arch

```bash
sudo pacman -S --needed pkgconf libx11 libxcb libxkbcommon wayland vulkan-icd-loader \
  vulkan-mesa-layer
```

### NixOS

System libraries live in the nix store, so point `LIBRARY_PATH` at them for
linking and `LD_LIBRARY_PATH` for running. Example with a Vulkan software
rasterizer (llvmpipe) on Wayland:

```bash
export LIBRARY_PATH=/nix/store/zyvz6mkqf6iihqr5yfvmfr2inafxdlq4-libxcb-1.17.0/lib:/nix/store/xg73b708qsrdvb82vdwvir097p9w7vr3-libxkbcommon-1.13.2/lib:/nix/store/b4r5xlxclsvy3z6fvvwf74vln5l1hw4y-wayland-1.25.0/lib

export LD_LIBRARY_PATH=$LIBRARY_PATH:/nix/store/xin0b9mlvl6w1qqhvr2nfdcv5qns1b13-vulkan-loader-1.4.350.0/lib:/nix/store/3967gykw3wcyq3svf238nk31jlhxnl7c-mesa-26.1.5/lib
export VK_ICD_FILENAMES=/nix/store/3967gykw3wcyq3svf238nk31jlhxnl7c-mesa-26.1.5/share/vulkan/icd.d/lvp_icd.x86_64.json

cargo build -p juicity-gui
```

(The store hashes depend on the installed nixpkgs revision; resolve them with
`nix-store -q` or use a `pkgs.symlinkJoin` dev shell.)

### macOS

Vulkan is provided via MoltenVK:

```bash
brew install molten-vk
```

### Windows

Ensure a Vulkan-capable driver and runtime (e.g. the Vulkan SDK from LunarG).

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
