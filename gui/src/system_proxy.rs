use crate::config::{AppConfig, PacMode, SystemProxyMode};
use anyhow::bail;
use std::process::Command;

/// Command that never flashes a console window on Windows.
fn command(program: &str) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

pub fn apply_system_proxy(config: &AppConfig) -> anyhow::Result<()> {
    // The mixed inbound answers HTTP and SOCKS5 on the same address, so both
    // the HTTP and the SOCKS entries point at it.
    let listen = &config.mixed_listen;
    let pac_url = match config.pac_mode {
        PacMode::Online => config
            .online_pac_url
            .clone()
            .unwrap_or_else(|| crate::pac::pac_url(&config.pac_listen)),
        PacMode::Local => crate::pac::pac_url(&config.pac_listen),
    };

    #[cfg(target_os = "linux")]
    {
        return apply_linux(config.system_proxy_mode, &pac_url, listen);
    }

    #[cfg(target_os = "macos")]
    {
        return apply_macos(config.system_proxy_mode, &pac_url, listen);
    }

    #[cfg(target_os = "windows")]
    {
        return apply_windows(config.system_proxy_mode, &pac_url, listen);
    }

    #[allow(unreachable_code)]
    {
        tracing::warn!("system proxy backend is not implemented on this OS yet");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_linux(mode: SystemProxyMode, pac_url: &str, listen: &str) -> anyhow::Result<()> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let desktop_lower = desktop.to_lowercase();

    if desktop_lower.contains("gnome") {
        // Detected GNOME desktop environment; only apply GNOME settings.
        match apply_linux_gnome(mode, pac_url, listen) {
            Ok(true) => Ok(()),
            Ok(false) => bail!("GNOME proxy apply failed (gsettings not found)"),
            Err(err) => bail!("GNOME proxy apply failed: {err}"),
        }
    } else if desktop_lower.contains("kde") {
        // Detected KDE desktop environment; only apply KDE settings.
        match apply_linux_kde(mode, pac_url, listen) {
            Ok(true) => Ok(()),
            Ok(false) => bail!("KDE proxy apply failed (kwriteconfig6 or kwriteconfig5 not found)"),
            Err(err) => bail!("KDE proxy apply failed: {err}"),
        }
    } else {
        // Unknown or unset XDG_CURRENT_DESKTOP; try both backends for backward compatibility.
        let mut gnome_ok = false;
        let mut kde_ok = false;

        match apply_linux_gnome(mode, pac_url, listen) {
            Ok(ok) => gnome_ok = ok,
            Err(err) => tracing::warn!("GNOME proxy apply failed: {err}"),
        }

        match apply_linux_kde(mode, pac_url, listen) {
            Ok(ok) => kde_ok = ok,
            Err(err) => tracing::warn!("KDE proxy apply failed: {err}"),
        }

        if gnome_ok || kde_ok {
            Ok(())
        } else {
            bail!("no Linux system proxy backend available (need gsettings and/or kwriteconfig6 or kwriteconfig5)")
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_gnome(mode: SystemProxyMode, pac_url: &str, listen: &str) -> anyhow::Result<bool> {
    let mut ok = false;
    match mode {
        SystemProxyMode::Disable => {
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "mode", "none"],
            )?;
        }
        SystemProxyMode::Pac => {
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "mode", "auto"],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "autoconfig-url", pac_url],
            )?;
        }
        SystemProxyMode::Global => {
            let (host, port) = crate::util::split_host_port(listen);
            let port = port.to_string();

            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "mode", "manual"],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "use-same-proxy", "true"],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.http", "host", host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.http", "port", &port],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.https", "host", host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.https", "port", &port],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.socks", "host", host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.socks", "port", &port],
            )?;
        }
    }

    Ok(ok)
}

#[cfg(target_os = "linux")]
fn apply_linux_kde(mode: SystemProxyMode, pac_url: &str, listen: &str) -> anyhow::Result<bool> {
    let mut ok = false;

    match mode {
        SystemProxyMode::Disable => {
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "ProxyType",
                "0",
            ])?;
        }
        SystemProxyMode::Pac => {
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "ProxyType",
                "2",
            ])?;
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "Proxy Config Script",
                pac_url,
            ])?;
        }
        SystemProxyMode::Global => {
            let (host, port) = crate::util::split_host_port(listen);
            let port = port.to_string();
            let host = if host.contains(':') {
                format!("[{}]", host)
            } else {
                host.to_string()
            };

            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "ProxyType",
                "1",
            ])?;
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "httpProxy",
                &format!("http://{} {}", host, port),
            ])?;
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "httpsProxy",
                &format!("http://{} {}", host, port),
            ])?;
            ok |= kwriteconfig(&[
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                "socksProxy",
                &format!("socks://{} {}", host, port),
            ])?;
        }
    }

    // Refresh KIO where available (best-effort; must not affect `ok` so that
    // a missing kwriteconfig isn't falsely counted as a success).
    let reparse = ["org.kde.KIO", "/KIO/Scheduler", "reparseSlaveConfiguration"];
    if !matches!(run_if_available("qdbus6", &reparse), Ok(true)) {
        let _ = run_if_available("qdbus", &reparse);
    }

    Ok(ok)
}

/// Write a KDE setting with Plasma 6's `kwriteconfig6`, falling back to
/// Plasma 5's `kwriteconfig5`; `Ok(false)` when neither is installed.
#[cfg(target_os = "linux")]
fn kwriteconfig(args: &[&str]) -> anyhow::Result<bool> {
    if run_if_available("kwriteconfig6", args)? {
        return Ok(true);
    }
    run_if_available("kwriteconfig5", args)
}

#[cfg(target_os = "macos")]
fn apply_macos(mode: SystemProxyMode, pac_url: &str, listen: &str) -> anyhow::Result<()> {
    let services = list_macos_network_services()?;
    if services.is_empty() {
        bail!("no active macOS network services found")
    }

    for service in services {
        match mode {
            SystemProxyMode::Disable => {
                run_required("networksetup", &["-setwebproxystate", &service, "off"])?;
                run_required(
                    "networksetup",
                    &["-setsecurewebproxystate", &service, "off"],
                )?;
                run_required(
                    "networksetup",
                    &["-setsocksfirewallproxystate", &service, "off"],
                )?;
                run_required("networksetup", &["-setautoproxystate", &service, "off"])?;
            }
            SystemProxyMode::Pac => {
                run_required("networksetup", &["-setautoproxyurl", &service, pac_url])?;
                run_required("networksetup", &["-setautoproxystate", &service, "on"])?;
                run_required("networksetup", &["-setwebproxystate", &service, "off"])?;
                run_required(
                    "networksetup",
                    &["-setsecurewebproxystate", &service, "off"],
                )?;
                run_required(
                    "networksetup",
                    &["-setsocksfirewallproxystate", &service, "off"],
                )?;
            }
            SystemProxyMode::Global => {
                // Only parse host/port in the branch that actually needs them.
                let (host, port) = crate::util::split_host_port(listen);
                let port = port.to_string();

                run_required("networksetup", &["-setwebproxy", &service, host, &port])?;
                run_required(
                    "networksetup",
                    &["-setsecurewebproxy", &service, host, &port],
                )?;
                run_required(
                    "networksetup",
                    &["-setsocksfirewallproxy", &service, host, &port],
                )?;
                run_required("networksetup", &["-setwebproxystate", &service, "on"])?;
                run_required("networksetup", &["-setsecurewebproxystate", &service, "on"])?;
                run_required(
                    "networksetup",
                    &["-setsocksfirewallproxystate", &service, "on"],
                )?;
                run_required("networksetup", &["-setautoproxystate", &service, "off"])?;
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn list_macos_network_services() -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;

    let output = Command::new("networksetup")
        .arg("-listallnetworkservices")
        .output()
        .context("failed to run networksetup -listallnetworkservices")?;

    if !output.status.success() {
        bail!("networksetup -listallnetworkservices failed")
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let services = stdout
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('*'))
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

    Ok(services)
}

#[cfg(target_os = "windows")]
const INTERNET_SETTINGS: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

#[cfg(target_os = "windows")]
fn apply_windows(mode: SystemProxyMode, pac_url: &str, listen: &str) -> anyhow::Result<()> {
    let bypass = windows_bypass();
    match mode {
        SystemProxyMode::Disable => {
            reg_add("ProxyEnable", "REG_DWORD", "0")?;
            reg_delete("AutoConfigURL");
            // Remove only the values Global mode wrote, so a proxy the user
            // configured by hand is left as it was.
            if reg_query("ProxyServer").as_deref() == Some(listen) {
                reg_delete("ProxyServer");
            }
            if reg_query("ProxyOverride").as_deref() == Some(bypass.as_str()) {
                reg_delete("ProxyOverride");
            }
        }
        SystemProxyMode::Pac => {
            reg_add("ProxyEnable", "REG_DWORD", "0")?;
            reg_add("AutoConfigURL", "REG_SZ", pac_url)?;
        }
        SystemProxyMode::Global => {
            reg_add("ProxyEnable", "REG_DWORD", "1")?;
            reg_add("ProxyServer", "REG_SZ", listen)?;
            reg_add("ProxyOverride", "REG_SZ", &bypass)?;
            // A PAC script would take precedence over the global proxy.
            reg_delete("AutoConfigURL");
        }
    }

    Ok(())
}

/// `ProxyOverride` for Global mode: local names, loopback and private ranges
/// go direct.
#[cfg(any(target_os = "windows", test))]
fn windows_bypass() -> String {
    let mut hosts = vec!["localhost".to_string(), "127.*".into(), "10.*".into()];
    hosts.extend((16..32).map(|n| format!("172.{n}.*")));
    hosts.push("192.168.*".into());
    hosts.push("<local>".into());
    hosts.join(";")
}

#[cfg(target_os = "windows")]
fn reg_add(name: &str, kind: &str, data: &str) -> anyhow::Result<()> {
    run_required(
        "reg",
        &[
            "add",
            INTERNET_SETTINGS,
            "/v",
            name,
            "/t",
            kind,
            "/d",
            data,
            "/f",
        ],
    )
}

/// `reg delete` exits non-zero when the value is absent; the end state is the
/// same either way, so the exit code is ignored.
#[cfg(target_os = "windows")]
fn reg_delete(name: &str) {
    let _ = command("reg")
        .args(["delete", INTERNET_SETTINGS, "/v", name, "/f"])
        .status();
}

#[cfg(target_os = "windows")]
fn reg_query(name: &str) -> Option<String> {
    let output = command("reg")
        .args(["query", INTERNET_SETTINGS, "/v", name])
        .output()
        .ok()?;
    parse_reg_sz(&String::from_utf8_lossy(&output.stdout), name)
}

/// The data of `name` in `reg query` output, e.g.
/// `    ProxyServer    REG_SZ    127.0.0.1:1080`.
#[cfg(any(target_os = "windows", test))]
fn parse_reg_sz(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(name)?;
        let data = rest.trim_start().strip_prefix("REG_SZ")?;
        Some(data.trim().to_string())
    })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_required(program: &str, args: &[&str]) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let status = command(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {} {:?}", program, args))?;

    if !status.success() {
        bail!("{} {:?} exited with {}", program, args, status);
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_if_available(program: &str, args: &[&str]) -> anyhow::Result<bool> {
    let mut cmd = command(program);
    cmd.args(args);

    let status = match cmd.status() {
        Ok(v) => v,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to run {} {:?}: {}",
                program,
                args,
                err
            ))
        }
    };

    if !status.success() {
        return Err(anyhow::anyhow!(
            "{} {:?} exited with {}",
            program,
            args,
            status
        ));
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_reg_sz_value() {
        let output = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyServer    REG_SZ    127.0.0.1:1080\r\n\r\n";
        assert_eq!(
            parse_reg_sz(output, "ProxyServer").as_deref(),
            Some("127.0.0.1:1080")
        );
        assert_eq!(parse_reg_sz(output, "ProxyOverride"), None);
        assert_eq!(parse_reg_sz("", "ProxyServer"), None);
    }

    #[test]
    fn bypass_covers_local_and_private_hosts() {
        let bypass = windows_bypass();
        let hosts: Vec<&str> = bypass.split(';').collect();
        for host in [
            "localhost",
            "127.*",
            "10.*",
            "172.16.*",
            "172.31.*",
            "192.168.*",
            "<local>",
        ] {
            assert!(hosts.contains(&host), "{host} missing from {bypass}");
        }
        assert!(!hosts.contains(&"172.32.*"));
    }
}
