use crate::config::{AppConfig, PacMode, SystemProxyMode};
use anyhow::bail;
use std::process::Command;

pub fn apply_system_proxy(config: &AppConfig) -> anyhow::Result<()> {
    let pac_url = match config.pac_mode {
        PacMode::Online => config
            .online_pac_url
            .clone()
            .unwrap_or_else(|| crate::pac::pac_url(&config.pac_listen)),
        PacMode::Local => crate::pac::pac_url(&config.pac_listen),
    };

    #[cfg(target_os = "linux")]
    {
        return apply_linux(config.system_proxy_mode, &pac_url, &config.http_listen, &config.socks_listen);
    }

    #[cfg(target_os = "macos")]
    {
        return apply_macos(config.system_proxy_mode, &pac_url, &config.http_listen, &config.socks_listen);
    }

    #[cfg(target_os = "windows")]
    {
        return apply_windows(config.system_proxy_mode, &pac_url, &config.http_listen, &config.socks_listen);
    }

    #[allow(unreachable_code)]
    {
        tracing::warn!("system proxy backend is not implemented on this OS yet");
        Ok(())
    }
}


#[cfg(target_os = "linux")]
fn apply_linux(mode: SystemProxyMode, pac_url: &str, http_listen: &str, socks_listen: &str) -> anyhow::Result<()> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let desktop_lower = desktop.to_lowercase();

    if desktop_lower.contains("gnome") {
        // Detected GNOME desktop environment; only apply GNOME settings.
        match apply_linux_gnome(mode, pac_url, http_listen, socks_listen) {
            Ok(true) => Ok(()),
            Ok(false) => bail!("GNOME proxy apply failed (gsettings not found)"),
            Err(err) => bail!("GNOME proxy apply failed: {err}"),
        }
    } else if desktop_lower.contains("kde") {
        // Detected KDE desktop environment; only apply KDE settings.
        match apply_linux_kde(mode, pac_url, http_listen, socks_listen) {
            Ok(true) => Ok(()),
            Ok(false) => bail!("KDE proxy apply failed (kwriteconfig5 not found)"),
            Err(err) => bail!("KDE proxy apply failed: {err}"),
        }
    } else {
        // Unknown or unset XDG_CURRENT_DESKTOP; try both backends for backward compatibility.
        let mut gnome_ok = false;
        let mut kde_ok = false;

        match apply_linux_gnome(mode, pac_url, http_listen, socks_listen) {
            Ok(ok) => gnome_ok = ok,
            Err(err) => tracing::warn!("GNOME proxy apply failed: {err}"),
        }

        match apply_linux_kde(mode, pac_url, http_listen, socks_listen) {
            Ok(ok) => kde_ok = ok,
            Err(err) => tracing::warn!("KDE proxy apply failed: {err}"),
        }

        if gnome_ok || kde_ok {
            Ok(())
        } else {
            bail!("no Linux system proxy backend available (need gsettings and/or kwriteconfig5)")
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_gnome(mode: SystemProxyMode, pac_url: &str, http_listen: &str, socks_listen: &str) -> anyhow::Result<bool> {
    let mut ok = false;
    match mode {
        SystemProxyMode::Disable => {
            ok |= run_if_available("gsettings", &["set", "org.gnome.system.proxy", "mode", "none"])?;
        }
        SystemProxyMode::Pac => {
            ok |= run_if_available("gsettings", &["set", "org.gnome.system.proxy", "mode", "auto"])?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy", "autoconfig-url", pac_url],
            )?;
        }
        SystemProxyMode::Global => {
            let (http_host, http_port) = crate::util::split_host_port(http_listen);
            let (socks_host, socks_port) = crate::util::split_host_port(socks_listen);

            ok |= run_if_available("gsettings", &["set", "org.gnome.system.proxy", "mode", "manual"])?;
            ok |= run_if_available("gsettings", &["set", "org.gnome.system.proxy", "use-same-proxy", "true"])?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.http", "host", http_host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.http", "port", &http_port.to_string()],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.https", "host", http_host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.https", "port", &http_port.to_string()],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.socks", "host", socks_host],
            )?;
            ok |= run_if_available(
                "gsettings",
                &["set", "org.gnome.system.proxy.socks", "port", &socks_port.to_string()],
            )?;
        }
    }

    Ok(ok)
}

#[cfg(target_os = "linux")]
fn apply_linux_kde(mode: SystemProxyMode, pac_url: &str, http_listen: &str, socks_listen: &str) -> anyhow::Result<bool> {
    let mut ok = false;

    match mode {
        SystemProxyMode::Disable => {
            ok |= run_if_available(
                "kwriteconfig5",
                &["--file", "kioslaverc", "--group", "Proxy Settings", "--key", "ProxyType", "0"],
            )?;
        }
        SystemProxyMode::Pac => {
            ok |= run_if_available(
                "kwriteconfig5",
                &["--file", "kioslaverc", "--group", "Proxy Settings", "--key", "ProxyType", "2"],
            )?;
            ok |= run_if_available(
                "kwriteconfig5",
                &[
                    "--file",
                    "kioslaverc",
                    "--group",
                    "Proxy Settings",
                    "--key",
                    "Proxy Config Script",
                    pac_url,
                ],
            )?;
        }
        SystemProxyMode::Global => {
            let (http_host, http_port) = crate::util::split_host_port(http_listen);
            let (socks_host, socks_port) = crate::util::split_host_port(socks_listen);

            let http_host = if http_host.contains(':') {
                format!("[{}]", http_host)
            } else {
                http_host.to_string()
            };
            let socks_host = if socks_host.contains(':') {
                format!("[{}]", socks_host)
            } else {
                socks_host.to_string()
            };

            ok |= run_if_available(
                "kwriteconfig5",
                &["--file", "kioslaverc", "--group", "Proxy Settings", "--key", "ProxyType", "1"],
            )?;
            ok |= run_if_available(
                "kwriteconfig5",
                &[
                    "--file",
                    "kioslaverc",
                    "--group",
                    "Proxy Settings",
                    "--key",
                    "httpProxy",
                    &format!("http://{} {}", http_host, http_port),
                ],
            )?;
            ok |= run_if_available(
                "kwriteconfig5",
                &[
                    "--file",
                    "kioslaverc",
                    "--group",
                    "Proxy Settings",
                    "--key",
                    "httpsProxy",
                    &format!("http://{} {}", http_host, http_port),
                ],
            )?;
            ok |= run_if_available(
                "kwriteconfig5",
                &[
                    "--file",
                    "kioslaverc",
                    "--group",
                    "Proxy Settings",
                    "--key",
                    "socksProxy",
                    &format!("socks://{} {}", socks_host, socks_port),
                ],
            )?;
        }
    }

    // Refresh KIO where available (best-effort; must not affect `ok` so that
    // a missing kwriteconfig5 isn't falsely counted as a success).
    let _ = run_if_available(
        "qdbus",
        &["org.kde.KIO", "/KIO/Scheduler", "reparseSlaveConfiguration"],
    );

    Ok(ok)
}

#[cfg(target_os = "macos")]
fn apply_macos(mode: SystemProxyMode, pac_url: &str, http_listen: &str, socks_listen: &str) -> anyhow::Result<()> {
    let services = list_macos_network_services()?;
    if services.is_empty() {
        bail!("no active macOS network services found")
    }

    for service in services {
        match mode {
            SystemProxyMode::Disable => {
                run_required("networksetup", &["-setwebproxystate", &service, "off"])?;
                run_required("networksetup", &["-setsecurewebproxystate", &service, "off"])?;
                run_required("networksetup", &["-setsocksfirewallproxystate", &service, "off"])?;
                run_required("networksetup", &["-setautoproxystate", &service, "off"])?;
            }
            SystemProxyMode::Pac => {
                run_required("networksetup", &["-setautoproxyurl", &service, pac_url])?;
                run_required("networksetup", &["-setautoproxystate", &service, "on"])?;
                run_required("networksetup", &["-setwebproxystate", &service, "off"])?;
                run_required("networksetup", &["-setsecurewebproxystate", &service, "off"])?;
                run_required("networksetup", &["-setsocksfirewallproxystate", &service, "off"])?;
            }
            SystemProxyMode::Global => {
                // Only parse host/port in the branch that actually needs them.
                let (http_host, http_port) = crate::util::split_host_port(http_listen);
                let (socks_host, socks_port) = crate::util::split_host_port(socks_listen);

                run_required(
                    "networksetup",
                    &["-setwebproxy", &service, &http_host, &http_port.to_string()],
                )?;
                run_required(
                    "networksetup",
                    &["-setsecurewebproxy", &service, &http_host, &http_port.to_string()],
                )?;
                run_required(
                    "networksetup",
                    &[
                        "-setsocksfirewallproxy",
                        &service,
                        &socks_host,
                        &socks_port.to_string(),
                    ],
                )?;
                run_required("networksetup", &["-setwebproxystate", &service, "on"])?;
                run_required("networksetup", &["-setsecurewebproxystate", &service, "on"])?;
                run_required("networksetup", &["-setsocksfirewallproxystate", &service, "on"])?;
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
fn apply_windows(mode: SystemProxyMode, pac_url: &str, http_listen: &str, _socks_listen: &str) -> anyhow::Result<()> {
    match mode {
        SystemProxyMode::Disable => {
            run_required(
                "reg",
                &[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "ProxyEnable",
                    "/t",
                    "REG_DWORD",
                    "/d",
                    "0",
                    "/f",
                ],
            )?;
            // `reg delete` exits non-zero when the value doesn't exist (e.g. proxy
            // was previously set to Global mode and AutoConfigURL was never written).
            // Ignore the exit code — the desired end-state (no AutoConfigURL) is the
            // same regardless of whether the value was present beforehand.
            let _ = Command::new("reg")
                .args(&[
                    "delete",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "AutoConfigURL",
                    "/f",
                ])
                .status();
        }
        SystemProxyMode::Pac => {
            run_required(
                "reg",
                &[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "ProxyEnable",
                    "/t",
                    "REG_DWORD",
                    "/d",
                    "0",
                    "/f",
                ],
            )?;
            run_required(
                "reg",
                &[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "AutoConfigURL",
                    "/t",
                    "REG_SZ",
                    "/d",
                    pac_url,
                    "/f",
                ],
            )?;
        }
        SystemProxyMode::Global => {
            run_required(
                "reg",
                &[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "ProxyEnable",
                    "/t",
                    "REG_DWORD",
                    "/d",
                    "1",
                    "/f",
                ],
            )?;
            run_required(
                "reg",
                &[
                    "add",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    "ProxyServer",
                    "/t",
                    "REG_SZ",
                    "/d",
                    http_listen,
                    "/f",
                ],
            )?;
        }
    }

    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_required(program: &str, args: &[&str]) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {} {:?}", program, args))?;

    if !status.success() {
        bail!("{} {:?} exited with {}", program, args, status);
    }

    Ok(())
}

fn run_if_available(program: &str, args: &[&str]) -> anyhow::Result<bool> {
    let mut cmd = Command::new(program);
    cmd.args(args);

    let status = match cmd.status() {
        Ok(v) => v,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(anyhow::anyhow!("failed to run {} {:?}: {}", program, args, err)),
    };

    if !status.success() {
        return Err(anyhow::anyhow!("{} {:?} exited with {}", program, args, status));
    }

    Ok(true)
}
