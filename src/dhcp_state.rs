// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

//! The DHCPv4 servers dnsmasq actually loaded, for each network a `config dhcp`
//! section names: whether one serves it, its pool, lease time and live leases.
//! The operator's staged config is deliberately not a source of live state:
//! the committed dhcp config, procd's dnsmasq instances, netifd's interfaces,
//! dnsmasq's generated configuration with its includes, and the lease files.
//!
//! This is the plugin's root half (ADR-027), run as root by the privileged
//! helper for the `dhcp-state` read: dnsmasq's configuration and leases are
//! root's to read. Only the lines that decide DHCP scope are kept from a config
//! file; host reservations, options and DNS records never leave.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};

/// The directives that decide what a dnsmasq instance serves and where else
/// its configuration lives; nothing else in a config file is read.
const SCOPE_KEYS: [&str; 8] = [
    "dhcp-range",
    "dhcp-leasefile",
    "interface",
    "no-dhcp-interface",
    "except-interface",
    "conf-file",
    "conf-dir",
    "conf-script",
];

/// How much of one config file is scanned for those lines: far past any
/// blocklist a package drops beside the configuration (adblock's runs to tens
/// of megabytes), short of reading without end.
const SCAN_LIMIT: u64 = 256 << 20;
/// The most of a lease file, and of everything the includes add up to.
const READ_LIMIT: usize = 1 << 20;

/// What a network's DHCP server is doing.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NetworkState {
    pub enabled: bool,
    pub state: String,
    pub reason: String,
    pub pool: String,
    pub lease_time: String,
    pub leases: Option<usize>,
}

impl NetworkState {
    fn value(&self) -> Value {
        let mut v = Map::new();
        v.insert("enabled".into(), json!(self.enabled));
        v.insert("state".into(), json!(self.state));
        for (key, text) in [
            ("reason", &self.reason),
            ("pool", &self.pool),
            ("lease_time", &self.lease_time),
        ] {
            if !text.is_empty() {
                v.insert(key.into(), json!(text));
            }
        }
        if let Some(leases) = self.leases {
            v.insert("leases".into(), json!(leases));
        }
        Value::Object(v)
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Range {
    tag: String,
    first: String,
    last: String,
    mask: String,
    duration: String,
}

#[derive(Debug, Default, Clone)]
struct Instance {
    id: String,
    running: bool,
    failed: bool,
    readable: bool,
    ranges: Vec<Range>,
    excluded: Vec<String>,
    interfaces: Vec<String>,
    lease_file: String,
}

/// Reader is where the state reads files from: the router itself, or a test's
/// table.
pub trait Reader {
    /// config is a config file's DHCP scope lines.
    fn config(&self, path: &Path) -> Result<String, String>;
    /// leases is a lease file, whole.
    fn leases(&self, path: &Path) -> Result<String, String>;
    /// list is a directory's file names (not its directories).
    fn list(&self, path: &Path) -> Result<Vec<String>, String>;
}

/// Where each config file's scope lines are kept until the file changes, so a
/// blocklist many megabytes long is scanned once rather than at every look:
/// RAM, root's alone, gone at reboot.
const SCANS: &str = "/tmp/run/verso-dnsdhcp/scans";

/// System reads the router's own files.
pub struct System;

impl Reader for System {
    fn config(&self, path: &Path) -> Result<String, String> {
        let file = regular(path, SCAN_LIMIT)?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        let stamp = format!("{} {:?}\n", meta.len(), meta.modified().ok());
        let kept = Path::new(SCANS).join(scan_name(path));
        if let Some(body) = fs::read_to_string(&kept)
            .ok()
            .and_then(|k| k.strip_prefix(&stamp).map(String::from))
        {
            return Ok(body);
        }
        let body = scan(file)?;
        keep(&kept, &format!("{stamp}{body}"));
        Ok(body)
    }

    fn leases(&self, path: &Path) -> Result<String, String> {
        let file = regular(path, READ_LIMIT as u64)?;
        let mut body = String::new();
        file.take(READ_LIMIT as u64)
            .read_to_string(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(body)
    }

    fn list(&self, path: &Path) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(names)
    }
}

/// scan_name is the name a config file's scan is kept under: its path, in hex.
fn scan_name(path: &Path) -> String {
    path.to_string_lossy()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// keep writes a scan beside its kept name and renames it into place; a scan
/// that cannot be kept is scanned again next time.
fn keep(kept: &Path, text: &str) {
    use std::os::unix::fs::DirBuilderExt;
    let Some(dir) = kept.parent() else {
        return;
    };
    if fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .is_err()
    {
        return;
    }
    if let Ok(tmp) = verso_plugin::root::write_new(kept, text.as_bytes(), 0o600) {
        if fs::rename(&tmp, kept).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// scan is a config file's DHCP scope lines, read line by line so a file of
/// any size up to the scan limit costs no more memory than its longest line.
fn scan(file: fs::File) -> Result<String, String> {
    let mut body = String::new();
    let mut reader = BufReader::new(file.take(SCAN_LIMIT));
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches('\n');
        if let Some((key, _)) = text.trim().split_once('=') {
            if SCOPE_KEYS.contains(&key) {
                body.push_str(text);
                body.push('\n');
            }
        }
    }
    Ok(body)
}

fn regular(path: &Path, limit: u64) -> Result<fs::File, String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > limit {
        return Err("DHCP file is not a bounded regular file".into());
    }
    Ok(file)
}

/// state is every network's DHCP state, keyed by its interface's name.
pub fn state(
    config: &Value,
    services: &Value,
    dump: &Value,
    reader: &impl Reader,
    now: i64,
) -> Map<String, Value> {
    let instances = instances(services, reader);
    let mut networks: BTreeMap<String, &Value> = BTreeMap::new();
    for v in dump["interface"].as_array().into_iter().flatten() {
        networks.insert(text(&v["interface"]), v);
    }
    let mut out = Map::new();
    for section in config.as_object().into_iter().flat_map(|c| c.values()) {
        if section[".type"] != "dhcp" {
            continue;
        }
        let name = text(&section["interface"]);
        if name.is_empty() {
            continue;
        }
        let live = networks.get(&name).copied();
        let tag = match text(&section["networkid"]) {
            t if t.is_empty() => name.clone(),
            t => t,
        };
        let enabled = section["ignore"] != "1" && section["dhcpv4"] != "disabled";
        let selected = text(&section["instance"]);
        let device = live
            .map(|l| match text(&l["l3_device"]) {
                d if d.is_empty() => text(&l["device"]),
                d => d,
            })
            .unwrap_or_default();
        let serving = serving(&instances, &selected, &tag, &device);
        let mut network = serving.judge(enabled, &selected, live, &instances);
        if !serving.matched.is_empty() {
            serving.pool(&mut network, live, reader, now);
        }
        out.insert(name, network.value());
    }
    out
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// truthy reads a ubus boolean, which may arrive as a boolean or a number.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

/// failed is whether procd stopped restarting an instance that is not running.
fn failed(instance: &Value) -> bool {
    if truthy(&instance["running"]) {
        return false;
    }
    if truthy(&instance["respawn_failed"]) {
        return true;
    }
    instance.get("respawn").is_none() && instance["exit_code"].as_f64().unwrap_or(0.0) != 0.0
}

fn instances(services: &Value, reader: &impl Reader) -> Vec<Instance> {
    let mut out = Vec::new();
    for (id, v) in services["dnsmasq"]["instances"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let mut instance = Instance {
            id: id.clone(),
            running: truthy(&v["running"]),
            failed: failed(v),
            lease_file: "/tmp/dhcp.leases".into(),
            ..Instance::default()
        };
        let args: Vec<String> = v["command"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        let mut path = String::new();
        for (i, arg) in args.iter().enumerate() {
            if arg == "-C" && i + 1 < args.len() {
                path = args[i + 1].clone();
            }
            if let Some(file) = arg.strip_prefix("--conf-file=") {
                path = file.to_string();
            }
        }
        // procd's known generated file, never an arbitrary command argument path.
        let generated = Path::new(&path);
        let known = generated.parent() == Some(Path::new("/var/etc"))
            && generated
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("dnsmasq.conf."));
        if known {
            let (body, ok) = expand(generated, reader);
            instance.readable = ok;
            parse(&body, &mut instance);
        }
        out.push(instance);
    }
    out
}

fn ipv4(address: &str) -> Option<Ipv4Addr> {
    address.parse().ok()
}

fn parse(body: &str, instance: &mut Instance) {
    for line in body.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key {
            "dhcp-leasefile" => instance.lease_file = value.to_string(),
            "interface" => instance.interfaces.push(value.to_string()),
            "no-dhcp-interface" | "except-interface" => instance.excluded.push(value.to_string()),
            "dhcp-range" => {
                let mut fields: Vec<&str> = value.split(',').collect();
                let mut r = Range::default();
                while fields
                    .first()
                    .is_some_and(|f| f.starts_with("set:") || f.starts_with("tag:"))
                {
                    if let Some(tag) = fields[0].strip_prefix("set:") {
                        r.tag = tag.to_string();
                    }
                    fields.remove(0);
                }
                if fields.len() < 2 || ipv4(fields[0]).is_none() {
                    continue;
                }
                r.first = fields[0].to_string();
                r.last = fields[1].to_string();
                if r.last != "static" && ipv4(&r.last).is_none() {
                    continue;
                }
                for field in &fields[2..] {
                    if ipv4(field).is_some() && r.mask.is_empty() {
                        r.mask = field.to_string();
                    } else if ipv4(field).is_none() && !field.trim().is_empty() {
                        r.duration = field
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_string();
                    }
                }
                instance.ranges.push(r);
            }
            _ => {}
        }
    }
}

/// Serving is what the instances serving one network add up to: their ranges
/// for its tag, their lease files, and their health.
#[derive(Default)]
struct Serving {
    matched: Vec<Range>,
    lease_files: BTreeSet<String>,
    running: bool,
    stopped: bool,
    failed: bool,
    unknown: bool,
    blocked: bool,
}

/// serving joins the instances that serve the network tagged tag on device:
/// the one the section binds, or without a binding every instance with a range
/// for the tag.
fn serving(instances: &[Instance], selected: &str, tag: &str, device: &str) -> Serving {
    let mut s = Serving::default();
    for instance in instances {
        if !selected.is_empty() && instance.id != selected {
            continue;
        }
        let ranges: Vec<Range> = instance
            .ranges
            .iter()
            .filter(|r| r.tag == tag)
            .cloned()
            .collect();
        // An explicit instance binding must not borrow another daemon's health.
        // Without a binding, a range's network tag identifies the serving instance.
        if ranges.is_empty() && selected.is_empty() {
            continue;
        }
        s.running |= instance.running;
        s.stopped |= !instance.running;
        s.failed |= instance.failed;
        s.unknown |= !instance.readable;
        if !instance.interfaces.is_empty() {
            let allowed = instance
                .interfaces
                .iter()
                .any(|p| device_matches(p, device));
            s.blocked |= !allowed;
        }
        for excluded in &instance.excluded {
            s.blocked |= device_matches(excluded, device);
        }
        s.matched.extend(ranges);
        s.lease_files.insert(instance.lease_file.clone());
    }
    s
}

impl Serving {
    /// judge is the network's verdict from its serving instances and its live
    /// netifd status.
    fn judge(
        &self,
        enabled: bool,
        selected: &str,
        live: Option<&Value>,
        instances: &[Instance],
    ) -> NetworkState {
        let addressed =
            !self.matched.is_empty() && self.matched.iter().all(|r| on_network(&r.first, live));
        let mut state = NetworkState {
            enabled,
            state: "disabled".into(),
            ..NetworkState::default()
        };
        let warn = |state: &mut NetworkState, reason: &str| {
            state.state = "warning".into();
            state.reason = reason.into();
        };
        if !enabled && self.matched.is_empty() {
        } else if !self.running && (self.failed || self.stopped) {
            state.state = "stopped".into();
            if self.failed {
                state.reason = "failed".into();
            }
        } else if !self.running && self.matched.is_empty() {
            // An enabled network no instance joined. A bound instance that did
            // not join is absent; without a binding, an instance that runs
            // serves no range for this network.
            state.state = "stopped".into();
            if selected.is_empty() && instances.iter().any(|i| i.running) {
                let unknown = instances.iter().any(|i| !i.readable);
                warn(&mut state, if unknown { "unknown" } else { "not-serving" });
            }
        } else if live.is_none() {
            warn(&mut state, "interface-unavailable");
        } else if live.is_some_and(|l| truthy(&l["pending"]) || !truthy(&l["up"])) {
            warn(&mut state, "interface-down");
        } else if self.unknown {
            warn(&mut state, "unknown");
        } else if self.blocked || self.matched.is_empty() || !addressed {
            warn(&mut state, "not-serving");
        } else if self.stopped || self.failed {
            warn(&mut state, "partial");
        } else {
            state.state = "running".into();
        }
        state
    }

    /// pool states the network's ranges, lease time and live leases, and turns
    /// a running verdict into a warning when the leases cannot be read or every
    /// address in the pool is taken.
    fn pool(&self, state: &mut NetworkState, live: Option<&Value>, reader: &impl Reader, now: i64) {
        let mut leased: BTreeSet<String> = BTreeSet::new();
        let mut lease_ok = true;
        for path in &self.lease_files {
            let Ok(data) = reader.leases(Path::new(path)) else {
                lease_ok = false;
                continue;
            };
            for line in data.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 3 {
                    continue;
                }
                let Ok(expiry) = fields[0].parse::<i64>() else {
                    continue;
                };
                if expiry != 0 && expiry <= now {
                    continue;
                }
                if on_network(fields[2], live) {
                    leased.insert(fields[2].to_string());
                }
            }
        }
        let mut pools = Vec::new();
        let (mut capacity, mut used) = (0u64, 0u64);
        for r in &self.matched {
            if r.last == "static" {
                pools.push("reservations".to_string());
            } else {
                pools.push(format!("{}–{}", r.first, r.last));
                let (first, last) = (number(&r.first), number(&r.last));
                if last >= first {
                    capacity += u64::from(last) - u64::from(first) + 1;
                    used += leased
                        .iter()
                        .map(|ip| number(ip))
                        .filter(|n| *n >= first && *n <= last)
                        .count() as u64;
                }
            }
            if state.lease_time.is_empty() {
                state.lease_time = r.duration.clone();
            }
        }
        state.pool = pools.join(", ");
        if lease_ok && live.is_some() {
            state.leases = Some(leased.len());
        }
        if state.state == "running" && !lease_ok {
            state.state = "warning".into();
            state.reason = "leases-unavailable".into();
        }
        if state.state == "running" && capacity > 0 && used >= capacity {
            state.state = "warning".into();
            state.reason = "pool-full".into();
        }
    }
}

fn number(address: &str) -> u32 {
    ipv4(address).map(u32::from).unwrap_or(0)
}

fn on_network(address: &str, live: Option<&Value>) -> bool {
    let Some(ip) = ipv4(address) else {
        return false;
    };
    live.and_then(|l| l["ipv4-address"].as_array())
        .into_iter()
        .flatten()
        .any(|v| {
            let mask = v["mask"].as_u64().unwrap_or(0);
            match ipv4(&text(&v["address"])) {
                Some(router) if mask > 0 && mask <= 32 => {
                    let bits = u32::MAX.checked_shl(32 - mask as u32).unwrap_or(0);
                    u32::from(router) & bits == u32::from(ip) & bits
                }
                _ => false,
            }
        })
}

fn device_matches(pattern: &str, device: &str) -> bool {
    !device.is_empty()
        && (pattern == device
            || pattern
                .strip_suffix('*')
                .is_some_and(|prefix| device.starts_with(prefix)))
}

/// clean is a path with its `.` parts gone and each `..` taking the part
/// before it, as Go's filepath.Clean has it.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// expand follows the daemon's includes too: a custom no-dhcp-interface must
/// not leave a green chip merely because the generated main file contains a
/// range. It answers what it read and whether everything could be read; an
/// executable configuration source stays unverifiable.
fn expand(path: &Path, reader: &impl Reader) -> (String, bool) {
    let mut seen = BTreeSet::new();
    let mut output = String::new();
    let mut ok = true;
    visit(path, reader, &mut seen, &mut output, &mut ok);
    (output, ok)
}

fn visit(
    path: &Path,
    reader: &impl Reader,
    seen: &mut BTreeSet<PathBuf>,
    output: &mut String,
    ok: &mut bool,
) {
    let path = clean(path);
    if seen.contains(&path) {
        return;
    }
    if seen.len() >= 64 || output.len() >= READ_LIMIT {
        *ok = false;
        return;
    }
    seen.insert(path.clone());
    let Ok(body) = reader.config(&path) else {
        *ok = false;
        return;
    };
    if output.len() + body.len() > READ_LIMIT {
        *ok = false;
        return;
    }
    output.push_str(&body);
    output.push('\n');
    for line in body.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key {
            "conf-file" => {
                if !Path::new(value).is_absolute() {
                    *ok = false;
                    continue;
                }
                visit(Path::new(value), reader, seen, output, ok);
            }
            "conf-script" => *ok = false,
            "conf-dir" => {
                let parts: Vec<&str> = value.split(',').collect();
                let dir = Path::new(parts[0]);
                if !dir.is_absolute() {
                    *ok = false;
                    continue;
                }
                let Ok(names) = reader.list(dir) else {
                    *ok = false;
                    continue;
                };
                for name in names {
                    if name.starts_with('.')
                        || name.ends_with('~')
                        || (name.starts_with('#') && name.ends_with('#'))
                    {
                        continue;
                    }
                    let (mut excluded, mut restricted, mut included) = (false, false, false);
                    for ext in &parts[1..] {
                        if let Some(wanted) = ext.strip_prefix('*') {
                            restricted = true;
                            included |= name.ends_with(wanted);
                        } else {
                            excluded |= name.ends_with(ext);
                        }
                    }
                    if !excluded && (!restricted || included) {
                        visit(&dir.join(&name), reader, seen, output, ok);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Table reads files from a map; a missing one cannot be read.
    struct Table(HashMap<String, String>);

    impl Reader for Table {
        fn config(&self, path: &Path) -> Result<String, String> {
            self.0
                .get(path.to_str().unwrap())
                .cloned()
                .ok_or_else(|| "unreadable".into())
        }
        fn leases(&self, path: &Path) -> Result<String, String> {
            self.config(path)
        }
        fn list(&self, path: &Path) -> Result<Vec<String>, String> {
            System.list(path)
        }
    }

    fn fixture() -> (Value, Value, Value, HashMap<String, String>) {
        let config = json!({"lan": {".type": "dhcp", "interface": "lan"}});
        let services = json!({"dnsmasq": {"instances": {"main": {"running": 1, "pid": 123,
            "command": ["/usr/sbin/dnsmasq", "-C", "/var/etc/dnsmasq.conf.lan", "-k"]}}}});
        let dump = json!({"interface": [{"interface": "lan", "up": 1, "l3_device": "br-lan",
            "ipv4-address": [{"address": "192.168.1.1", "mask": 24}]}]});
        let files = HashMap::from([
            ("/var/etc/dnsmasq.conf.lan".to_string(),
             "dhcp-leasefile=/tmp/dhcp.leases\ndhcp-range=set:lan,192.168.1.100,192.168.1.249,255.255.255.0,12h\n".to_string()),
            ("/tmp/dhcp.leases".to_string(),
             "2000 aa:bb:cc:dd:ee:01 192.168.1.101 laptop *\n900 aa:bb:cc:dd:ee:02 192.168.1.102 expired *\n0 aa:bb:cc:dd:ee:03 192.168.1.50 reserved *\n2000 aa:bb:cc:dd:ee:04 192.168.2.100 other *\n2000 aa:bb:cc:dd:ee:01 192.168.1.101 duplicate *\n".to_string()),
        ]);
        (config, services, dump, files)
    }

    type Change = fn(&mut Value, &mut Value, &mut Value, &mut HashMap<String, String>);

    #[test]
    fn observed_service_states() {
        let cases: Vec<(&str, &str, &str, Change)> = vec![
            ("running", "running", "", |_, _, _, _| {}),
            ("stopped", "stopped", "", |_, s, _, _| {
                s["dnsmasq"]["instances"]["main"]["running"] = json!(0)
            }),
            ("failed", "stopped", "failed", |_, s, _, _| {
                s["dnsmasq"]["instances"]["main"]["running"] = json!(false);
                s["dnsmasq"]["instances"]["main"]["exit_code"] = json!(1);
            }),
            (
                "down interface",
                "warning",
                "interface-down",
                |_, _, d, _| d["interface"][0]["up"] = json!(false),
            ),
            (
                "different active subnet",
                "warning",
                "not-serving",
                |_, _, d, _| d["interface"][0]["ipv4-address"][0]["address"] = json!("192.168.2.1"),
            ),
            (
                "range suppressed",
                "warning",
                "not-serving",
                |_, _, _, f| {
                    f.insert(
                        "/var/etc/dnsmasq.conf.lan".into(),
                        "no-dhcp-interface=br-lan\n".into(),
                    );
                },
            ),
            (
                "excluded interface",
                "warning",
                "not-serving",
                |_, _, _, f| {
                    f.get_mut("/var/etc/dnsmasq.conf.lan")
                        .unwrap()
                        .push_str("no-dhcp-interface=br-*\n");
                },
            ),
            ("unreadable config", "warning", "unknown", |_, _, _, f| {
                f.remove("/var/etc/dnsmasq.conf.lan");
            }),
            (
                "unreadable leases",
                "warning",
                "leases-unavailable",
                |_, _, _, f| {
                    f.remove("/tmp/dhcp.leases");
                },
            ),
            ("disabled", "disabled", "", |c, _, _, f| {
                c["lan"]["ignore"] = json!("1");
                f.insert("/var/etc/dnsmasq.conf.lan".into(), String::new());
            }),
            ("full pool", "warning", "pool-full", |_, _, _, f| {
                f.insert(
                    "/var/etc/dnsmasq.conf.lan".into(),
                    "dhcp-range=set:lan,192.168.1.101,192.168.1.101,255.255.255.0,12h\n".into(),
                );
            }),
            ("reservations only", "running", "", |_, _, _, f| {
                f.insert(
                    "/var/etc/dnsmasq.conf.lan".into(),
                    "dhcp-range=tag:trusted,set:lan,192.168.1.100,static,255.255.255.0,12h\n"
                        .into(),
                );
            }),
            (
                "wrong instance cannot borrow health",
                "stopped",
                "",
                |c, s, _, _| {
                    c["lan"]["instance"] = json!("other");
                    s["dnsmasq"]["instances"]["other"] = json!({"running": false});
                },
            ),
            ("bound instance absent", "stopped", "", |c, _, _, _| {
                c["lan"]["instance"] = json!("nosuch")
            }),
        ];
        for (name, want_state, want_reason, change) in cases {
            let (mut c, mut s, mut d, mut f) = fixture();
            change(&mut c, &mut s, &mut d, &mut f);
            let got = state(&c, &s, &d, &Table(f), 1000);
            let lan = &got["lan"];
            assert_eq!(
                (
                    lan["state"].as_str().unwrap_or_default(),
                    lan["reason"].as_str().unwrap_or_default()
                ),
                (want_state, want_reason),
                "{name}: {lan}"
            );
        }
    }

    #[test]
    fn leases_use_the_subnet_expiry_and_unique_addresses() {
        let (c, s, d, f) = fixture();
        let lan = &state(&c, &s, &d, &Table(f), 1000)["lan"];
        assert_eq!(lan["leases"], 2);
        assert_eq!(lan["pool"], "192.168.1.100–192.168.1.249");
        assert_eq!(lan["lease_time"], "12h");
        assert_eq!(lan["enabled"], true);
    }

    #[test]
    fn includes_can_suppress_a_generated_range() {
        let (c, s, d, mut f) = fixture();
        f.get_mut("/var/etc/dnsmasq.conf.lan")
            .unwrap()
            .push_str("conf-file=/etc/dnsmasq.conf\n");
        f.insert(
            "/etc/dnsmasq.conf".into(),
            "no-dhcp-interface=br-lan\n".into(),
        );
        let lan = state(&c, &s, &d, &Table(f.clone()), 1000)["lan"].clone();
        assert_eq!(
            (lan["state"].as_str(), lan["reason"].as_str()),
            (Some("warning"), Some("not-serving"))
        );
        f.remove("/etc/dnsmasq.conf");
        let lan = state(&c, &s, &d, &Table(f), 1000)["lan"].clone();
        assert_eq!(
            (lan["state"].as_str(), lan["reason"].as_str()),
            (Some("warning"), Some("unknown"))
        );
    }

    /// adblock drops a blocklist of many megabytes into dnsmasq's runtime
    /// directory. Only the lines that decide DHCP scope are kept from a config
    /// file, so a list that size is read through rather than taken as an
    /// unreadable include, and the server it sits beside is still verified.
    #[test]
    fn a_blocklist_beside_the_config_is_scanned_not_refused() {
        let root = std::env::temp_dir().join(format!("dnsdhcp-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("dnsmasq.cfg01411c.d");
        fs::create_dir_all(&dir).unwrap();
        let main = root.join("dnsmasq.conf.cfg01411c");
        let body = format!(
            "dhcp-range=set:lan,192.168.1.100,192.168.1.249,255.255.255.0,12h\nconf-dir={}\n",
            dir.display()
        );
        fs::write(&main, &body).unwrap();
        let list = "local=/ads.example.com/\n".repeat(400_000) + "interface=br-lan\n";
        assert!(list.len() > 1 << 20, "the list must outgrow 1 MiB");
        fs::write(dir.join("adb_list.overall"), list).unwrap();
        let (got, ok) = expand(&main, &System);
        assert!(ok, "a large blocklist is refused");
        assert_eq!(got, format!("{body}\ninterface=br-lan\n\n"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_device_matches_its_name_or_a_trailing_wildcard() {
        assert!(device_matches("br-lan", "br-lan"));
        assert!(device_matches("br-*", "br-lan"));
        assert!(!device_matches("br-*", ""));
        assert!(!device_matches("eth*", "br-lan"));
    }
}
