// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

//! What the router's resolver can do, for the DNS page: whether dnsmasq is
//! installed and built with DNSSEC, whether https-dns-proxy and adblock are,
//! and which program answers each local UDP port — what a `127.0.0.1#port`
//! upstream really is (AdGuard Home, https-dns-proxy).
//!
//! This is the plugin's root half (ADR-027), run as root by the privileged
//! helper for the `dns-state` read: another process's open sockets are root's
//! to read.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn state() -> Map<String, Value> {
    let features = Command::new("/usr/sbin/dnsmasq")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok();
    let dnssec = features.as_ref().is_some_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .any(|w| w == "DNSSEC")
    });
    let mut out = Map::new();
    out.insert(
        "resolver".into(),
        json!(Path::new("/usr/sbin/dnsmasq").is_file()),
    );
    out.insert("dnssec".into(), json!(dnssec));
    out.insert(
        "doh".into(),
        json!(Path::new("/usr/sbin/https-dns-proxy").is_file()),
    );
    out.insert(
        "adblock".into(),
        json!(Path::new("/etc/init.d/adblock").is_file()),
    );
    out.insert("listeners".into(), local_listeners());
    out
}

/// local_listeners is each UDP port a program on this router answers on, for
/// loopback or every address, with the name of the process that holds it.
fn local_listeners() -> Value {
    let mut ports = BTreeMap::new();
    for table in ["/proc/net/udp", "/proc/net/udp6"] {
        ports.extend(udp_listeners(
            &fs::read_to_string(table).unwrap_or_default(),
        ));
    }
    let mut out = Map::new();
    if ports.is_empty() {
        return Value::Object(out);
    }
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name();
        let Some(pid) = pid
            .to_str()
            .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            let Some(port) = target
                .strip_prefix("socket:[")
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|inode| ports.get(inode))
            else {
                continue;
            };
            let name = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            out.entry(port.to_string()).or_insert(json!(name.trim()));
        }
    }
    Value::Object(out)
}

/// udp_listeners reads a /proc/net/udp or udp6 table: each socket bound to
/// loopback or every address and talking to no one, by inode, with its port.
fn udp_listeners(table: &str) -> Vec<(String, u16)> {
    const LOCAL: [&str; 4] = [
        "0100007F",
        "00000000",
        "00000000000000000000000001000000",
        "00000000000000000000000000000000",
    ];
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (address, port) = fields.get(1)?.split_once(':')?;
            let (_, remote_port) = fields.get(2)?.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let inode = *fields.get(9)?;
            (LOCAL.contains(&address) && remote_port == "0000" && port != 0 && inode != "0")
                .then(|| (inode.to_string(), port))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local upstream is whatever answers on that port here: the table's
    /// loopback and any-address sockets, by inode, never one talking out.
    #[test]
    fn a_udp_table_lists_the_local_listeners_by_inode() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n\
   1: 0100007F:13BD 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 4242 2 0 0\n\
   2: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 4343 2 0 0\n\
   3: 0101A8C0:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 4444 2 0 0\n\
   4: 0100007F:C350 08080808:0035 01 00000000:00000000 00:00000000 00000000     0        0 4545 2 0 0\n";
        assert_eq!(
            udp_listeners(table),
            vec![("4242".to_string(), 5053), ("4343".to_string(), 53)]
        );
        let six = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   1: 00000000000000000000000001000000:13BD 00000000000000000000000000000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 5151 2 0 0\n";
        assert_eq!(udp_listeners(six), vec![("5151".to_string(), 5053)]);
    }
}
