//! Narrow the session bus address list before anything connects to D-Bus.
//!
//! `DBUS_SESSION_BUS_ADDRESS` may hold several `;`-separated addresses that a
//! client should try in order. zbus parses only one address, so a list makes
//! both the tray and the single-instance check fail to connect.

use std::path::Path;

const VAR: &str = "DBUS_SESSION_BUS_ADDRESS";

/// Replace an address list in `DBUS_SESSION_BUS_ADDRESS` with its first usable
/// entry and return that entry; `None` when the variable is left unchanged.
///
/// This sets an environment variable, so call it before any other thread starts.
pub fn select() -> Option<String> {
    let current = std::env::var(VAR).ok()?;
    let chosen = first_usable(&current, |path| path.exists())?;
    if chosen == current {
        return None;
    }
    std::env::set_var(VAR, &chosen);
    Some(chosen)
}

/// The first entry of `list` that may connect: a `unix:path=` entry counts only
/// when its socket exists, other transports are taken as they are. Falls back to
/// the first entry when none qualifies.
fn first_usable(list: &str, exists: impl Fn(&Path) -> bool) -> Option<String> {
    let entries: Vec<&str> = list
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    entries
        .iter()
        .find(|entry| unix_path(entry).is_none_or(|path| exists(Path::new(path))))
        .or(entries.first())
        .map(|entry| entry.to_string())
}

fn unix_path(address: &str) -> Option<&str> {
    address
        .strip_prefix("unix:")?
        .split(',')
        .find_map(|option| option.strip_prefix("path="))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(list: &str, live: &[&str]) -> Option<String> {
        first_usable(list, |path| live.iter().any(|l| Path::new(l) == path))
    }

    #[test]
    fn skips_a_missing_socket() {
        let list = "unix:path=/run/user/1000/bus;unix:path=/tmp/dbus-abc,guid=0123";
        assert_eq!(
            pick(list, &["/tmp/dbus-abc"]).as_deref(),
            Some("unix:path=/tmp/dbus-abc,guid=0123")
        );
    }

    #[test]
    fn keeps_the_first_live_entry() {
        let list = "unix:path=/a;unix:path=/b";
        assert_eq!(pick(list, &["/a", "/b"]).as_deref(), Some("unix:path=/a"));
    }

    #[test]
    fn takes_unchecked_transports_as_they_are() {
        let list = "unix:path=/gone;unix:abstract=/tmp/dbus-x;tcp:host=localhost,port=1";
        assert_eq!(
            pick(list, &[]).as_deref(),
            Some("unix:abstract=/tmp/dbus-x")
        );
    }

    #[test]
    fn falls_back_to_the_first_entry() {
        assert_eq!(
            pick("unix:path=/a;unix:path=/b", &[]).as_deref(),
            Some("unix:path=/a")
        );
        assert_eq!(pick("unix:path=/a", &[]).as_deref(), Some("unix:path=/a"));
        assert_eq!(pick(";", &[]), None);
    }
}
