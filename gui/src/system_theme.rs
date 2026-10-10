//! Desktop appearance detection, used to theme the window.
//!
//! The application tries to match the colour the user's desktop already uses,
//! so it blends with the rest of the system:
//!
//! * Windows — the DWM accent colour, from the registry, plus the light/dark
//!   preference.
//! * macOS — the accent colour and light/dark preference recorded in
//!   `NSGlobalDomain`.
//! * KDE — the accent colour and light/dark scheme from `kdeglobals`.
//! * other Linux desktops — the GTK accent colour and light/dark preference
//!   (`gsettings`, else the active GTK theme).
//!
//! Detection is best-effort: when a setting cannot be read the bundled theme
//! keeps its value. Font sizes are left to the GPUI defaults.

use gpui_kit::{App, Hsla, Window};

/// A colour sampled from the desktop, as 8-bit sRGB components.
#[derive(Clone, Copy, Debug)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    /// Perceived luminance in the `0.0..=1.0` range (Rec. 709 weights).
    fn luminance(self) -> f32 {
        (0.2126 * f32::from(self.r) + 0.7152 * f32::from(self.g) + 0.0722 * f32::from(self.b)) / 255.0
    }

    /// Whether the readable foreground on this colour is dark rather than light.
    fn is_light(self) -> bool {
        self.luminance() > 0.6
    }

    fn to_hsla(self) -> Hsla {
        let value = (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b);
        gpui_kit::rgb(value).into()
    }
}

/// Match the desktop's light/dark mode and accent colour.
///
/// Does nothing for a setting that cannot be determined.
pub fn apply(cx: &mut App) {
    apply_inner(None, cx);
}

/// Re-apply the desktop appearance, e.g. when the window's light/dark
/// preference changes at runtime.
pub fn sync(window: &mut Window, cx: &mut App) {
    apply_inner(Some(window), cx);
}

fn apply_inner(window: Option<&mut Window>, cx: &mut App) {
    // Follow the light/dark preference first: switching the mode reloads the
    // registered theme, which would overwrite the colours applied below.
    sync_mode(window, cx);

    let Some(rgb) = accent_color() else {
        tracing::debug!("no desktop appearance settings detected; keeping the built-in theme");
        return;
    };
    tracing::info!(r = rgb.r, g = rgb.g, b = rgb.b, "theming from the desktop accent colour");

    let accent = rgb.to_hsla();
    let foreground: Hsla = if rgb.is_light() {
        gpui_kit::black()
    } else {
        gpui_kit::white()
    };
    let hover = shade(accent, 1.12);
    let active = shade(accent, 0.88);

    gpui_kit::component::Theme::update(cx, |theme| {
        theme.colors.primary = accent;
        theme.colors.primary_hover = hover;
        theme.colors.primary_active = active;
        theme.colors.primary_foreground = foreground;

        // Primary buttons only fall back to `primary` when the theme file
        // did not set them, so write the button tokens explicitly.
        theme.colors.button_primary = accent;
        theme.colors.button_primary_hover = hover;
        theme.colors.button_primary_active = active;
        theme.colors.button_primary_foreground = foreground;

        // Selection / hover highlight used by menus, lists and selects.
        theme.colors.accent = accent;
        theme.colors.accent_foreground = foreground;

        // Focus ring.
        theme.colors.ring = accent;
    });
}

/// Follow the system light/dark preference.
///
/// Linux desktops describe the preference in their own settings, which is more
/// reliable than GPUI's platform detection there; elsewhere GPUI reports the
/// system appearance.
fn sync_mode(window: Option<&mut Window>, cx: &mut App) {
    #[cfg(target_os = "linux")]
    {
        if let Some(dark) = linux_prefers_dark() {
            let mode = if dark {
                gpui_kit::component::ThemeMode::Dark
            } else {
                gpui_kit::component::ThemeMode::Light
            };
            gpui_kit::component::Theme::change(mode, window, cx);
            return;
        }
    }
    gpui_kit::component::Theme::sync_system_appearance(window, cx);
}

/// Blend an opaque colour toward white (`factor > 1`) or black (`factor < 1`).
fn shade(color: Hsla, factor: f32) -> Hsla {
    Hsla {
        l: (color.l * factor).clamp(0.0, 1.0),
        ..color
    }
}

fn accent_color() -> Option<Rgb> {
    #[cfg(target_os = "windows")]
    {
        windows_accent()
    }
    #[cfg(target_os = "macos")]
    {
        macos_accent()
    }
    #[cfg(target_os = "linux")]
    {
        linux_accent()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

// ── Windows ──────────────────────────────────────────────────────────────

/// The Windows accent colour, which DWM stores as `0xAABBGGRR`.
#[cfg(target_os = "windows")]
fn windows_accent() -> Option<Rgb> {
    use windows_registry::CURRENT_USER;

    let abgr = CURRENT_USER
        .open(r"Software\Microsoft\Windows\DWM")
        .ok()
        .and_then(|key| key.get_u32("AccentColor").ok())
        .or_else(|| {
            CURRENT_USER
                .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent")
                .ok()
                .and_then(|key| key.get_u32("AccentColorMenu").ok())
        })?;

    Some(Rgb {
        r: (abgr & 0xff) as u8,
        g: ((abgr >> 8) & 0xff) as u8,
        b: ((abgr >> 16) & 0xff) as u8,
    })
}

// ── macOS ────────────────────────────────────────────────────────────────

/// The macOS accent colour.
///
/// `AppleHighlightColor` is a `<r> <g> <b> <name>` string with components in
/// the `0..=1` range; when it is absent the system default (blue) applies.
#[cfg(target_os = "macos")]
fn macos_accent() -> Option<Rgb> {
    if let Some(text) = run_command("defaults", &["read", "-g", "AppleHighlightColor"]) {
        if let Some(rgb) = parse_unit_triplet(&text) {
            return Some(rgb);
        }
    }
    Some(Rgb {
        r: 0x00,
        g: 0x7a,
        b: 0xff,
    })
}

#[cfg(target_os = "macos")]
fn parse_unit_triplet(text: &str) -> Option<Rgb> {
    let mut parts = text.split_whitespace();
    let r: f64 = parts.next()?.parse().ok()?;
    let g: f64 = parts.next()?.parse().ok()?;
    let b: f64 = parts.next()?.parse().ok()?;
    let to_u8 = |value: f64| (value * 255.0).round().clamp(0.0, 255.0) as u8;
    Some(Rgb {
        r: to_u8(r),
        g: to_u8(g),
        b: to_u8(b),
    })
}

// ── Linux ────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn linux_accent() -> Option<Rgb> {
    if is_kde() {
        if let Some(rgb) = kde_accent() {
            return Some(rgb);
        }
    }
    gtk_accent()
}

/// Linux desktops expose the light/dark preference in their own settings.
#[cfg(target_os = "linux")]
fn linux_prefers_dark() -> Option<bool> {
    if is_kde() {
        if let Some(dark) = kde_prefers_dark() {
            return Some(dark);
        }
    }
    gtk_prefers_dark()
}

/// KDE's active colour scheme is named in `kdeglobals`; a dark scheme either
/// says so in its name or paints a dark window background.
#[cfg(target_os = "linux")]
fn kde_prefers_dark() -> Option<bool> {
    let text = std::fs::read_to_string(config_home().join("kdeglobals")).ok()?;
    if let Some(scheme) = ini_value(&text, "General", "ColorScheme") {
        return Some(scheme.to_ascii_lowercase().contains("dark"));
    }
    Some(parse_ini_color(&text, "Colors:Window", "BackgroundNormal")?.luminance() < 0.5)
}

/// GTK/GNOME store the preference in `color-scheme` (`prefer-dark` when set).
#[cfg(target_os = "linux")]
fn gtk_prefers_dark() -> Option<bool> {
    Some(gsettings_string("color-scheme")?.contains("dark"))
}

#[cfg(target_os = "linux")]
fn gsettings_string(key: &str) -> Option<String> {
    let text = run_command("gsettings", &["get", "org.gnome.desktop.interface", key])?;
    Some(text.trim().trim_matches(['\'', '"']).to_string())
}

#[cfg(target_os = "linux")]
fn is_kde() -> bool {
    if std::env::var_os("KDE_FULL_SESSION").is_some() {
        return true;
    }
    ["XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .any(|value| value.to_ascii_lowercase().contains("kde"))
}

/// KDE records the accent explicitly on Plasma 6 and exposes it as the
/// selection background on older versions, both in `kdeglobals`.
#[cfg(target_os = "linux")]
fn kde_accent() -> Option<Rgb> {
    let text = std::fs::read_to_string(config_home().join("kdeglobals")).ok()?;
    parse_ini_color(&text, "General", "AccentColor")
        .or_else(|| parse_ini_color(&text, "Colors:Selection", "BackgroundNormal"))
}

/// The GTK accent colour: the `accent-color` setting when the desktop provides
/// one, otherwise the active theme's selected background.
#[cfg(target_os = "linux")]
fn gtk_accent() -> Option<Rgb> {
    if let Some(text) = run_command("gsettings", &["get", "org.gnome.desktop.interface", "accent-color"])
    {
        let name = text.trim().trim_matches(['\'', '"']).to_ascii_lowercase();
        if let Some(rgb) = gtk_named_accent(&name) {
            return Some(rgb);
        }
    }
    gtk_theme_selection()
}

#[cfg(target_os = "linux")]
fn gtk_named_accent(name: &str) -> Option<Rgb> {
    Some(match name {
        "blue" => Rgb { r: 0x35, g: 0x84, b: 0xe4 },
        "teal" => Rgb { r: 0x21, g: 0x90, b: 0xa4 },
        "green" => Rgb { r: 0x3a, g: 0x94, b: 0x4a },
        "yellow" => Rgb { r: 0xc8, g: 0x88, b: 0x00 },
        "orange" => Rgb { r: 0xed, g: 0x5b, b: 0x00 },
        "red" => Rgb { r: 0xe6, g: 0x2d, b: 0x42 },
        "pink" => Rgb { r: 0xd5, g: 0x61, b: 0x99 },
        "purple" => Rgb { r: 0x91, g: 0x41, b: 0xac },
        "slate" => Rgb { r: 0x6f, g: 0x83, b: 0x96 },
        _ => return None,
    })
}

/// Read `@define-color theme_selected_bg_color ...;` from the active GTK theme.
#[cfg(target_os = "linux")]
fn gtk_theme_selection() -> Option<Rgb> {
    let theme = gtk_theme_name()?;
    let candidates = [theme.clone(), format!("{theme}-dark")];
    for root in ["/usr/share/themes", "/usr/local/share/themes"] {
        for variant in &candidates {
            for version in ["gtk-4.0", "gtk-3.0"] {
                let css = std::path::Path::new(root)
                    .join(variant)
                    .join(version)
                    .join("gtk.css");
                if let Ok(text) = std::fs::read_to_string(css) {
                    if let Some(rgb) = parse_define_color(&text, "theme_selected_bg_color") {
                        return Some(rgb);
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn gtk_theme_name() -> Option<String> {
    let text = std::fs::read_to_string(config_home().join("gtk-3.0/settings.ini")).ok()?;
    ini_value(&text, "Settings", "gtk-theme-name").map(str::to_string)
}

#[cfg(target_os = "linux")]
fn config_home() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config")))
        .unwrap_or_default()
}

// ── Shared helpers ───────────────────────────────────────────────────────

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Value of `key` inside `[section]` of an INI-style file, if present.
#[cfg(any(target_os = "linux", test))]
fn ini_value<'a>(text: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            if let Some(name) = rest.strip_suffix(']') {
                in_section = name.eq_ignore_ascii_case(section);
            }
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            if name.trim().eq_ignore_ascii_case(key) {
                return Some(value.trim());
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_ini_color(text: &str, section: &str, key: &str) -> Option<Rgb> {
    parse_rgb_triplet(ini_value(text, section, key)?)
}

/// Parse a `R,G,B` triplet as KDE writes colours.
#[cfg(any(target_os = "linux", test))]
fn parse_rgb_triplet(value: &str) -> Option<Rgb> {
    let mut parts = value.split(',').map(str::trim);
    Some(Rgb {
        r: parts.next()?.parse().ok()?,
        g: parts.next()?.parse().ok()?,
        b: parts.next()?.parse().ok()?,
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_define_color(text: &str, name: &str) -> Option<Rgb> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("@define-color") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        match parts.next() {
            Some(defined) if defined == name => {}
            _ => continue,
        }
        if let Some(value) = parts.next() {
            if let Some(rgb) = parse_hex_color(value.trim_end_matches(';')) {
                return Some(rgb);
            }
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_hex_color(value: &str) -> Option<Rgb> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    Some(Rgb {
        r: u8::from_str_radix(&hex[0..2], 16).ok()?,
        g: u8::from_str_radix(&hex[2..4], 16).ok()?,
        b: u8::from_str_radix(&hex[4..6], 16).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_triplet_parses_with_spaces() {
        let rgb = parse_rgb_triplet("56, 132, 255").unwrap();
        assert_eq!((rgb.r, rgb.g, rgb.b), (56, 132, 255));
    }

    #[test]
    fn ini_value_matches_section_and_key() {
        let text = "[General]\nAccentColor=56,132,255\n[Other]\nAccentColor=1,2,3\n";
        let rgb = parse_ini_color(text, "General", "AccentColor").unwrap();
        assert_eq!((rgb.r, rgb.g, rgb.b), (56, 132, 255));
    }

    #[test]
    fn define_color_reads_hex() {
        let css = "@define-color theme_selected_bg_color #3584e4;\n";
        let rgb = parse_define_color(css, "theme_selected_bg_color").unwrap();
        assert_eq!((rgb.r, rgb.g, rgb.b), (0x35, 0x84, 0xe4));
    }

    #[test]
    fn shade_lightens_and_darkens() {
        let base = Hsla { h: 0.5, s: 0.5, l: 0.5, a: 1.0 };
        assert!(shade(base, 1.2).l > base.l);
        assert!(shade(base, 0.8).l < base.l);
    }
}
