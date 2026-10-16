// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

//! DNS and DHCP: two pages over one config file, one daemon and one ACL, plus
//! the reservation tab of the shell's device panel. Reads and mutations are
//! brokered by the shell; the plugin has no privileges.

use verso_plugin::{serve_described, Envelope, Form, Request};

mod describe;
mod dhcp;
mod dns;
mod files;
mod form;
mod format;
mod hosts;
mod leases;
mod live;
mod model;
mod page;
mod records;
mod upstreams;

#[cfg(test)]
mod fixture;

use live::Leases;
use model::Dnsdhcp;

fn main() {
    serve_described("dnsdhcp", get, post, describe::describe);
}

fn get(request: &Request) -> Envelope {
    let model = Dnsdhcp::read(&request.snapshot);
    match Route::of(&request.path) {
        Route::NotFound => Envelope::not_found(),
        Route::Dhcp => dhcp::get(request),
        Route::Dns => dns::page(request),
        Route::Files => files::get(request),
        Route::NewRecord => records::blank(),
        Route::EditRecord(section) => {
            records::edit(&model, &section).unwrap_or_else(|| records::missing(request))
        }
        Route::NewServer => upstreams::blank(),
        Route::EditServer(index) => {
            upstreams::edit(&model, &index).unwrap_or_else(|| upstreams::missing(request))
        }
        Route::EntityDevice(mac) => {
            hosts::entity_tab(&model, &Leases::read(&request.ubus), &mac)
        }
    }
}

fn post(request: &Request, form: &Form) -> Envelope {
    let mut model = Dnsdhcp::read(&request.snapshot);
    match Route::of(&request.path) {
        Route::NotFound => Envelope::not_found(),
        Route::Dhcp => dhcp::post(request, form),
        Route::Dns => dns::post(request, form),
        Route::Files => files::post(request, form),
        Route::NewRecord => records::create(request, &mut model, form),
        Route::EditRecord(section) => records::save(request, &mut model, &section, form)
            .unwrap_or_else(|| records::missing(request)),
        Route::NewServer => upstreams::create(request, &mut model, form),
        Route::EditServer(index) => upstreams::save(request, &mut model, &index, form)
            .unwrap_or_else(|| upstreams::missing(request)),
        Route::EntityDevice(mac) => {
            hosts::entity_save(&model, &Leases::read(&request.ubus), &mac, form)
        }
    }
}

/// Route is what a request's sub-path asks for: one of the two pages, a DNS
/// object's own page, a custom options file, or the device panel's tab. An
/// address this plugin had before (`config`, `leases`, `dhcp`) answers with the
/// DHCP page, so a stale link lands somewhere real; an editor's own root
/// (`dns/records` with no section) is the DNS page it belongs to; any other
/// path has nothing at it.
enum Route {
    Dhcp,
    Dns,
    Files,
    NewRecord,
    EditRecord(String),
    NewServer,
    EditServer(String),
    /// This plugin's say about one device, for the shell's device panel. It
    /// answers with a tab, not a page: the panel around it is the shell's, and
    /// the other tabs in it belong to plugins this one knows nothing about.
    EntityDevice(String),
    /// A path that was never an address here.
    NotFound,
}

impl Route {
    fn of(path: &str) -> Route {
        let path = path.trim_matches('/');
        if path.starts_with(files::PREFIX) {
            return Route::Files;
        }
        if let Some(rest) = sub_path(path, page::RECORDS) {
            return match rest {
                "" => Route::Dns,
                page::NEW => Route::NewRecord,
                section => Route::EditRecord(section.to_string()),
            };
        }
        if let Some(rest) = sub_path(path, page::SERVERS) {
            return match rest {
                "" => Route::Dns,
                page::NEW => Route::NewServer,
                index => Route::EditServer(index.to_string()),
            };
        }
        if let Some(mac) = path.strip_prefix("entity/device/") {
            return Route::EntityDevice(mac.to_string());
        }
        // The plugin's own address is DHCP, and so is every address it had
        // before the two pages settled: a stale link lands somewhere real. A
        // path that was never an address here has nothing at it.
        let first = path.split('/').next().unwrap_or_default();
        match (path, first) {
            (page::DNS, _) => Route::Dns,
            ("", _) | (_, "config" | "leases" | "dhcp") => Route::Dhcp,
            _ => Route::NotFound,
        }
    }
}

/// sub_path returns the tail below an editor's prefix, or None when the path is
/// not under it. The prefix ends at a slash or at the end of the path, so `dns`
/// never matches the `dns/records` prefix and a section is never glued onto it.
fn sub_path<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    match rest.strip_prefix('/') {
        Some(section) => Some(section),
        None if rest.is_empty() => Some(""),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use verso_plugin::{Snapshot, Ubus};

    fn read(path: &str) -> Value {
        serde_json::to_value(get(&fixture::request(path))).expect("serialize")
    }

    #[test]
    fn the_two_pages_have_two_addresses_and_old_ones_land_on_dhcp() {
        assert_eq!(read("/")["title"], "DHCP");
        assert_eq!(read("/dns")["title"], "DNS");
        for stale in ["/config", "/leases", "/dhcp", "/config/reservations/new"] {
            assert_eq!(read(stale)["title"], "DHCP", "{stale}");
            assert!(read(stale).get("not_found").is_none(), "{stale}");
        }
    }

    // A path that was never an address here has nothing at it, and says so
    // rather than passing for the DHCP page.
    #[test]
    fn a_path_that_names_nothing_has_nothing_at_it() {
        for path in ["/nowhere", "/dnsx", "/dhcp-servers"] {
            assert_eq!(read(path)["not_found"], true, "{path}");
        }
    }

    #[test]
    fn each_dns_editor_keeps_its_own_page_and_its_root_is_the_dns_page() {
        for (path, title) in [
            ("/dns/records/new", "New record"),
            ("/dns/records/domain_backup_v4", "Edit record"),
            ("/dns/servers/new", "New server"),
            ("/dns/servers/0", "Edit server"),
        ] {
            let body = read(path);
            assert_eq!(body["title"], title, "{path}");
            assert_eq!(body["width"], "form", "{path}");
        }
        assert_eq!(read("/dns/records")["title"], "DNS");
        assert_eq!(read("/dns/servers")["title"], "DNS");
        // A stale editor address lands on the DNS page and says why.
        let stale = read("/dns/records/no_such_record");
        assert_eq!(stale["title"], "DNS");
        assert_eq!(stale["notice"]["level"], "danger");
    }

    #[test]
    fn the_device_panel_asks_for_its_tab() {
        assert_eq!(read("/entity/device/30:9C:23:5E:88:01")["title"], "Reserved address");
    }

    #[test]
    fn a_read_without_brokered_leases_still_renders_both_pages() {
        for path in ["/", "/dns"] {
            let mut request = fixture::request(path);
            request.ubus = Ubus::from_value(Value::Null);
            let body = serde_json::to_value(get(&request)).expect("serialize");
            assert_eq!(body["schema_version"], 1, "{path}");
        }
    }

    #[test]
    fn an_empty_config_still_answers_both_pages() {
        for path in ["/", "/dns"] {
            let mut request = fixture::request(path);
            request.snapshot =
                Snapshot::from_value(serde_json::json!({"dhcp": {}, "network": {}}));
            assert_eq!(get(&request).schema_version, 1, "{path}");
        }
    }
}
