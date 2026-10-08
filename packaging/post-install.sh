#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# SPDX-FileCopyrightText: 2026 Mono Technologies Inc.
#
# apk post-install / post-upgrade hook for the DNS/DHCP plugin. The shell package
# owns the verso user and group; this one brings its own service up, then has
# the shell re-read the manifests (SIGHUP) so the pages and nav rows appear.
/etc/init.d/verso-plugin-dnsdhcp enable 2>/dev/null
/etc/init.d/verso-plugin-dnsdhcp restart 2>/dev/null
ubus call service signal '{"name":"verso","signal":1}' 2>/dev/null
exit 0
