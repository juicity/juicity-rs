use crate::config::{AppConfig, ProxyProfile, ProxyProtocol};
use anyhow::Context;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Resolve the path of a proxy binary:
/// 1. If `override_path` is given and the file exists, use it.
/// 2. Try the directory that contains the running executable.
/// 3. Fall back to just `name` and let the OS $PATH resolve it.
fn find_binary(name: &str, override_path: Option<&PathBuf>) -> PathBuf {
    if let Some(path) = override_path {
        if path.exists() {
            return path.clone();
        }
    }
    // Directory that contains the running juicity-gui executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // Current working directory (useful when running from the project root during development).
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
}

#[derive(Debug)]
struct RunningCore {
    protocol: ProxyProtocol,
    child: Child,
    /// Temporary config file written for this run; deleted on stop.
    temp_config: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct CoreManager {
    running: Option<RunningCore>,
}

impl CoreManager {
    pub fn new() -> Self {
        Self { running: None }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn current_protocol(&self) -> Option<ProxyProtocol> {
        self.running.as_ref().map(|v| v.protocol)
    }

    pub fn start_profile(&mut self, config: &AppConfig, profile: &ProxyProfile) -> anyhow::Result<()> {
        self.stop()?;

        let (mut cmd, temp_config) = build_command(config, profile)?;
        tracing::info!("starting {:?} core for profile {}", profile.protocol, profile.name);

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {:?} core process", profile.protocol))?;

        self.running = Some(RunningCore {
            protocol: profile.protocol,
            child,
            temp_config,
        });
        Ok(())
    }

    pub fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut running) = self.running.take() {
            tracing::info!("stopping {:?} core", running.protocol);
            let _ = running.child.kill();
            let _ = running.child.wait();
            if let Some(path) = running.temp_config {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }

    pub fn poll(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        if let Some(running) = &mut self.running {
            if let Some(status) = running
                .child
                .try_wait()
                .with_context(|| format!("failed to poll {:?} process", running.protocol))?
            {
                // clean up temp config
                if let Some(path) = self.running.as_ref().and_then(|r| r.temp_config.as_ref()) {
                    let _ = std::fs::remove_file(path);
                }
                self.running = None;
                return Ok(Some(status));
            }
        }
        Ok(None)
    }
}

/// Returns the command and an optional temp config path that must be cleaned up.
fn build_command(config: &AppConfig, profile: &ProxyProfile) -> anyhow::Result<(Command, Option<PathBuf>)> {
    match profile.protocol {
        ProxyProtocol::Juicity => build_juicity_command(config, profile),
        ProxyProtocol::Shadowsocks => build_shadowsocks_command(config, profile),
    }
}

fn write_temp_config(prefix: &str, json: &str) -> anyhow::Result<PathBuf> {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("{}{}-config.json", prefix, pid));
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write temp config {}", path.display()))?;
    Ok(path)
}

fn build_juicity_command(config: &AppConfig, profile: &ProxyProfile) -> anyhow::Result<(Command, Option<PathBuf>)> {
    let binary = find_binary("juicity-client", config.juicity_client_path.as_ref());

    // Use legacy config_path if set; otherwise generate from individual fields.
    let (config_path, temp) = if let Some(path) = &profile.config_path {
        (path.clone(), None)
    } else {
        let sni = profile
            .sni
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&profile.server);
        let json = serde_json::json!({
            "listen": config.socks_listen,
            "server": format!("{}:{}", profile.server, profile.server_port),
            "uuid": profile.uuid,
            "password": profile.password,
            "sni": sni,
            "allow_insecure": profile.allow_insecure,
            "log_level": "info"
        });
        let path = write_temp_config("juicity-gui-", &json.to_string())?;
        (path.clone(), Some(path))
    };

    let mut cmd = Command::new(binary);
    cmd.arg("run")
        .arg("-c")
        .arg(&config_path)
        .arg("--log-level")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok((cmd, temp))
}

fn build_shadowsocks_command(config: &AppConfig, profile: &ProxyProfile) -> anyhow::Result<(Command, Option<PathBuf>)> {
    let binary = find_binary("sslocal", config.ss_local_path.as_ref());

    // Use legacy config_path if set; otherwise generate from individual fields.
    let (config_path, temp) = if let Some(path) = &profile.config_path {
        (path.clone(), None)
    } else {
        let local_port: u16 = config
            .socks_listen
            .rsplitn(2, ':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(1080);
        let local_addr = config
            .socks_listen
            .rsplitn(2, ':')
            .nth(1)
            .unwrap_or("127.0.0.1");

        let mut json = serde_json::json!({
            "server": profile.server,
            "server_port": profile.server_port,
            "password": profile.password,
            "method": profile.method,
            "local_address": local_addr,
            "local_port": local_port,
            "timeout": profile.timeout
        });
        if let Some(plugin) = &profile.plugin {
            json["plugin"] = serde_json::Value::String(plugin.clone());
        }
        if let Some(opts) = &profile.plugin_opts {
            json["plugin_opts"] = serde_json::Value::String(opts.clone());
        }
        if let Some(args) = &profile.plugin_args {
            json["plugin_args"] = serde_json::Value::String(args.clone());
        }
        let path = write_temp_config("juicity-gui-ss-", &json.to_string())?;
        (path.clone(), Some(path))
    };

    let mut cmd = Command::new(binary);
    cmd.arg("-c")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok((cmd, temp))
}
