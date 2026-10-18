// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.
//! dnsmasq's option files, listed and edited as every set of hand-edited files
//! is (verso_plugin::files): the page names where they live and what they are
//! called, the SDK draws them.
use verso_plugin::files::{self, FileSet};
use verso_plugin::{CommitOp, Envelope, Form, Grid, Request, Tone, Value, Widget};

const ROOT: &str = "/plugins/dnsdhcp/dns";

/// PREFIX is the sub-path a file's editor lives under: on the DNS page, whose
/// custom options they are.
pub const PREFIX: &str = "dns/files/";

const SET: FileSet = FileSet {
    family: "dnsmasq",
    page: ROOT,
    editors: "/plugins/dnsdhcp/dns/files/",
    dir: "/etc/dnsmasq.d/",
    suffix: ".conf",
    main: Some("/etc/dnsmasq.conf"),
    line: ("option", "options"),
    empty: "No custom option files",
    too_large: "Custom option files must be at most 32 KiB of text.",
    placeholder: "10-local",
};

/// FOLDER is where dnsmasq reads the custom option files from: the folder a
/// file saved here lands in, which the resolver's `confdir` must name.
const FOLDER: &str = "/etc/dnsmasq.d";

/// listed is dnsmasq's family's files, as the shell hands this plugin the
/// families of the configs it reads (ADR-027).
fn listed(r: &Request) -> Vec<Value> {
    files::files(r.ubus.get("configFiles"))
}

/// folder_ops is what saving a file in FOLDER also stages: dnsmasq's jail
/// mounts only its configured include folder, so each resolver that names
/// none is told this one. A resolver that reads another folder, or none at
/// all, refuses the save in the words returned.
fn folder_ops(r: &Request) -> Result<Vec<CommitOp>, &'static str> {
    let resolvers = r.snapshot.sections_of_type("dhcp", "dnsmasq");
    if resolvers.is_empty() {
        return Err("The router's resolver settings are not available, so nothing was saved.");
    }
    let mut ops = Vec::new();
    for resolver in resolvers {
        match resolver.scalar("confdir").as_str() {
            "" => ops.push(CommitOp {
                config: "dhcp".into(),
                section: resolver.name(),
                section_type: String::new(),
                delete: false,
                values: verso_plugin::json!({"confdir": FOLDER}),
            }),
            FOLDER => {}
            _ => return Err(
                "The resolver reads its custom options from another folder, so nothing was saved.",
            ),
        }
    }
    Ok(ops)
}

/// name is the file an editor's address names, after its prefix.
fn name(r: &Request) -> Option<&str> {
    r.path.trim_matches('/').strip_prefix(PREFIX)
}

pub fn listing(r: &Request) -> Widget {
    SET.listing(&listed(r))
}

/// with_editor is the DNS page with a file's editor open over it.
fn with_editor(r: &Request, editor: Widget) -> Envelope {
    let mut result = crate::dns::page(r);
    if let Widget::Grid(Grid { children, .. }) = &mut result.widget {
        children.push(editor);
    }
    result.with_back("Cancel", ROOT)
}

pub fn get(r: &Request) -> Envelope {
    let Some(name) = name(r) else {
        return crate::dns::page(r);
    };
    match SET.open(&listed(r), name) {
        Ok(editor) => with_editor(r, editor),
        Err(why) => crate::dns::page(r).with_notice(Tone::Danger, &why),
    }
}

pub fn post(r: &Request, f: &Form) -> Envelope {
    let Some((editor, command)) = name(r).and_then(|name| SET.save(&listed(r), name, f)) else {
        return crate::dns::page(r);
    };
    let in_folder = command
        .as_ref()
        .and_then(|c| c.args.get("path"))
        .is_some_and(|p| p.starts_with(&format!("{FOLDER}/")));
    let mut result = with_editor(r, editor);
    if in_folder {
        match folder_ops(r) {
            Ok(ops) => result.commit = ops,
            Err(why) => return result.with_notice(Tone::Danger, why),
        }
    }
    result.commands.extend(command);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    // With no files, the band still stands — its name, its count, its add —
    // and the table says it has nothing under it in its own words, rather than
    // a made-up row in the path column.
    #[test]
    fn with_no_files_the_band_stands_alone_and_the_table_says_so() {
        let r = Request {
            path: "/".into(),
            query: Form::default(),
            snapshot: fixture::snapshot(),
            ubus: fixture::ubus(),
            plugins: verso_plugin::Plugins::default(),
        };
        let body = serde_json::to_value(listing(&r)).expect("serialize");
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["group"]["label"], "Files");
        // A lane never counts itself; the table says there is nothing in it.
        assert!(rows[0]["group"].get("chain").is_none());
        assert!(rows[0]["cells"].as_array().is_none_or(Vec::is_empty));
        assert_eq!(body["empty_text"], "No custom option files");
    }

    fn saving(path: &str, form: &str, dnsmasq: Value) -> Value {
        let r = Request {
            path: path.into(),
            query: Form::default(),
            snapshot: verso_plugin::Snapshot::from_value(verso_plugin::json!({"dhcp": dnsmasq})),
            ubus: verso_plugin::Ubus::from_value(
                verso_plugin::json!({"configFiles": {"files": []}}),
            ),
            plugins: verso_plugin::Plugins::default(),
        };
        serde_json::to_value(post(&r, &Form::parse(form))).unwrap()
    }

    const NEW_FILE: &str = "filename=10-local&content=log-queries&expected=af63bd4c8601b7df";

    /// dnsmasq's jail mounts only the folder its resolver names: a file saved
    /// in /etc/dnsmasq.d stages that folder on each resolver that names none,
    /// beside the file, and the shell stages the file first.
    #[test]
    fn a_file_in_the_folder_stages_the_folder_on_the_resolver() {
        let body = saving(
            "/dns/files/new",
            NEW_FILE,
            verso_plugin::json!({"main": {".type": "dnsmasq", ".name": "main"}}),
        );
        assert_eq!(body["commands"][0]["name"], "config-file-stage");
        assert_eq!(
            body["commands"][0]["args"]["path"],
            "/etc/dnsmasq.d/10-local.conf"
        );
        assert_eq!(
            body["commit"],
            verso_plugin::json!([{"config": "dhcp", "section": "main", "values": {"confdir": "/etc/dnsmasq.d"}}])
        );

        let named = saving(
            "/dns/files/new",
            NEW_FILE,
            verso_plugin::json!({"main": {".type": "dnsmasq", ".name": "main", "confdir": "/etc/dnsmasq.d"}}),
        );
        assert!(
            named
                .get("commit")
                .is_none_or(|c| c.as_array().is_none_or(Vec::is_empty)),
            "{named}"
        );
    }

    #[test]
    fn a_resolver_reading_another_folder_refuses_the_save() {
        for dnsmasq in [
            verso_plugin::json!({"main": {".type": "dnsmasq", ".name": "main", "confdir": "/tmp/dnsmasq.d"}}),
            verso_plugin::json!({}),
        ] {
            let body = saving("/dns/files/new", NEW_FILE, dnsmasq);
            assert_eq!(body["notice"]["level"], "danger", "{body}");
            assert!(
                body.get("commands")
                    .is_none_or(|c| c.as_array().is_none_or(Vec::is_empty)),
                "nothing staged: {body}"
            );
        }
    }
}
