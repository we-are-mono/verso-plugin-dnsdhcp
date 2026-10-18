# verso-plugin-dnsdhcp

DNS and DHCP for [Verso](https://github.com/we-are-mono/verso), the web
interface for OpenWrt: the DHCP and DNS pages, over dnsmasq and odhcpd, and a
Reserved address tab on every device's panel.

It is a non-core Verso plugin: a package of its own, installed only where
dnsmasq serves the network. Once installed its pages join the Network section
of Verso's sidebar; removed, they leave it. Like every Verso plugin it runs as
its own process, describes its pages with Verso's widgets, and reads and writes
the router only through rpcd and the access list it ships.

## Root half

What only root can see is read by the plugin's root half: this plugin's own
binary run as root by Verso's helper, for the commands the package registers in
`/usr/share/verso/plugins/dnsdhcp/commands.json` (Verso ADR-027):

- `dhcp-state`, a read: each network's DHCP server as dnsmasq loaded it;
- `dns-state`, a read: what the resolver can do and which program answers each
  local port;
- `files`, the `dnsmasq` file family's transform: a listing shows the files as
  they are, and a save is checked by dnsmasq itself before it is staged.

### `dnsdhcp.dhcp-state`, version 1

`{"networks": {"lan": {"enabled": true, "state": "running", "pool": "192.168.1.100–192.168.1.249", "lease_time": "12h", "leases": 12}}}`

- **Sources.** procd's service list, the ranges dnsmasq generated, netifd and
  the unexpired leases.
- **`enabled`** comes from the applied configuration, not the operator's staged
  changes.
- **`state`** is `running`, `warning`, `stopped` or `disabled`; an optional
  `reason` tells a failure, an unavailable reading and a full pool apart.
- **`leases`** is absent when it could not be counted, never zero.

The Interfaces plugin reads it too, through the SDK's `dhcp::servers`.

### `dnsdhcp.dns-state`, version 1

`{"resolver": true, "dnssec": true, "doh": false, "adblock": false, "listeners": {"5053": "AdGuardHome"}}`

- **`resolver`, `doh`, `adblock`.** Whether dnsmasq, https-dns-proxy and
  adblock are installed.
- **`dnssec`.** Whether the installed dnsmasq is built with DNSSEC.
- **`listeners`.** Each UDP port a program answers on, for loopback or every
  address, with that program's name: what a `127.0.0.1#port` upstream really
  is.

### The `dnsmasq` family's files

dnsmasq's custom option files are `/etc/dnsmasq.conf` and `.conf` files in
`/etc/dnsmasq.d`, listed through Verso's `configFiles` read as they will read
once staged changes are applied. Saving one stages it through Verso's
`stageConfigFile`; dnsmasq checks the options first (`dnsmasq --test`) and a
file it would not start with is refused. A file saved in `/etc/dnsmasq.d` also
stages that folder as each resolver's `confdir`, which dnsmasq's jail must
mount; a resolver that reads another folder refuses the save.

## Install

On a router running Verso, from the Verso package feed:

```sh
apk update
apk add verso-plugin-dnsdhcp
```

The package depends on `verso` and `dnsmasq`.

## Build

The plugin is a static Rust binary built on Verso's plugin SDK, which Cargo
reads from the Verso checkout beside it (`../verso-plugin-sdk`).
`rust-toolchain.toml` provisions the compiler and both musl targets; nothing
else is needed but `rustup` and `make`.

```sh
make test     # unit and page tests
make lint     # clippy, warnings as errors
make build    # build/verso-plugin-dnsdhcp-{amd64,arm64}
make apk      # a signed package for the router (needs an OpenWrt buildroot)
```

`make apk` takes the apk tool and the signing key from an OpenWrt buildroot:
pass `OPENWRT_DIR=/path/to/openwrt/source`, or set it in a gitignored
`local.mk`. The version is the latest `vX.Y.Z` tag and the commits on it
(`make version`).

## Layout

- `src/` — the plugin: its pages, the router reads behind them, and tests
- `manifest.json` — what the shell discovers: the plugin's pages and its socket
- `i18n/` — its translations, one catalog per language
- `rootfs/` — what the package installs as it stands: the init script, the
  rpcd access list and the command registry
- `packaging/post-install.sh` — enables and starts the plugin's service

## License

GPL-2.0-only. See `LICENSE`.
