# Juicity GUI (Bootstrap)

This directory contains the initial gtk-rs frontend scaffold.

## Implemented in this bootstrap

- GTK4 window with Shadowsocks-Windows-like split layout (Servers + Details)
- JSON config persistence in the platform standard config directory
- Protocol-driven core process manager (no manual core type switch):
  - Juicity profile -> `juicity-client run -c <config>`
  - Shadowsocks profile -> `sslocal -c <config>`
- DropDown-based profile/protocol selectors
- URL import/export entry for `juicity://` and `ss://` with parser validation
- Linux tray support (StatusNotifierItem via `ksni`), with Show/Hide/Quit
- System proxy apply action (Linux GNOME/KDE implemented, macOS/Windows command path scaffolded)
- Start/stop and process status polling

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

gtk-rs needs native GTK development packages.

### Linux (Debian/Ubuntu)

```bash
sudo apt update
sudo apt install -y pkg-config libgtk-4-dev
```

### Fedora

```bash
sudo dnf install -y pkgconf-pkg-config gtk4-devel
```

### Arch

```bash
sudo pacman -S --needed pkgconf gtk4
```

### macOS (Homebrew)

```bash
brew install pkg-config gtk4
```

### Windows

Install GTK4 runtime/devel and ensure `pkg-config` can resolve gtk4 related `.pc` files.
A practical path is using `vcpkg` or MSYS2 for gtk4 + pkg-config environment.

## Run

From workspace root:

```bash
cargo run -p juicity-gui
```
