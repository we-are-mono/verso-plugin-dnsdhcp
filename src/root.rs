// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

//! The plugin's root half (ADR-027): `verso-plugin-dnsdhcp root`, run by the
//! privileged helper as root for what its registry declares:
//!
//! - `dhcp-state`, a read: each network's DHCP server as dnsmasq loaded it;
//! - `dns-state`, a read: what the resolver can do and who answers each local
//!   port;
//! - `files`, the `dnsmasq` family's transform: a listing shows the files as
//!   they are, and a save is checked by dnsmasq itself before it is staged.

use crate::{dhcp_state, resolver};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use verso_plugin::root::{RootError, RootRequest};
use verso_plugin::{json, Map, Value};

/// The version of the `dns-state` read, raised only when its result changes
/// incompatibly (ADR-027 §4). `dhcp-state`'s is the SDK's
/// `dhcp::DHCP_STATE_VERSION`, since verso-plugin-interfaces reads it too.
pub const DNS_STATE_VERSION: u32 = 1;

/// The refusal of options dnsmasq would not start with.
pub const REJECTED: &str = "dnsmasq rejected these options. Check the option names and values.";

/// Where a draft is written for dnsmasq to check: root's alone.
const DRAFTS: &str = "/tmp/run/verso-dnsdhcp/drafts";

pub fn run() -> ! {
    verso_plugin::root::run(&[
        ("dhcp-state", &dhcp),
        ("dns-state", &dns),
        ("files", &files),
    ])
}

fn ubus(object: &str, method: &str, args: &str) -> Value {
    Command::new("/bin/ubus")
        .args(["call", object, method, args])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice(&o.stdout).ok())
        .unwrap_or(Value::Null)
}

fn dhcp(_: &RootRequest) -> Result<Map<String, Value>, RootError> {
    let config = verso_plugin::root::uci::committed("dhcp").map_err(RootError::failure)?;
    let services = ubus("service", "list", r#"{"name":"dnsmasq"}"#);
    let dump = ubus("network.interface", "dump", "{}");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let networks = dhcp_state::state(&config, &services, &dump, &dhcp_state::System, now);
    let mut out = Map::new();
    out.insert("networks".into(), Value::Object(networks));
    Ok(out)
}

fn dns(_: &RootRequest) -> Result<Map<String, Value>, RootError> {
    Ok(resolver::state())
}

fn files(request: &RootRequest) -> Result<Map<String, Value>, RootError> {
    match request.input["op"].as_str() {
        Some("list") => Ok(list(&request.input)),
        Some("stage") => stage(&request.input, check),
        _ => Err(RootError::failure(
            "the files transform takes op list or stage",
        )),
    }
}

/// list shows dnsmasq's option files as they are: they hold nothing to fold.
fn list(input: &Value) -> Map<String, Value> {
    let files: Vec<Value> = input["files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| json!({"path": f["path"], "content": f["content"].as_str().unwrap_or_default()}))
        .collect();
    let mut out = Map::new();
    out.insert("files".into(), Value::Array(files));
    out
}

/// stage is a save as dnsmasq itself would read it on its own: refused, in
/// words the editor shows, when dnsmasq would not start with it.
fn stage(
    input: &Value,
    check: impl Fn(&str) -> Result<bool, String>,
) -> Result<Map<String, Value>, RootError> {
    let edited = input["edited"].as_str().unwrap_or_default();
    if !check(edited).map_err(RootError::failure)? {
        return Err(RootError::refusal(REJECTED));
    }
    let mut out = Map::new();
    out.insert("content".into(), json!(edited));
    Ok(out)
}

/// check is whether `dnsmasq --test` accepts body as a file of options.
fn check(body: &str) -> Result<bool, String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(DRAFTS)
        .map_err(|e| e.to_string())?;
    let draft = verso_plugin::root::write_new(
        &Path::new(DRAFTS).join("draft.conf"),
        body.as_bytes(),
        0o600,
    )?;
    let result = Command::new("/usr/sbin/dnsmasq")
        .arg("--test")
        .arg(format!("--conf-file={}", draft.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&draft);
    result.map(|s| s.success()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use verso_plugin::Registry;

    #[test]
    fn a_listing_shows_the_files_as_they_are() {
        let out = list(
            &json!({"op": "list", "files": [{"path": "/etc/dnsmasq.conf", "content": "log-queries\n", "pending": false}]}),
        );
        assert_eq!(
            out["files"],
            json!([{"path": "/etc/dnsmasq.conf", "content": "log-queries\n"}])
        );
    }

    #[test]
    fn a_save_dnsmasq_refuses_is_refused_in_the_editors_words() {
        let input =
            json!({"op": "stage", "path": "/etc/dnsmasq.d/x.conf", "edited": "log-querys\n"});
        assert_eq!(
            stage(&input, |_| Ok(false)).unwrap_err(),
            RootError::refusal(REJECTED)
        );
        let input =
            json!({"op": "stage", "path": "/etc/dnsmasq.d/x.conf", "edited": "log-queries\n"});
        assert_eq!(
            stage(&input, |_| Ok(true)).unwrap()["content"],
            "log-queries\n"
        );
        assert!(matches!(
            stage(&input, |_| Err("no dnsmasq".into())),
            Err(RootError::Failure(_))
        ));
    }

    /// The registry the package installs registers what this half answers,
    /// and declares the dnsmasq family it owns.
    #[test]
    fn the_registry_names_what_the_root_half_answers() {
        let text = std::fs::read_to_string("rootfs/usr/share/verso/plugins/dnsdhcp/commands.json")
            .unwrap();
        let registry: Registry = serde_json::from_str(&text).unwrap();
        registry.check().unwrap();
        let names: Vec<&str> = registry.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dhcp-state", "dns-state", "files"]);
        assert_eq!(
            registry.commands[0].version,
            Some(verso_plugin::dhcp::DHCP_STATE_VERSION)
        );
        assert_eq!(registry.commands[1].version, Some(DNS_STATE_VERSION));
        let family = &registry.families[0];
        assert_eq!(
            (family.name.as_str(), family.config.as_str()),
            ("dnsmasq", "dhcp")
        );
        assert_eq!(family.files, ["/etc/dnsmasq.conf"]);
        assert_eq!(family.transform.as_deref(), Some("files"));
    }
}
