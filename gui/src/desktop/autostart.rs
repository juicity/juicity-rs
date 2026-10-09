//! Start the GUI with the desktop session.
//!
//! Linux writes an XDG autostart entry; Windows and macOS follow in M4.

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

/// Whether this platform can start the GUI at login yet.
pub const SUPPORTED: bool = cfg!(target_os = "linux");

/// Name of the autostart entry, matching the application id.
#[cfg(target_os = "linux")]
const ENTRY: &str = "io.juicity.gui.desktop";

/// Enable or disable starting the GUI at login.
pub fn apply(enabled: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("juicity-gui"));
        apply_with(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
            enabled,
            &exe,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = enabled;
        Ok(())
    }
}

/// [`apply`] with explicit environment values. Disabling also removes an
/// entry left in `$HOME/.config/autostart` when `XDG_CONFIG_HOME` points
/// elsewhere.
#[cfg(target_os = "linux")]
pub fn apply_with(
    xdg_config: Option<OsString>,
    home: Option<OsString>,
    enabled: bool,
    exe: &Path,
) -> anyhow::Result<()> {
    let legacy = home
        .as_ref()
        .map(|home| PathBuf::from(home).join(".config/autostart"));
    let dir = autostart_dir(xdg_config, home)?;
    apply_in(&dir, enabled, exe)?;
    match legacy {
        Some(legacy) if !enabled && legacy != dir => apply_in(&legacy, false, exe),
        _ => Ok(()),
    }
}

/// `$XDG_CONFIG_HOME/autostart`, or `$HOME/.config/autostart` when it is
/// unset or relative (XDG Base Directory rules).
#[cfg(target_os = "linux")]
pub fn autostart_dir(
    xdg_config: Option<OsString>,
    home: Option<OsString>,
) -> anyhow::Result<PathBuf> {
    let config = xdg_config
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(config.join("autostart"))
}

/// Write or remove the autostart entry in `dir`.
#[cfg(target_os = "linux")]
pub fn apply_in(dir: &Path, enabled: bool, exe: &Path) -> anyhow::Result<()> {
    let file = dir.join(ENTRY);
    if !enabled {
        match std::fs::remove_file(&file) {
            Ok(()) => tracing::info!("autostart entry removed from {}", file.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&file, desktop_entry(exe))?;
    tracing::info!("autostart entry created at {}", file.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Juicity GUI\n\
         Comment=Juicity GUI Client\n\
         Exec={}\n\
         Icon=io.juicity.gui\n\
         Terminal=false\n\
         Categories=Network;\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_argument(&exe.to_string_lossy())
    )
}

/// Encode `arg` as one `Exec` argument (Desktop Entry spec): quote it when
/// it has reserved characters, escape `"` `` ` `` `$` `\` inside the quotes,
/// double `%` so it is not a field code, then apply the string escapes.
#[cfg(target_os = "linux")]
fn exec_argument(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let quoted = if arg.contains(RESERVED) {
        let mut out = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        arg.to_string()
    };
    let mut out = String::new();
    for c in quoted.chars() {
        match c {
            '%' => out.push_str("%%"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("juicity-autostart-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn dir_follows_xdg_config_home_then_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            autostart_dir(xdg.map(Into::into), home.map(Into::into)).ok()
        };
        assert_eq!(
            dir(Some("/cfg"), Some("/home/u")),
            Some(PathBuf::from("/cfg/autostart"))
        );
        assert_eq!(
            dir(Some("relative"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
        assert_eq!(
            dir(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/autostart"))
        );
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn entry_is_created_then_removed() {
        let dir = temp("toggle");
        let exe = Path::new("/usr/bin/juicity-gui");
        apply_in(&dir, true, exe).unwrap();
        let entry = std::fs::read_to_string(dir.join(ENTRY)).unwrap();
        assert!(entry.contains("Exec=/usr/bin/juicity-gui\n"), "{entry}");
        apply_in(&dir, false, exe).unwrap();
        assert!(!dir.join(ENTRY).exists());
        // Disabling twice is not an error.
        apply_in(&dir, false, exe).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_is_one_escaped_argument() {
        assert_eq!(
            exec_argument("/usr/bin/juicity-gui"),
            "/usr/bin/juicity-gui"
        );
        assert_eq!(
            exec_argument("/opt/My Apps/juicity 100%/juicity-gui"),
            "\"/opt/My Apps/juicity 100%%/juicity-gui\""
        );
        assert_eq!(exec_argument("/opt/50%/gui"), "/opt/50%%/gui");
        // `\$` inside quotes, then the string-level `\\`.
        assert_eq!(exec_argument("/a$b"), "\"/a\\\\$b\"");
        assert_eq!(exec_argument("/a\"b"), "\"/a\\\\\"b\"");
        let dir = temp("exec");
        apply_in(&dir, true, Path::new("/opt/My Apps/100%/juicity-gui")).unwrap();
        let entry = std::fs::read_to_string(dir.join(ENTRY)).unwrap();
        assert!(
            entry.contains("Exec=\"/opt/My Apps/100%%/juicity-gui\"\n"),
            "{entry}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabling_also_removes_the_legacy_home_entry() {
        let root = temp("legacy");
        let home = root.join("home");
        let xdg = root.join("xdg");
        let exe = Path::new("/usr/bin/juicity-gui");
        apply_in(&home.join(".config/autostart"), true, exe).unwrap();
        apply_with(
            Some(xdg.clone().into()),
            Some(home.clone().into()),
            true,
            exe,
        )
        .unwrap();
        assert!(xdg.join("autostart").join(ENTRY).exists());
        apply_with(
            Some(xdg.clone().into()),
            Some(home.clone().into()),
            false,
            exe,
        )
        .unwrap();
        assert!(!xdg.join("autostart").join(ENTRY).exists());
        assert!(!home.join(".config/autostart").join(ENTRY).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
