#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# SPDX-FileCopyrightText: 2026 Mono Technologies Inc.
#
# apk post-deinstall hook for the DNS/DHCP plugin: the user its service added
# for itself goes with it, and the shell re-reads the manifests (SIGHUP) so
# its pages and nav rows go too. The verso group is the shell package's.
sed -i '/^verso-plugin-dnsdhcp:/d' /etc/passwd
ubus call service signal '{"name":"verso","signal":1}' 2>/dev/null
exit 0
