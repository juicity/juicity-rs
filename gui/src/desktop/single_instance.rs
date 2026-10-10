//! One GUI process per session.
//!
//! Linux owns the D-Bus name `io.juicity.gui` and serves
//! `org.freedesktop.Application.Activate`; Windows holds a named mutex and
//! listens on a named pipe. A second launch asks the first one to show its
//! window and exits. macOS relies on LaunchServices.

/// Callback run (on a background thread) when another launch asks this
/// instance to show its window.
pub type OnActivate = Box<dyn Fn() + Send + Sync>;

/// D-Bus well-known name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const NAME: &str = "io.juicity.gui";
/// Message a second Windows instance writes to the pipe.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const ACTIVATE_MESSAGE: &[u8] = b"activate";

/// Result of asking for the instance lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Acquired,
    /// Another process holds it.
    #[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
    Taken,
    /// The lock cannot be checked (e.g. no session bus).
    Unavailable,
}

/// What this launch does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run as the only instance.
    Primary,
    /// Activate the running instance and exit.
    ActivateExisting,
    /// Run without single-instance protection.
    Unguarded,
}

pub fn decide(claim: Claim) -> Decision {
    match claim {
        Claim::Acquired => Decision::Primary,
        Claim::Taken => Decision::ActivateExisting,
        Claim::Unavailable => Decision::Unguarded,
    }
}

/// Whether a pipe message asks for activation.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn is_activate(message: &[u8]) -> bool {
    message.trim_ascii() == ACTIVATE_MESSAGE
}

/// Activation requests that arrive before the window exists wait for it.
#[derive(Debug, Default)]
pub struct Activation {
    ready: bool,
    pending: bool,
}

impl Activation {
    /// Record a request; true when the window can be shown now.
    pub fn request(&mut self) -> bool {
        if !self.ready {
            self.pending = true;
        }
        self.ready
    }

    /// The window exists; true when a queued request must be shown now.
    pub fn window_ready(&mut self) -> bool {
        self.ready = true;
        std::mem::take(&mut self.pending)
    }
}

/// The lock held by the first instance; dropping it releases the name.
pub struct Instance {
    #[cfg(target_os = "linux")]
    _bus: Option<zbus::blocking::Connection>,
    #[cfg(target_os = "windows")]
    _mutex: Option<windows::Mutex>,
}

pub enum Startup {
    Primary(Instance),
    /// The running instance was asked to show its window; exit now.
    Secondary,
}

/// Claim the instance lock before any side effect. A second launch
/// activates the first one and returns [`Startup::Secondary`].
pub fn acquire(on_activate: OnActivate) -> Startup {
    #[cfg(target_os = "linux")]
    let (claim, guard) = linux::claim(on_activate);
    #[cfg(target_os = "windows")]
    let (claim, guard) = windows::claim(on_activate);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let (claim, guard) = {
        drop(on_activate);
        (Ok::<Claim, String>(Claim::Acquired), ())
    };

    let reason = claim.as_ref().err().cloned();
    let claim = claim.unwrap_or(Claim::Unavailable);
    match decide(claim) {
        Decision::ActivateExisting => {
            #[cfg(target_os = "linux")]
            let activated = guard.as_ref().map_or(Ok(()), linux::activate_existing);
            #[cfg(target_os = "windows")]
            let activated = {
                drop(guard);
                windows::activate_existing()
            };
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            let activated: anyhow::Result<()> = Ok(());
            match activated {
                Ok(()) => tracing::info!("juicity is already running; showing its window"),
                Err(err) => {
                    tracing::warn!("juicity is already running but did not answer: {err:#}")
                }
            }
            Startup::Secondary
        }
        decision => {
            if decision == Decision::Unguarded {
                tracing::warn!(
                    "single-instance check unavailable, running without it: {}",
                    reason.unwrap_or_default()
                );
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            let _ = guard;
            Startup::Primary(Instance {
                #[cfg(target_os = "linux")]
                _bus: guard,
                #[cfg(target_os = "windows")]
                _mutex: guard,
            })
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Claim, OnActivate, NAME};
    use std::collections::HashMap;
    use zbus::blocking::Connection;
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    use zbus::zvariant::{OwnedValue, Value};

    const PATH: &str = "/io/juicity/gui";
    const INTERFACE: &str = "org.freedesktop.Application";

    struct Application {
        on_activate: OnActivate,
    }

    #[zbus::interface(name = "org.freedesktop.Application")]
    impl Application {
        fn activate(&self, _platform_data: HashMap<String, OwnedValue>) {
            (self.on_activate)();
        }
    }

    pub fn claim(on_activate: OnActivate) -> (Result<Claim, String>, Option<Connection>) {
        let connection = zbus::blocking::connection::Builder::session()
            .and_then(|builder| builder.serve_at(PATH, Application { on_activate }))
            .and_then(|builder| builder.build());
        let connection = match connection {
            Ok(connection) => connection,
            Err(err) => return (Err(format!("no D-Bus session bus: {err}")), None),
        };
        let claim =
            match connection.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into()) {
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
                    Ok(Claim::Acquired)
                }
                Ok(RequestNameReply::Exists | RequestNameReply::InQueue) => Ok(Claim::Taken),
                // zbus reports `Exists` as this error.
                Err(zbus::Error::NameTaken) => Ok(Claim::Taken),
                Err(err) => Err(format!("could not request {NAME}: {err}")),
            };
        (claim, Some(connection))
    }

    pub fn activate_existing(connection: &Connection) -> anyhow::Result<()> {
        let platform_data: HashMap<&str, Value> = HashMap::new();
        connection.call_method(
            Some(NAME),
            PATH,
            Some(INTERFACE),
            "Activate",
            &(platform_data,),
        )?;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{is_activate, Claim, OnActivate, ACTIVATE_MESSAGE};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    const MUTEX_NAME: &str = "Local\\io.juicity.gui";

    /// Pipe names are machine-wide, so scope ours to this session like the
    /// `Local\` mutex; otherwise another user's instance could own it.
    fn pipe_name() -> String {
        let mut session = 0u32;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
        format!("\\\\.\\pipe\\io.juicity.gui-{session}")
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Owned mutex handle, closed on drop.
    pub struct Mutex(HANDLE);

    impl Drop for Mutex {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn claim(on_activate: OnActivate) -> (Result<Claim, String>, Option<Mutex>) {
        let name = wide(MUTEX_NAME);
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if handle.is_null() {
            let err = std::io::Error::last_os_error();
            return (Err(format!("could not create {MUTEX_NAME}: {err}")), None);
        }
        let mutex = Mutex(handle);
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return (Ok(Claim::Taken), None);
        }
        let spawned = std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || serve(on_activate));
        if let Err(err) = spawned {
            tracing::warn!("single-instance listener failed to start: {err}");
        }
        (Ok(Claim::Acquired), Some(mutex))
    }

    /// Pipe handle moved to the thread that reads one client.
    struct Client(HANDLE);

    // SAFETY: a pipe handle may be used from any thread.
    unsafe impl Send for Client {}

    impl Drop for Client {
        fn drop(&mut self) {
            unsafe {
                DisconnectNamedPipe(self.0);
                CloseHandle(self.0);
            }
        }
    }

    /// Accept clients and activate on "activate". Each client is read on
    /// its own thread so one that never writes cannot block later launches.
    fn serve(on_activate: OnActivate) {
        let on_activate = std::sync::Arc::new(on_activate);
        let name = wide(&pipe_name());
        let mut backoff = std::time::Duration::from_millis(100);
        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    0,
                    64,
                    0,
                    null(),
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                let err = std::io::Error::last_os_error();
                tracing::warn!("single-instance pipe failed, retrying: {err}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
            backoff = std::time::Duration::from_millis(100);
            let client = Client(pipe);
            let connected = unsafe { ConnectNamedPipe(pipe, null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                continue;
            }
            let on_activate = on_activate.clone();
            let spawned = std::thread::Builder::new()
                .name("single-instance-client".into())
                .spawn(move || {
                    // Move the whole guard in, not just its non-Send handle.
                    let client = client;
                    let mut buffer = [0u8; 64];
                    let mut read = 0u32;
                    let ok = unsafe {
                        ReadFile(
                            client.0,
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                            &mut read,
                            null_mut(),
                        )
                    } != 0;
                    drop(client);
                    if ok && is_activate(&buffer[..read as usize]) {
                        on_activate();
                    }
                });
            if let Err(err) = spawned {
                tracing::warn!("single-instance client thread failed: {err}");
            }
        }
    }

    pub fn activate_existing() -> anyhow::Result<()> {
        // Let the running instance bring its window to the foreground.
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        let pipe_name = pipe_name();
        let name = wide(&pipe_name);
        for _ in 0..20 {
            let pipe = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if pipe != INVALID_HANDLE_VALUE {
                let mut written = 0u32;
                let ok = unsafe {
                    WriteFile(
                        pipe,
                        ACTIVATE_MESSAGE.as_ptr(),
                        ACTIVATE_MESSAGE.len() as u32,
                        &mut written,
                        null_mut(),
                    )
                } != 0;
                let err = std::io::Error::last_os_error();
                unsafe { CloseHandle(pipe) };
                return if ok { Ok(()) } else { Err(err.into()) };
            }
            if unsafe { GetLastError() } == ERROR_PIPE_BUSY {
                unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
            } else {
                // The first instance may still be creating its pipe.
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        anyhow::bail!("no answer on {pipe_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_decides_the_role() {
        assert_eq!(decide(Claim::Acquired), Decision::Primary);
        assert_eq!(decide(Claim::Taken), Decision::ActivateExisting);
        assert_eq!(decide(Claim::Unavailable), Decision::Unguarded);
    }

    #[test]
    fn only_the_activate_message_activates() {
        assert!(is_activate(b"activate"));
        assert!(is_activate(b"activate\n"));
        assert!(!is_activate(b""));
        assert!(!is_activate(b"quit"));
    }

    #[test]
    fn activation_waits_for_the_window() {
        let mut activation = Activation::default();
        assert!(!activation.request());
        assert!(activation.window_ready(), "the queued request is delivered");
        assert!(!activation.window_ready(), "and delivered once");
        assert!(activation.request());
        let mut idle = Activation::default();
        assert!(!idle.window_ready());
    }
}
