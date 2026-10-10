//! Overview callbacks and snapshot sync.

use super::{logs, spawn_rules, update, update_with};
use crate::config::{PacRuleMode, SystemProxyMode};
use crate::ui::controller::{Notice as CtlNotice, OverviewSnapshot};
use crate::ui::{Actions, AppState, Connection, FieldText, MainWindow, Notice, PacRule, ProxyMode};
use crate::validate::RequiredField;
use slint::ComponentHandle;

fn proxy_mode_from(mode: ProxyMode) -> SystemProxyMode {
    match mode {
        ProxyMode::Off => SystemProxyMode::Disable,
        ProxyMode::Pac => SystemProxyMode::Pac,
        ProxyMode::Global => SystemProxyMode::Global,
    }
}

fn proxy_mode_to(mode: SystemProxyMode) -> ProxyMode {
    match mode {
        SystemProxyMode::Disable => ProxyMode::Off,
        SystemProxyMode::Pac => ProxyMode::Pac,
        SystemProxyMode::Global => ProxyMode::Global,
    }
}

fn pac_rule_from(rule: PacRule) -> PacRuleMode {
    match rule {
        PacRule::BypassChina => PacRuleMode::BypassChina,
        PacRule::GfwList => PacRuleMode::ProxyGfw,
    }
}

fn pac_rule_to(rule: PacRuleMode) -> PacRule {
    match rule {
        PacRuleMode::BypassChina => PacRule::BypassChina,
        PacRuleMode::ProxyGfw => PacRule::GfwList,
    }
}

pub fn wire(ui: &MainWindow) {
    let actions = ui.global::<Actions>();
    let weak = ui.as_weak();
    actions.on_navigate(move |page| {
        if let Some(ui) = weak.upgrade() {
            ui.global::<AppState>().set_page(page);
        }
        logs::update_refresh();
    });
    actions.on_toggle_connection(|| update(|c, now| c.toggle_connection(now)));
    actions.on_set_proxy_mode(|mode| update(|c, now| c.set_proxy_mode(proxy_mode_from(mode), now)));
    actions.on_set_pac_rule(|rule| update(|c, now| c.set_pac_rule(pac_rule_from(rule), now)));
    actions.on_update_rules(|| {
        if let Some(Some(job)) = update_with(|c, _| c.update_rules()) {
            spawn_rules(job);
        }
    });
    actions.on_copy_pac_url(|| update(|c, _| c.copy_pac_url()));
    actions.on_dismiss_notice(|| update(|c, _| c.dismiss_notice()));
}

pub fn sync(ui: &MainWindow, snapshot: &OverviewSnapshot) {
    let state = ui.global::<AppState>();
    state.set_connection(if snapshot.running {
        Connection::Running
    } else {
        Connection::Stopped
    });
    state.set_active_name(snapshot.active_name.as_str().into());
    state.set_active_address(snapshot.active_address.as_str().into());
    state.set_local_host(snapshot.local_host.as_str().into());
    state.set_local_port(snapshot.local_port.as_str().into());
    state.set_pac_url(snapshot.pac_url.as_str().into());
    state.set_proxy_mode(proxy_mode_to(snapshot.proxy_mode));
    state.set_pac_rule(pac_rule_to(snapshot.pac_rule));
    state.set_rules_age_days(snapshot.rules_age_days);
    state.set_rules_time(snapshot.rules_time.as_str().into());
    state.set_rules_updating(snapshot.rules_updating);
}

pub fn sync_notice(ui: &MainWindow, notice: &CtlNotice) {
    let (kind, detail) = match notice {
        CtlNotice::None => (Notice::None, String::new()),
        CtlNotice::Imported => (Notice::Imported, String::new()),
        CtlNotice::ImportFailed(d) => (Notice::ImportFailed, d.clone()),
        CtlNotice::ImportPartial(added, _) => (Notice::ImportPartial, added.to_string()),
        CtlNotice::ExportFailed(d) => (Notice::ExportFailed, d.clone()),
        CtlNotice::ExportInvalid => (Notice::ExportInvalid, String::new()),
        CtlNotice::LinkCopied => (Notice::LinkCopied, String::new()),
        CtlNotice::StartFailed(d) => (Notice::StartFailed, d.clone()),
        CtlNotice::RulesUpdated => (Notice::RulesUpdated, String::new()),
        CtlNotice::RulesFailed(d) => (Notice::RulesFailed, d.clone()),
        CtlNotice::ProxyFailed(d) => (Notice::ProxyFailed, d.clone()),
        CtlNotice::SaveFailed(d) => (Notice::SaveFailed, d.clone()),
        CtlNotice::CoreExited(d) => (Notice::CoreExited, d.clone()),
        CtlNotice::NoNode => (Notice::NoNode, String::new()),
        CtlNotice::MissingFields(fields) => (Notice::MissingFields, field_list(ui, fields)),
        CtlNotice::NodesSaveFailed(d) => (Notice::NodesSaveFailed, d.clone()),
        CtlNotice::SaveInvalid => (Notice::SaveInvalid, String::new()),
        CtlNotice::NodeIncomplete(name, _) => (Notice::NodeIncomplete, name.clone()),
        CtlNotice::ProfilesChanged => (Notice::ProfilesChanged, String::new()),
    };
    let state = ui.global::<AppState>();
    state.set_notice(kind);
    state.set_notice_detail(detail.into());
    let extra = match notice {
        CtlNotice::ImportPartial(_, skipped) => skipped.to_string(),
        CtlNotice::NodeIncomplete(_, fields) => field_list(ui, fields),
        _ => String::new(),
    };
    state.set_notice_extra(extra.into());
    state.set_notice_error(notice.is_error());
}

/// Translated field names joined with the locale's list separator.
fn field_list(ui: &MainWindow, fields: &[RequiredField]) -> String {
    let text = ui.global::<FieldText>();
    let names: Vec<String> = fields
        .iter()
        .map(|field| match field {
            RequiredField::Server => text.get_server(),
            RequiredField::Uuid => text.get_uuid(),
            RequiredField::Password => text.get_password(),
        })
        .map(String::from)
        .collect();
    names.join(text.get_separator().as_str())
}
