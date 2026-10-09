//! Backend-neutral profile validation.

use crate::config::{ProxyProfile, ProxyProtocol};

/// A mandatory profile field. Each frontend formats its own label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredField {
    Server,
    Uuid,
    Password,
}

/// Mandatory fields the profile is still missing, in display order.
pub fn missing_fields(profile: &ProxyProfile) -> Vec<RequiredField> {
    let mut missing = Vec::new();
    if profile.server.trim().is_empty() {
        missing.push(RequiredField::Server);
    }
    match profile.protocol {
        ProxyProtocol::Juicity => {
            if profile.uuid.trim().is_empty() {
                missing.push(RequiredField::Uuid);
            }
            if profile.password.is_empty() {
                missing.push(RequiredField::Password);
            }
        }
        ProxyProtocol::Shadowsocks => {
            if profile.password.is_empty() && !matches!(profile.method.as_str(), "none" | "plain") {
                missing.push(RequiredField::Password);
            }
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_reports_incomplete_juicity_profile() {
        let mut p = ProxyProfile::default();
        assert_eq!(
            missing_fields(&p),
            [
                RequiredField::Server,
                RequiredField::Uuid,
                RequiredField::Password
            ]
        );
        p.server = "example.com".into();
        p.uuid = "id".into();
        p.password = "pw".into();
        assert!(missing_fields(&p).is_empty());
    }

    #[test]
    fn missing_fields_allows_passwordless_ss_none() {
        let p = ProxyProfile {
            protocol: ProxyProtocol::Shadowsocks,
            server: "example.com".into(),
            method: "none".into(),
            ..Default::default()
        };
        assert!(missing_fields(&p).is_empty());
    }
}
