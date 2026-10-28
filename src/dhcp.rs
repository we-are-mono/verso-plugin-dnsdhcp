// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

//! The DHCP page, top to bottom: the servers, the reservations, the leases, and
//! the daemon's settings.
//!
//! A server and a reservation are objects, so each lives in its own drawer over
//! the page (ADR-005 §8); the page's address names the open one (`?open=`), so a
//! drawer survives a reload and travels in a link. The settings have no row, so
//! they are edited on the page itself.

use verso_plugin::{
    commit, commit_delete, commit_new, dhcp, json, ColumnWidth, CommitOp, Envelope, Errors, Form, HeadingAct,
    List, Map, Request, RowDrawer, SelectOption, Switch, Table, TableCell, TableColumn, TableRow,
    TableRowAct, Tone, Value, Widget,
};

use crate::form;
use crate::format::{self, Subnet, DEFAULT_LIMIT, DEFAULT_START};
use crate::hosts;
use crate::leases::{self, RESERVE};
use crate::live::Leases;
use crate::model::{Dnsdhcp, Options, CONFIG, DAEMON_TYPE};
use crate::page::{self, NEW, NEW_SERVER, OPEN, PANEL};

/// TITLE is the page's name, which is also its nav label.
const TITLE: &str = "DHCP";

/// SETTINGS marks a submission as the settings form's.
const SETTINGS: &str = "_settings";

/// OWNED is every option a server's drawer writes on its `config dhcp` section.
/// The drawer states all of them, so an option the operator cleared is cleared
/// on disk; anything else the section carries is left as it is.
const OWNED: [&str; 9] = [
    "ignore",
    "start",
    "limit",
    "leasetime",
    "dynamicdhcp",
    "force",
    "ra",
    "dhcpv6",
    "ra_slaac",
];

/// MODES are odhcpd's words for how a network's IPv6 is served.
const MODES: [(&str, &str); 4] = [
    ("disabled", "Off"),
    ("server", "Serve"),
    ("relay", "Relay"),
    ("hybrid", "Hybrid"),
];

/// get answers a visit: the page, with the drawer its address names open.
pub fn get(r: &Request) -> Envelope {
    let state = State::read(r);
    let key = r.query.get(OPEN);
    let open = match key.trim() {
        "" => Open::Nothing,
        NEW => Open::Reservation(None, hosts::blank(&state.leases, &r.query.get(RESERVE))),
        NEW_SERVER => state.blank_server(None),
        key => state.opened(key),
    };
    state.render(open)
}

/// post answers a submission: a row's confirmed removal, a drawer's save, or the
/// settings form. Anything else is something this page did not draw.
pub fn post(r: &Request, f: &Form) -> Envelope {
    let state = State::read(r);
    let server = f.get(REMOVE_SERVER);
    if !server.trim().is_empty() {
        return state.remove_server(server.trim());
    }
    let removal = f.get(hosts::REMOVE);
    if !removal.trim().is_empty() {
        return match hosts::remove(&state.model, removal.trim()) {
            Some(op) => state
                .render(Open::Nothing)
                .with_notice(Tone::Success, "Reservation removed.")
                .with_commit(vec![op]),
            None => state.render(Open::Nothing).with_notice(Tone::Danger, GONE),
        };
    }
    if f.get(PANEL) == "1" {
        return state.save_drawer(&r.query.get(OPEN), f);
    }
    if f.get(SETTINGS) == "1" {
        return state.save_settings(f);
    }
    state
        .render(Open::Nothing)
        .with_notice(Tone::Danger, form::UNKNOWN)
}

const GONE: &str = "That isn’t in the config any more, so there was nothing to save.";

/// REMOVE_SERVER names the server a row's confirmed removal takes away, apart
/// from a reservation's.
const REMOVE_SERVER: &str = "_remove_server";

/// new_server is the page's act: the page is about servers. Making one is
/// editing one that does not exist yet, so it opens the server's own panel,
/// carrying it blank when an address asks for one.
fn new_server(blank: Option<RowDrawer>) -> HeadingAct {
    HeadingAct {
        label: "Add DHCP server".into(),
        href: page::open_href(NEW_SERVER),
        icon: "plus".into(),
        opens_panel: true,
        drawer: blank,
        ..Default::default()
    }
}

/// new_reservation is the Reservations card's act, on its header: a blank
/// reservation is a place of this page, so the address follows its drawer.
fn new_reservation() -> Widget {
    Widget::Link {
        label: "Add reservation".into(),
        icon: "plus".into(),
        href: page::open_href(NEW),
        style: "act".into(),
        panel: true,
        place: true,
    }
}

/// State is everything one render reads: the config, the live leases, and the
/// shell's live reading of each network's server.
struct State {
    model: Dnsdhcp,
    leases: Leases,
    servers: Vec<dhcp::Server>,
}

/// Open is the drawer in front of the page, if any.
enum Open {
    Nothing,
    Server(String, RowDrawer),
    NewServer(RowDrawer),
    Reservation(Option<String>, RowDrawer),
}

impl State {
    fn read(r: &Request) -> State {
        State {
            model: Dnsdhcp::read(&r.snapshot),
            leases: Leases::read(&r.ubus),
            servers: dhcp::servers(&r.snapshot, &r.ubus),
        }
    }

    /// has_server says whether a network has a DHCP server, by the SDK's one
    /// meaning of it (`dhcp::Server::exists`), which Interfaces reads too.
    fn has_server(&self, network: &str) -> bool {
        self.servers
            .iter()
            .any(|server| server.network == network && server.exists())
    }

    /// free is the networks that could take a server and have none
    /// (`dhcp::Server::can_be_added`).
    fn free(&self) -> Vec<&str> {
        self.servers
            .iter()
            .filter(|server| server.can_be_added())
            .map(|server| server.network.as_str())
            .collect()
    }

    /// opened is the drawer an address names: a server by its network, a
    /// network that could take one by the blank server with it chosen, else a
    /// reservation by its section or its device's MAC. A name that is none of
    /// these opens nothing — a stale link lands on the page.
    fn opened(&self, key: &str) -> Open {
        if self.has_server(key) {
            let pool = Pool::read(self.model.pool(key));
            return Open::Server(
                key.into(),
                self.server_drawer(key, &pool, &Errors::default()),
            );
        }
        if self.free().contains(&key) {
            return self.blank_server(Some(key));
        }
        match hosts::find(&self.model, key) {
            Some(options) => {
                let host = hosts::Host::read(options);
                let section = host.section.clone();
                let drawer = hosts::drawer(Some(&section), &host, &Errors::default());
                Open::Reservation(Some(section), drawer)
            }
            None => Open::Nothing,
        }
    }

    /// save_drawer answers the open drawer's save. A refusal stays in the drawer
    /// with what was typed; an accepted save stages and keeps the drawer, which
    /// is also what a live preview of the form asks for.
    fn save_drawer(&self, key: &str, f: &Form) -> Envelope {
        let key = key.trim();
        if key == NEW {
            let saved = hosts::submit(None, f);
            return self.staged(Open::Reservation(None, saved.drawer), saved.commit);
        }
        if key == NEW_SERVER || self.free().contains(&key) {
            return self.save_new_server(f);
        }
        if self.has_server(key) {
            let pool = Pool::submitted(f);
            let errors = self.validate(key, &pool);
            let drawer = self.server_drawer(key, &pool, &errors);
            let commit = errors.is_empty().then(|| self.write(key, &pool));
            return self.staged(Open::Server(key.into(), drawer), commit);
        }
        match hosts::find(&self.model, key) {
            Some(options) => {
                let section = options.section.clone();
                let saved = hosts::submit(Some(&section), f);
                self.staged(Open::Reservation(Some(section), saved.drawer), saved.commit)
            }
            None => self.render(Open::Nothing).with_notice(Tone::Danger, GONE),
        }
    }

    /// blank_server is the drawer that makes a server, with `network` chosen
    /// or else the first that could take one. With none free it opens nothing.
    fn blank_server(&self, network: Option<&str>) -> Open {
        let free = self.free();
        match network.or(free.first().copied()) {
            Some(network) => Open::NewServer(self.new_server_drawer(
                network,
                &Pool::blank(),
                &Errors::default(),
            )),
            None => Open::Nothing,
        }
    }

    /// save_new_server answers the blank server's save: the network it names
    /// must be one that could take a server, and the pool is checked against it.
    fn save_new_server(&self, f: &Form) -> Envelope {
        let free = self.free();
        if free.is_empty() {
            return self.render(Open::Nothing).with_notice(Tone::Danger, GONE);
        }
        let network = f.get("network").trim().to_string();
        let pool = Pool::submitted(f);
        let errors = match free.contains(&network.as_str()) {
            true => self.validate(&network, &pool),
            false => {
                let mut errors = Errors::default();
                errors.field("network", "Choose a network that has no DHCP server yet.");
                errors
            }
        };
        let drawer = self.new_server_drawer(&network, &pool, &errors);
        let commit = errors.is_empty().then(|| self.write(&network, &pool));
        self.staged(Open::NewServer(drawer), commit)
    }

    /// remove_server answers a row's confirmed removal: the server's section
    /// goes, and the network returns to the ones a server can be added to.
    fn remove_server(&self, network: &str) -> Envelope {
        match self.model.pool(network).filter(|_| self.has_server(network)) {
            Some(options) => self
                .render(Open::Nothing)
                .with_notice(Tone::Success, "DHCP server removed.")
                .with_commit(vec![commit_delete(CONFIG, &options.section)]),
            None => self.render(Open::Nothing).with_notice(Tone::Danger, GONE),
        }
    }

    fn staged(&self, open: Open, commit: Option<CommitOp>) -> Envelope {
        let answer = self.render(open);
        match commit {
            Some(op) => answer.with_commit(vec![op]),
            None => answer.with_notice(Tone::Danger, form::REFUSED),
        }
    }

    fn render(&self, open: Open) -> Envelope {
        self.render_with(open, &Errors::default())
    }

    /// render_with draws the page with the drawer `open` names in front of it
    /// and the settings form carrying whatever its last save got wrong.
    fn render_with(&self, open: Open, settings: &Errors) -> Envelope {
        let (mut server_open, mut new_server_open, mut host_open, mut blank) =
            (None, None, None, None);
        match open {
            Open::Nothing => {}
            Open::Server(network, drawer) => server_open = Some((network, drawer)),
            Open::NewServer(drawer) => new_server_open = Some(drawer),
            Open::Reservation(Some(section), drawer) => host_open = Some((section, drawer)),
            Open::Reservation(None, drawer) => blank = Some(drawer),
        }
        let mut reservations = section(
            "Reservations",
            "",
            hosts::table(&self.model, host_open, blank),
        );
        if let Widget::Section(verso_plugin::SectionWidget { control, .. }) = &mut reservations {
            *control = Some(Box::new(new_reservation()));
        }
        let page = Envelope::page(
            TITLE,
            Widget::stack(vec![
                section("Servers", "", self.servers_table(server_open)),
                reservations,
                section("Leases", "", leases::table(&self.model, &self.leases)),
                section(
                    "Settings",
                    "These apply to every server above.",
                    self.settings_form(settings),
                ),
            ]),
        )
        .with_width("wide");
        // A server can be added while some network could take one.
        match self.free().is_empty() {
            true => page,
            false => page.with_act(new_server(new_server_open)),
        }
    }

    fn servers_table(&self, open: Option<(String, RowDrawer)>) -> Widget {
        let mut open = open;
        let rows = self
            .servers
            .iter()
            .filter(|server| self.has_server(&server.network))
            .map(|server| {
                let drawer = match &open {
                    Some((network, _)) if *network == server.network => open.take().map(|(_, d)| d),
                    _ => None,
                };
                self.server_row(server, drawer)
            })
            .collect();
        Widget::Table(Table {
            columns: [
                ("Network", "reference", ColumnWidth::Short),
                ("Range", "mono", ColumnWidth::Grow),
                ("Leased", "meter", ColumnWidth::Name),
                ("Lease time", "keyword", ColumnWidth::Short),
                ("IPv6", "text", ColumnWidth::Address),
                ("State", "status", ColumnWidth::Word),
                ("", "actions", ColumnWidth::Short),
            ]
            .into_iter()
            .map(|(label, kind, width)| TableColumn {
                label: label.into(),
                kind: kind.into(),
                width,
            })
            .collect(),
            rows,
            empty_text: match self.free().is_empty() {
                true => "No network has an address of its own to hand out from. Add one in \
                         Interfaces.",
                false => "No network has a DHCP server yet. Add one with Add DHCP server.",
            }
            .into(),
            ..Default::default()
        })
    }

    fn server_row(&self, server: &dhcp::Server, drawer: Option<RowDrawer>) -> TableRow {
        let network = &server.network;
        let door = page::open_href(network);
        let pool = Pool::read(self.model.pool(network));
        let subnet = self.model.subnet(network);
        let serving = pool.serve && subnet.is_some();
        let (state, tone) = match subnet {
            None => ("Client — uplink", "neutral"),
            Some(_) => (server.label(), server.tone()),
        };
        let leased = subnet.map_or(0, |s| self.leases.on(&s)) as u32;
        let size = format::offset(&pool.limit, DEFAULT_LIMIT).max(1);
        let quiet = |cell: TableCell| match serving {
            true => cell,
            false => page::text_cell(""),
        };
        TableRow {
            id: network.clone(),
            cells: vec![
                TableCell {
                    text: network.clone(),
                    href: door.clone(),
                    ..TableCell::default()
                },
                quiet(page::mono_cell(&format::range_label(
                    subnet,
                    &pool.start,
                    &pool.limit,
                ))),
                quiet(TableCell {
                    text: leased.to_string(),
                    fill: (leased * 100 / size).min(100),
                    variant: match leased * 100 / size {
                        0..=79 => "",
                        80..=94 => "warning",
                        _ => "danger",
                    }
                    .into(),
                    ..TableCell::default()
                }),
                quiet(page::mono_cell(&pool.leasetime)),
                quiet(page::text_cell(pool.ipv6_words())),
                TableCell {
                    text: state.into(),
                    variant: tone.into(),
                    dot: true,
                    ..TableCell::default()
                },
                TableCell {
                    actions: vec![
                        TableRowAct {
                            icon: "square-pen".into(),
                            title: "Edit".into(),
                            href: door.clone(),
                            ..TableRowAct::default()
                        },
                        TableRowAct {
                            icon: "trash-2".into(),
                            title: "Remove DHCP server".into(),
                            name: REMOVE_SERVER.into(),
                            value: network.clone(),
                            confirm_title: "Remove the DHCP server on %s?".into(),
                            confirm: "Devices on this network keep their addresses until their \
                                      leases run out, then get none."
                                .into(),
                            confirm_cancel: "Keep DHCP server".into(),
                            ..TableRowAct::default()
                        },
                    ],
                    ..TableCell::default()
                },
            ],
            drawer,
            panel: door,
            // A server kept off is a subject someone switched off.
            muted: !pool.serve,
            ..TableRow::default()
        }
    }

    /// server_drawer is one network's DHCP server: what the interface editor
    /// used to carry, plus how the network's IPv6 is announced. Save stages.
    fn server_drawer(&self, network: &str, pool: &Pool, errors: &Errors) -> RowDrawer {
        self.pool_drawer(network, None, network, pool, errors)
    }

    /// new_server_drawer is a server not made yet: the same drawer, led by the
    /// network it will serve, chosen among those that could take one.
    fn new_server_drawer(&self, network: &str, pool: &Pool, errors: &Errors) -> RowDrawer {
        let networks = self
            .free()
            .into_iter()
            .map(|free| SelectOption::new(free, free))
            .collect();
        let choice = form::select_field("network", "Network", network, networks, errors)
            .writes("interface")
            .typed("uciname");
        self.pool_drawer("New DHCP server", Some(choice), network, pool, errors)
    }

    /// pool_drawer is the drawer one server is read and written in, titled
    /// `title`, with `lead` as its first field when it has one.
    fn pool_drawer(
        &self,
        title: &str,
        lead: Option<Widget>,
        network: &str,
        pool: &Pool,
        errors: &Errors,
    ) -> RowDrawer {
        let subnet = self.model.subnet(network);
        let mut addresses: Vec<Widget> = lead.into_iter().collect();
        addresses.push(checkbox(
            "serve",
            "Hand out addresses",
            "ignore",
            "",
            pool.serve,
            errors,
        ));
        if subnet.is_some() {
            addresses.push(
                Widget::form_grid(
                    2,
                    vec![
                        field("start", "First address", &pool.start, "", errors),
                        field("limit", "How many", &pool.limit, "", errors),
                    ],
                )
                .labelled("Address pool", &range_words(subnet, pool)),
            );
        }
        addresses.extend([
            field(
                "leasetime",
                "Lease time",
                &pool.leasetime,
                "How long an address stays with a device after it was last seen: 45m, 12h, 7d \
                 or infinite.",
                errors,
            ),
            checkbox(
                "reservations_only",
                "Only devices with a reservation",
                "dynamicdhcp",
                "Everything else is refused an address.",
                pool.reservations_only,
                errors,
            ),
            checkbox(
                "force",
                "Serve even if another DHCP server is seen",
                "force",
                "",
                pool.force,
                errors,
            ),
        ]);
        let mut dns =
            Widget::list("dns", "DNS servers", "ip4addr", &pool.dns, "").writes("dhcp_option");
        if let Widget::List(List {
            style, errors: e, ..
        }) = &mut dns
        {
            *style = "rows".into();
            if !errors.get("dns").is_empty() {
                e.insert("0".into(), errors.get("dns").into());
            }
        }
        let fields = vec![
            Widget::section("", "", addresses).flush(),
            Widget::section(
                "What devices are told",
                "Left blank, devices are told to use this router for both.",
                vec![
                    field("gateway", "Gateway", &pool.gateway, "", errors).writes("dhcp_option"),
                    dns,
                ],
            )
            .ruled(),
            Widget::section(
                "IPv6",
                "",
                vec![
                    select("ra", "Router advertisements", &pool.ra, errors),
                    checkbox(
                        "ra_slaac",
                        "Let devices make their own address",
                        "ra_slaac",
                        "SLAAC: each device derives an address from the announced prefix.",
                        pool.ra_slaac,
                        errors,
                    ),
                    select("dhcpv6", "DHCPv6", &pool.dhcpv6, errors),
                ],
            )
            .ruled(),
            Widget::config_preview(
                &format!("/etc/config/{CONFIG}"),
                &self.preview(network, pool),
            ),
        ];
        RowDrawer {
            title: title.into(),
            closed: page::root_href(),
            open: true,
            children: vec![page::panel_form("Save DHCP server", fields).at(
                CONFIG,
                self.model.pool(network).map_or(network, |p| &p.section),
            )],
            ..RowDrawer::default()
        }
    }

    /// validate answers the daemons' question about one network's server — the
    /// checks the interface editor made, against the network the server serves.
    fn validate(&self, network: &str, pool: &Pool) -> Errors {
        let mut errors = Errors::default();
        for key in ["ra", "dhcpv6"] {
            let value = match key {
                "ra" => &pool.ra,
                _ => &pool.dhcpv6,
            };
            errors.check(
                key,
                MODES.iter().any(|(mode, _)| mode == value),
                "Choose how IPv6 is served here.",
            );
        }
        if !pool.serve {
            return errors;
        }
        let Some(subnet) = self.model.subnet(network) else {
            errors.field(
                "serve",
                "A DHCP server needs the network to have a static address.",
            );
            return errors;
        };
        let fits = match (pool.start.parse::<u32>(), pool.limit.parse::<u32>()) {
            (Ok(start), Ok(limit)) => subnet.fits(start, limit),
            _ => false,
        };
        errors.check(
            "start",
            fits,
            "The range must fit the network and leave out the router’s own address.",
        );
        errors.check(
            "leasetime",
            form::valid_leasetime(&pool.leasetime),
            "Use a duration such as 12h, 30m or 7d, or infinite.",
        );
        errors.check(
            "gateway",
            pool.gateway.is_empty() || pool.gateway.split(',').all(form::valid_ipv4),
            "Enter valid IPv4 addresses, separated by commas.",
        );
        errors.check(
            "dns",
            pool.dns.iter().all(|ip| form::valid_ipv4(ip)),
            "Enter valid IPv4 addresses for the DNS servers.",
        );
        errors
    }

    /// write is the change one server's save stages. A network with no section
    /// yet gets one named after it, as OpenWrt names its own.
    fn write(&self, network: &str, pool: &Pool) -> CommitOp {
        let old = self.model.pool(network);
        let values = Value::Object(pool.values(old));
        match old {
            Some(old) => commit(CONFIG, &old.section, values),
            None => {
                let mut values = values;
                values["interface"] = json!(network);
                let mut op = commit_new(CONFIG, "dhcp", values);
                op.section = network.into();
                op
            }
        }
    }

    /// preview is the section as the save leaves it: what it keeps of what the
    /// config already says, with what the form states over it.
    fn preview(&self, network: &str, pool: &Pool) -> String {
        let old = self.model.pool(network);
        let mut options: Vec<(String, Value)> = vec![("interface".into(), json!(network))];
        if let Some(old) = old {
            options.extend(old.entries().into_iter().filter(|(option, _)| {
                option != "interface"
                    && !OWNED.contains(&option.as_str())
                    && option != "dhcp_option"
            }));
        }
        options.extend(pool.values(old));
        let options: Vec<(&str, Value)> = options
            .iter()
            .map(|(option, value)| (option.as_str(), value.clone()))
            .collect();
        page::uci_block(
            "dhcp",
            old.map_or(network, |old| old.section.as_str()),
            &options,
        )
    }

    /// settings_form is the daemon-wide options that are about DHCP, edited in
    /// place: they belong to no row.
    fn settings_form(&self, errors: &Errors) -> Widget {
        let daemon = &self.model.daemon;
        let flag = |option: &str, label: &str, desc: &str, fallback: bool| {
            checkbox(
                option,
                label,
                option,
                desc,
                daemon.flag(option, fallback),
                errors,
            )
        };
        Widget::Form {
            style: "settings".into(),
            submit: "Save DHCP settings".into(),
            error: String::new(),
            note: String::new(),
            target: String::new(),
            fields: vec![
                Widget::hidden(SETTINGS, "1"),
                flag(
                    "authoritative",
                    "Authoritative",
                    "This router is the only DHCP server on its networks, so it answers at \
                     once instead of waiting for another.",
                    true,
                ),
                flag(
                    "sequential_ip",
                    "Hand out addresses in order",
                    "The next free address in the range, instead of one picked from the \
                     device’s MAC.",
                    false,
                ),
                flag(
                    "logdhcp",
                    "Log every lease",
                    "Each address handed out or renewed is written to the system log.",
                    false,
                ),
                flag(
                    "readethers",
                    "Read /etc/ethers",
                    "Also treat the MAC-to-address pairs in that file as reservations.",
                    false,
                ),
                field(
                    "dhcpleasemax",
                    "Most leases at once",
                    daemon.scalar("dhcpleasemax"),
                    "Across every network.",
                    errors,
                ),
                field(
                    "leasefile",
                    "Lease file",
                    daemon.scalar("leasefile"),
                    "",
                    errors,
                ),
            ],
        }
        .at(CONFIG, &daemon.section)
    }

    /// save_settings reads the settings form back and stages what changed.
    fn save_settings(&self, f: &Form) -> Envelope {
        let daemon = &self.model.daemon;
        let mut errors = Errors::default();
        let max = f.get("dhcpleasemax").trim().to_string();
        errors.check(
            "dhcpleasemax",
            max.is_empty() || form::valid_number(&max, u32::MAX),
            "Enter a whole number.",
        );
        let file = f.get("leasefile").trim().to_string();
        errors.check(
            "leasefile",
            file.is_empty() || file.starts_with('/'),
            "Enter a full path, such as /tmp/dhcp.leases.",
        );
        if !errors.is_empty() {
            return self
                .render_with(Open::Nothing, &errors)
                .with_notice(Tone::Danger, form::REFUSED);
        }
        let mut values = Map::new();
        for (option, fallback) in [
            ("authoritative", true),
            ("sequential_ip", false),
            ("logdhcp", false),
            ("readethers", false),
        ] {
            let on = matches!(f.get(option).as_str(), "1" | "on");
            if on != daemon.flag(option, fallback) {
                values.insert(option.into(), json!(if on { "1" } else { "0" }));
            }
        }
        for (option, value) in [("dhcpleasemax", &max), ("leasefile", &file)] {
            if daemon.scalar(option) != value.as_str() {
                values.insert(
                    option.into(),
                    match value.is_empty() {
                        true => Value::Null,
                        false => json!(value),
                    },
                );
            }
        }
        let answer = self.render(Open::Nothing);
        if values.is_empty() {
            return answer;
        }
        answer.with_commit(vec![match daemon.section.is_empty() {
            true => commit_new(CONFIG, DAEMON_TYPE, Value::Object(values)),
            false => commit(CONFIG, &daemon.section, Value::Object(values)),
        }])
    }
}

/// Pool is one network's server as its drawer holds it, in the words the
/// drawer posts.
#[derive(Clone)]
struct Pool {
    serve: bool,
    start: String,
    limit: String,
    leasetime: String,
    reservations_only: bool,
    force: bool,
    gateway: String,
    dns: Vec<String>,
    ra: String,
    dhcpv6: String,
    ra_slaac: bool,
}

impl Pool {
    /// blank is a server being made: handing out addresses, with the daemons'
    /// own defaults.
    fn blank() -> Pool {
        Pool {
            serve: true,
            ..Pool::read(None)
        }
    }

    /// read takes a server off its section, or the server a network with no
    /// section would start as: off, with the daemons' own defaults.
    fn read(options: Option<&Options>) -> Pool {
        let empty = Options::default();
        let o = options.unwrap_or(&empty);
        let mode = |option: &str| match o.scalar(option) {
            "" => "disabled".to_string(),
            mode => mode.to_string(),
        };
        let announced = |code: &str| -> Vec<String> {
            o.list("dhcp_option")
                .iter()
                .filter_map(|entry| entry.strip_prefix(code))
                .flat_map(|values| values.split(',').map(String::from))
                .collect()
        };
        let or = |value: &str, fallback: String| match value {
            "" => fallback,
            value => value.to_string(),
        };
        Pool {
            serve: options.is_some() && o.scalar("ignore") != "1",
            start: or(o.scalar("start"), DEFAULT_START.to_string()),
            limit: or(o.scalar("limit"), DEFAULT_LIMIT.to_string()),
            leasetime: or(o.scalar("leasetime"), "12h".into()),
            reservations_only: o.scalar("dynamicdhcp") == "0",
            force: o.flag("force", false),
            gateway: announced("3,").join(","),
            dns: announced("6,"),
            ra: mode("ra"),
            dhcpv6: mode("dhcpv6"),
            ra_slaac: o.flag("ra_slaac", true),
        }
    }

    fn submitted(f: &Form) -> Pool {
        let on = |name: &str| matches!(f.get(name).as_str(), "1" | "on");
        let text = |name: &str| f.get(name).trim().to_string();
        let mut dns: Vec<String> = vec![];
        for value in f.all("dns") {
            let value = value.trim().to_string();
            if !value.is_empty() && !dns.contains(&value) {
                dns.push(value);
            }
        }
        Pool {
            serve: on("serve"),
            start: text("start"),
            limit: text("limit"),
            leasetime: text("leasetime"),
            reservations_only: on("reservations_only"),
            force: on("force"),
            gateway: text("gateway"),
            dns,
            ra: text("ra"),
            dhcpv6: text("dhcpv6"),
            ra_slaac: on("ra_slaac"),
        }
    }

    /// values is what a save writes: every owned option, a null where the
    /// daemon's default stands, and the section's DHCP options with the two this
    /// drawer edits restated. A server that is off keeps its pool as written.
    fn values(&self, old: Option<&Options>) -> Map<String, Value> {
        let flag = |on: bool, written: &str| match on {
            true => json!(written),
            false => Value::Null,
        };
        let mode = |mode: &str| match mode {
            "disabled" | "" => Value::Null,
            mode => json!(mode),
        };
        let mut values = Map::new();
        values.insert("ignore".into(), flag(!self.serve, "1"));
        if self.serve {
            values.insert("start".into(), json!(self.start));
            values.insert("limit".into(), json!(self.limit));
            values.insert("leasetime".into(), json!(self.leasetime));
        }
        values.insert("dynamicdhcp".into(), flag(self.reservations_only, "0"));
        values.insert("force".into(), flag(self.force, "1"));
        values.insert("ra".into(), mode(&self.ra));
        values.insert("dhcpv6".into(), mode(&self.dhcpv6));
        values.insert("ra_slaac".into(), flag(!self.ra_slaac, "0"));
        let mut options: Vec<String> = old
            .map(|o| o.list("dhcp_option"))
            .unwrap_or_default()
            .into_iter()
            .filter(|o| !o.starts_with("3,") && !o.starts_with("6,"))
            .collect();
        if !self.gateway.is_empty() {
            options.push(format!("3,{}", self.gateway));
        }
        if !self.dns.is_empty() {
            options.push(format!("6,{}", self.dns.join(",")));
        }
        values.insert(
            "dhcp_option".into(),
            match options.is_empty() {
                true => Value::Null,
                false => json!(options),
            },
        );
        values
    }

    /// ipv6_words is how the network's IPv6 is served, in a word or two.
    fn ipv6_words(&self) -> &'static str {
        match (self.ra.as_str(), self.dhcpv6.as_str()) {
            ("disabled", "disabled") => "Off",
            (_, "server") if self.ra_slaac && self.ra != "disabled" => "SLAAC and DHCPv6",
            (_, "server") => "DHCPv6",
            ("relay", _) => "Relayed",
            _ if self.ra_slaac => "SLAAC",
            _ => "Announced",
        }
    }
}

/// range_words states a pool's span in real addresses, under its offsets.
fn range_words(subnet: Option<Subnet>, pool: &Pool) -> String {
    match format::range_label(subnet, &pool.start, &pool.limit).as_str() {
        format::EM_DASH => String::new(),
        span => format!("Hands out {span}."),
    }
}

fn section(title: &str, lede: &str, body: Widget) -> Widget {
    Widget::Section(verso_plugin::SectionWidget {
        title: title.into(),
        sub: lede.into(),
        hairline: true,
        children: vec![body],
        ..Default::default()
    })
}

fn field(name: &str, label: &str, value: &str, help: &str, errors: &Errors) -> Widget {
    form::text_field(name, label, value, help, errors)
}

fn checkbox(name: &str, label: &str, key: &str, help: &str, on: bool, errors: &Errors) -> Widget {
    Widget::Switch(Switch {
        name: name.into(),
        label: label.into(),
        key: key.into(),
        help: help.into(),
        style: "checkbox".into(),
        on,
        error: errors.get(name).into(),
        ..Default::default()
    })
}

fn select(name: &str, label: &str, value: &str, errors: &Errors) -> Widget {
    form::select_field(
        name,
        label,
        value,
        MODES
            .iter()
            .map(|(mode, words)| SelectOption::new(mode, words))
            .collect(),
        errors,
    )
    .writes(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use serde_json::Value as Json;

    fn request(query: &str) -> Request {
        Request {
            path: "/".into(),
            query: Form::parse(query),
            snapshot: fixture::snapshot(),
            ubus: fixture::ubus(),
            plugins: verso_plugin::Plugins::default(),
        }
    }

    fn read(query: &str) -> Json {
        serde_json::to_value(get(&request(query))).expect("serialize")
    }

    /// request_with is a visit to a router whose config `edit` changed first.
    fn request_with(query: &str, edit: impl FnOnce(&mut Json)) -> Request {
        let mut snapshot: Json =
            serde_json::from_str(include_str!("../testdata/snapshot.json")).expect("fixture");
        edit(&mut snapshot);
        Request {
            snapshot: verso_plugin::Snapshot::from_value(snapshot),
            ..request(query)
        }
    }

    /// with_iot gives the router a network with an address of its own and no
    /// server on it yet.
    fn with_iot(snapshot: &mut Json) {
        snapshot["network"]["iot"] = serde_json::json!({".name": "iot", ".type": "interface",
            "proto": "static", "ipaddr": "10.0.30.1", "netmask": "255.255.255.0"});
    }

    fn page_of(r: &Request) -> Json {
        serde_json::to_value(get(r)).expect("serialize")
    }

    fn answer_of(r: &Request, body: &str) -> Json {
        serde_json::to_value(post(r, &Form::parse(body))).expect("serialize")
    }

    fn server_rows(page: &Json) -> Vec<Json> {
        sections(page)[0]["children"][0]["rows"]
            .as_array()
            .unwrap()
            .to_vec()
    }

    fn row_ids(rows: &[Json]) -> Vec<String> {
        rows.iter()
            .filter_map(|r| r["id"].as_str().map(String::from))
            .collect()
    }

    fn answer(query: &str, body: &str) -> Json {
        serde_json::to_value(post(&request(query), &Form::parse(body))).expect("serialize")
    }

    fn sections(page: &Json) -> Vec<Json> {
        page["widget"]["children"].as_array().unwrap().to_vec()
    }

    fn find(v: &Json, wanted: &dyn Fn(&Json) -> bool) -> Option<Json> {
        if wanted(v) {
            return Some(v.clone());
        }
        match v {
            Json::Object(m) => m.values().find_map(|c| find(c, wanted)),
            Json::Array(a) => a.iter().find_map(|c| find(c, wanted)),
            _ => None,
        }
    }

    fn open_drawer(page: &Json) -> Json {
        find(page, &|v| v["open"] == true && v.get("children").is_some()).expect("open drawer")
    }

    fn control(tree: &Json, name: &str) -> Json {
        find(tree, &|v| v["name"] == name && v.get("type").is_some())
            .unwrap_or_else(|| panic!("no control {name}"))
    }

    /// STANDING is lan's drawer resubmitted as it renders.
    const STANDING: &str = "_panel=1&serve=1&start=100&limit=150&leasetime=12h&dns=10.0.0.30&\
ra=server&dhcpv6=server&ra_slaac=1";

    #[test]
    fn the_page_reads_servers_reservations_leases_then_settings() {
        let page = read("");
        assert_eq!(page["title"], "DHCP");
        let titles: Vec<Json> = sections(&page).iter().map(|s| s["title"].clone()).collect();
        assert_eq!(
            titles,
            vec!["Servers", "Reservations", "Leases", "Settings"]
                .into_iter()
                .map(Json::from)
                .collect::<Vec<_>>()
        );
        // Nothing open on a plain visit.
        assert!(find(&page, &|v| v["open"] == true).is_none());
    }

    #[test]
    fn a_server_states_its_range_in_real_addresses_and_how_full_it_is() {
        let page = read("");
        let rows = sections(&page)[0]["children"][0]["rows"].clone();
        let lan = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "lan")
            .unwrap();
        assert_eq!(lan["panel"], "/plugins/dnsdhcp/?open=lan");
        assert_eq!(lan["cells"][0]["href"], lan["panel"]);
        assert_eq!(lan["cells"][1]["text"], "10.0.0.100 – 10.0.0.249");
        // Three of the fixture's leases sit on lan.
        assert_eq!(lan["cells"][2]["text"], "3");
        assert_eq!(lan["cells"][2]["fill"], 2);
        assert_eq!(lan["cells"][3]["text"], "12h");
        assert_eq!(lan["cells"][4]["text"], "SLAAC and DHCPv6");
        // The state reads last, beside the acts it explains.
        let columns = &sections(&page)[0]["children"][0]["columns"];
        assert_eq!(columns[5]["kind"], "status");
        // "SLAAC and DHCPv6" stands on one line, and the range takes the slack.
        assert_eq!(columns[4]["width"], "address");
        assert_eq!(columns[1].get("width"), None);
        // A pool that states no offsets still shows the span dnsmasq would use.
        let guest = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "guest")
            .unwrap();
        assert_eq!(guest["cells"][1]["text"], "10.0.20.100 – 10.0.20.249");
    }

    #[test]
    fn the_servers_card_lists_the_networks_that_have_a_server() {
        // The stock uplink section says "do not serve here" on a network with
        // no address of its own: it is no server. A network with an address
        // and no section has no server either; "Add DHCP server" offers it.
        let page = page_of(&request_with("", with_iot));
        assert_eq!(row_ids(&server_rows(&page)), vec!["guest", "lan"]);
        // A server switched off stays on the list, reading as off, its values
        // intact.
        let off = page_of(&request_with("", |s| {
            s["dhcp"]["lan"]["ignore"] = Json::from("1");
        }));
        let rows = server_rows(&off);
        let lan = rows.iter().find(|r| r["id"] == "lan").unwrap();
        assert_eq!(lan["muted"], true);
        assert_eq!(lan["cells"][5]["text"], "Off");
        let guest = rows.iter().find(|r| r["id"] == "guest").unwrap();
        assert!(guest.get("muted").is_none(), "{guest}");
        // A section set to hand out addresses is a server, even on a network
        // that cannot give it any; its state says why it does not run.
        let misplaced = page_of(&request_with("", |s| {
            s["dhcp"]["wan"]["ignore"] = Json::from("0");
        }));
        assert_eq!(
            row_ids(&server_rows(&misplaced)),
            vec!["guest", "lan", "wan"]
        );
    }

    #[test]
    fn the_page_reads_the_sdks_meaning_of_a_server() {
        // Interfaces reads the same rule, so a network listed here wears the
        // DHCP chip there, and one offered here offers a server there too.
        let r = request_with("", with_iot);
        let servers = dhcp::servers(&r.snapshot, &r.ubus);
        let page = page_of(&r);
        let listed: Vec<String> = servers
            .iter()
            .filter(|s| s.exists())
            .map(|s| s.network.clone())
            .collect();
        assert_eq!(row_ids(&server_rows(&page)), listed);
        let offered: Vec<String> = servers
            .iter()
            .filter(|s| s.can_be_added())
            .map(|s| s.network.clone())
            .collect();
        let blank = page_of(&request_with("open=new-server", with_iot));
        let choices: Vec<String> = control(&blank["act"]["drawer"], "network")["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(choices, offered);
    }

    #[test]
    fn a_router_with_no_server_says_where_one_is_made() {
        let none = |s: &mut Json| {
            let dhcp = s["dhcp"].as_object_mut().unwrap();
            dhcp.retain(|_, section| section[".type"] != "dhcp");
        };
        let empty = &sections(&page_of(&request_with("", none)))[0]["children"][0];
        assert_eq!(empty["rows"], serde_json::json!([]));
        assert_eq!(
            empty["empty_text"],
            "No network has a DHCP server yet. Add one with Add DHCP server."
        );
        // With no network that can take one, the way to one is a network.
        let nowhere = page_of(&request_with("", |s| {
            none(s);
            let network = s["network"].as_object_mut().unwrap();
            network.retain(|name, _| name == "wan" || name == "loopback");
        }));
        assert_eq!(
            sections(&nowhere)[0]["children"][0]["empty_text"],
            "No network has an address of its own to hand out from. Add one in Interfaces."
        );
    }

    #[test]
    fn an_address_opens_the_drawer_it_names() {
        // A server by its network.
        let lan = open_drawer(&read("open=lan"));
        assert_eq!(lan["title"], "lan");
        assert_eq!(control(&lan, "serve")["on"], true);
        assert_eq!(control(&lan, "start")["value"], "100");
        assert_eq!(control(&lan, "ra")["value"], "server");
        assert_eq!(
            control(&lan, "dns")["items"],
            serde_json::json!(["10.0.0.30"])
        );
        // A reservation by its section, or by its device's MAC.
        assert_eq!(open_drawer(&read("open=host_nas"))["title"], "nas");
        assert_eq!(open_drawer(&read("open=30:9c:23:5e:88:01"))["title"], "nas");
        // A blank reservation, filled from a lease.
        let blank = open_drawer(&read("open=new&reserve=42:e6:ad:ff:b7:af"));
        assert_eq!(blank["title"], "New reservation");
        assert_eq!(control(&blank, "ip")["value"], "10.0.0.142");
        // A stale name opens nothing.
        assert!(find(&read("open=nowhere"), &|v| v["open"] == true).is_none());
    }

    #[test]
    fn saving_a_server_writes_only_what_its_drawer_owns_and_keeps_the_rest() {
        assert_eq!(
            answer("open=lan", STANDING)["commit"][0]["values"],
            serde_json::json!({
                "ignore": null, "start": "100", "limit": "150", "leasetime": "12h",
                "dynamicdhcp": null, "force": null, "ra": "server", "dhcpv6": "server",
                "ra_slaac": null, "dhcp_option": ["6,10.0.0.30"]
            })
        );
        let saved = answer(
            "open=lan",
            "_panel=1&serve=1&start=50&limit=100&leasetime=24h&gateway=10.0.0.2&\
             dns=9.9.9.9&ra=disabled&dhcpv6=disabled&reservations_only=1",
        );
        let op = &saved["commit"][0];
        assert_eq!(op["section"], "lan");
        assert_eq!(op["values"]["start"], "50");
        assert_eq!(op["values"]["dynamicdhcp"], "0");
        assert_eq!(op["values"]["ra"], Json::Null);
        assert_eq!(op["values"]["ra_slaac"], "0");
        assert_eq!(
            op["values"]["dhcp_option"],
            serde_json::json!(["3,10.0.0.2", "6,9.9.9.9"])
        );
        // The drawer stays in front of the page while the save stages.
        assert_eq!(open_drawer(&saved)["title"], "lan");
    }

    #[test]
    fn turning_a_server_off_keeps_its_pool_as_written() {
        let off = answer(
            "open=lan",
            "_panel=1&leasetime=12h&ra=server&dhcpv6=server&ra_slaac=1",
        );
        let values = &off["commit"][0]["values"];
        assert_eq!(values["ignore"], "1");
        assert!(values.get("start").is_none());
    }

    #[test]
    fn a_server_the_daemon_would_not_run_stays_refused_in_its_drawer() {
        for (body, field) in [
            ("_panel=1&serve=1&start=200&limit=100&leasetime=12h&ra=server&dhcpv6=server", "start"),
            ("_panel=1&serve=1&start=1&limit=10&leasetime=12h&ra=server&dhcpv6=server", "start"),
            ("_panel=1&serve=1&start=100&limit=50&leasetime=forever&ra=server&dhcpv6=server", "leasetime"),
            ("_panel=1&serve=1&start=100&limit=50&leasetime=12h&gateway=nope&ra=server&dhcpv6=server", "gateway"),
            ("_panel=1&serve=1&start=100&limit=50&leasetime=12h&ra=sometimes&dhcpv6=server", "ra"),
        ] {
            let refused = answer("open=lan", body);
            assert!(refused.get("commit").is_none(), "{body}");
            let drawer = open_drawer(&refused);
            assert!(
                !control(&drawer, field)["error"].as_str().unwrap_or("").is_empty(),
                "{body}: {field} carries no error"
            );
        }
        // The uplink has no server to save, and no address to hand out from.
        let uplink = answer(
            "open=wan",
            "_panel=1&serve=1&leasetime=8h&ra=disabled&dhcpv6=disabled",
        );
        assert!(uplink.get("commit").is_none());
        assert!(find(&uplink, &|v| v["open"] == true).is_none());
    }

    #[test]
    fn the_pages_act_makes_a_server_while_a_network_can_take_one() {
        let page = page_of(&request_with("", with_iot));
        assert_eq!(page["act"]["label"], "Add DHCP server");
        assert_eq!(page["act"]["href"], "/plugins/dnsdhcp/?open=new-server");
        assert_eq!(page["act"]["icon"], "plus");
        assert_eq!(page["act"]["opens_panel"], true);
        // Every network with an address of its own already has a server.
        assert!(read("").get("act").is_none(), "{}", read(""));
    }

    #[test]
    fn a_blank_server_starts_from_the_network_it_serves() {
        let blank = page_of(&request_with("open=new-server", with_iot));
        let drawer = &blank["act"]["drawer"];
        assert_eq!(drawer["title"], "New DHCP server");
        assert_eq!(drawer["open"], true);
        let network = control(drawer, "network");
        assert_eq!(network["value"], "iot");
        // A network's name is a machine string, read in mono.
        assert_eq!(network["datatype"], "uciname");
        let offered: Vec<Json> = network["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].clone())
            .collect();
        assert_eq!(offered, vec![Json::from("iot")]);
        // The Network choice leads; the server's own fields follow it.
        let first = find(drawer, &|v| v.get("name").is_some() && v.get("type").is_some()).unwrap();
        assert_eq!(first["name"], "network");
        assert_eq!(control(drawer, "serve")["on"], true);
        assert_eq!(control(drawer, "start")["value"], "100");
        // An address naming a network with no server opens the same drawer
        // with that network chosen.
        let iot = page_of(&request_with("open=iot", with_iot));
        assert_eq!(iot["act"]["drawer"]["title"], "New DHCP server");
        assert_eq!(control(&iot["act"]["drawer"], "network")["value"], "iot");
        // A stale name, or a blank server with nowhere to serve, opens nothing.
        for query in ["open=nosuch", "open=new-server"] {
            assert!(find(&read(query), &|v| v["open"] == true).is_none(), "{query}");
        }
    }

    #[test]
    fn saving_a_new_server_stages_a_section_named_after_its_network() {
        let r = request_with("open=new-server", with_iot);
        let saved = answer_of(
            &r,
            "_panel=1&network=iot&serve=1&start=100&limit=150&leasetime=12h&ra=disabled&\
             dhcpv6=disabled&ra_slaac=1",
        );
        let op = &saved["commit"][0];
        assert_eq!(op["config"], "dhcp");
        assert_eq!(op["type"], "dhcp");
        assert_eq!(op["section"], "iot");
        assert_eq!(op["values"]["interface"], "iot");
        assert_eq!(op["values"]["start"], "100");
        // The address that chose the network saves the same way.
        let chosen = answer_of(
            &request_with("open=iot", with_iot),
            "_panel=1&network=iot&serve=1&start=100&limit=150&leasetime=12h&ra=disabled&\
             dhcpv6=disabled&ra_slaac=1",
        );
        assert_eq!(chosen["commit"][0]["section"], "iot");
        // A network that already has a server is refused on the Network field.
        let taken = answer_of(
            &r,
            "_panel=1&network=lan&serve=1&start=100&limit=150&leasetime=12h&ra=disabled&\
             dhcpv6=disabled&ra_slaac=1",
        );
        assert!(taken.get("commit").is_none(), "{taken}");
        assert!(!control(&open_drawer(&taken), "network")["error"]
            .as_str()
            .unwrap_or("")
            .is_empty());
        // The pool is checked against the network chosen.
        let outside = answer_of(
            &r,
            "_panel=1&network=iot&serve=1&start=250&limit=100&leasetime=12h&ra=disabled&\
             dhcpv6=disabled",
        );
        assert!(outside.get("commit").is_none());
        assert!(!control(&open_drawer(&outside), "start")["error"]
            .as_str()
            .unwrap_or("")
            .is_empty());
    }

    #[test]
    fn a_server_is_removed_from_its_row() {
        let rows = server_rows(&read(""));
        let lan = rows.iter().find(|r| r["id"] == "lan").unwrap();
        let acts = lan["cells"][6]["actions"].as_array().unwrap();
        let remove = acts.iter().find(|a| a["name"] == "_remove_server").unwrap();
        assert_eq!(remove["value"], "lan");
        assert_eq!(remove["title"], "Remove DHCP server");
        assert_eq!(remove["confirm_title"], "Remove the DHCP server on %s?");
        assert_eq!(
            remove["confirm"],
            "Devices on this network keep their addresses until their leases run out, then get none."
        );
        assert_eq!(remove["confirm_cancel"], "Keep DHCP server");
        let removed = answer("", "_remove_server=lan");
        assert_eq!(
            removed["commit"][0],
            serde_json::json!({"config": "dhcp", "section": "lan", "delete": true})
        );
        assert_eq!(removed["notice"]["text"], "DHCP server removed.");
        let gone = answer("", "_remove_server=nosuch");
        assert!(gone.get("commit").is_none());
        assert_eq!(gone["notice"]["text"], GONE);
    }

    #[test]
    fn a_reservation_saves_from_its_drawer_and_removes_from_its_row() {
        let made = answer(
            "open=new",
            "_panel=1&name=toms-iphone&mac=42:e6:ad:ff:b7:af&ip=10.0.0.142",
        );
        assert_eq!(made["commit"][0]["type"], "host");
        let edited = answer(
            "open=host_nas",
            "_panel=1&name=nas&mac=30:9C:23:5E:88:01&ip=10.0.0.31",
        );
        assert_eq!(edited["commit"][0]["section"], "host_nas");
        let removed = answer("", "_remove=host_thermo");
        assert_eq!(
            removed["commit"][0],
            serde_json::json!({"config": "dhcp", "section": "host_thermo", "delete": true})
        );
        assert!(answer("", "_remove=gone").get("commit").is_none());
    }

    #[test]
    fn the_settings_form_writes_only_what_changed_on_the_daemons_section() {
        let standing = "_settings=1&authoritative=1&readethers=1&leasefile=%2Ftmp%2Fdhcp.leases";
        assert!(answer("", standing).get("commit").is_none());
        let saved = answer("", &format!("{standing}&logdhcp=1&dhcpleasemax=500"));
        assert_eq!(
            saved["commit"][0],
            serde_json::json!({"config": "dhcp", "section": "dnsmasq_main",
                               "values": {"logdhcp": "1", "dhcpleasemax": "500"}})
        );
        let refused = answer("", &format!("{standing}&dhcpleasemax=lots"));
        assert!(refused.get("commit").is_none());
        assert!(!control(&refused, "dhcpleasemax")["error"]
            .as_str()
            .unwrap_or("")
            .is_empty());
    }

    #[test]
    fn a_reservation_is_made_from_the_reservations_card() {
        // The heading line makes what the page is about, a server; the
        // Reservations card makes reservations, on its own header. A still page
        // is scrolled and found in with the browser's own find, so nothing
        // narrows it.
        let page = page_of(&request_with("", with_iot));
        assert_eq!(page["act"]["label"], "Add DHCP server");
        assert_eq!(
            sections(&page)[1]["control"],
            serde_json::json!({"type": "link", "label": "Add reservation", "icon": "plus",
                "href": "/plugins/dnsdhcp/?open=new", "style": "act", "panel": true, "place": true})
        );
        assert!(
            find(&page, &|v| v["type"] == "actionbar").is_none(),
            "{page}"
        );
    }

    #[test]
    fn a_blank_reservation_rides_its_listing() {
        // The page has no heading act here: every network has a server. The
        // blank reservation still opens, riding the listing it will land in.
        let page = read("open=new&reserve=42:e6:ad:ff:b7:af");
        assert!(page.get("act").is_none(), "{page}");
        let rows = sections(&page)[1]["children"][0]["rows"]
            .as_array()
            .unwrap()
            .to_vec();
        let blank = rows.last().unwrap();
        assert!(blank.get("cells").is_none_or(|c| c == &serde_json::json!([])), "{blank}");
        assert_eq!(blank["drawer"]["title"], "New reservation");
        assert_eq!(blank["drawer"]["open"], true);
        assert_eq!(control(&blank["drawer"], "ip")["value"], "10.0.0.142");
        // A plain visit carries no blank.
        let plain = sections(&read(""))[1]["children"][0]["rows"].clone();
        assert!(plain
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("drawer").is_none()));
    }
}
