# SPDX-License-Identifier: GPL-2.0-only
# SPDX-FileCopyrightText: 2026 Mono Technologies Inc.

# verso-plugin-dnsdhcp — build, test and packaging.

# Optional per-machine overrides (gitignored): OPENWRT_DIR, KEY, VERSO_REPO_DIR.
-include local.mk

ID          := dnsdhcp
NAME        := verso-plugin-$(ID)
DESCRIPTION := Verso DNS and DHCP — dnsmasq and odhcpd, as pages
# Useful only where dnsmasq serves the network, and only beside the shell it
# plugs into.
DEPENDS     := verso dnsmasq

CARGO    ?= $(if $(wildcard $(HOME)/.cargo/bin/cargo),$(HOME)/.cargo/bin/cargo,cargo)
BUILDDIR := build

# `make build` cross-compiles every architecture in ARCHES to a static musl
# binary; rust-toolchain.toml provisions the targets and .cargo/config.toml
# links them with rust-lld, so no host cross-gcc is needed.
ARCHES ?= amd64 arm64
rust_target_amd64 := x86_64-unknown-linux-musl
rust_target_arm64 := aarch64-unknown-linux-musl

# The version, <X.Y.Z>-r<N> (scripts/version.sh).
VER := $(shell scripts/version.sh)

# The apk tool and the signing key live under an OpenWrt buildroot.
OPENWRT_DIR ?= $(firstword $(wildcard $(HOME)/Mono/Gateway/openwrt/source))
APK         ?= $(OPENWRT_DIR)/staging_dir/host/bin/apk
KEY         ?= $(OPENWRT_DIR)/private-key.pem

# apk names architectures as OpenWrt does, not as Go and Rust do.
apk_arch_arm64 := aarch64_generic
apk_arch_amd64 := x86_64
APK_GOARCH ?= arm64
APK_ARCH   := $(apk_arch_$(APK_GOARCH))

APK_DIR     := $(BUILDDIR)/apk
APK_PAYLOAD := $(APK_DIR)/pkg
APK_OUT     := $(APK_DIR)/$(NAME)-$(VER).apk
POSTINST    := packaging/post-install.sh

# The apk feed `make apk-publish` adds the package to.
VERSO_REPO_DIR ?= /srv/verso

# build-<arch> is not phony: make skips pattern rules for phony targets.
.PHONY: all build test lint clean version apk apk-preflight apk-publish

all: lint test build

version:
	@echo $(VER)

build: $(addprefix build-,$(ARCHES))

build-%:
	$(CARGO) build --locked --release --target $(rust_target_$*)
	install -Dm755 target/$(rust_target_$*)/release/$(NAME) $(BUILDDIR)/$(NAME)-$*

test:
	$(CARGO) test --locked

lint:
	$(CARGO) clippy --locked --all-targets -- -D warnings

clean:
	rm -rf $(BUILDDIR)

apk-preflight:
	@test -n "$(OPENWRT_DIR)" || { echo "OPENWRT_DIR is unset and no buildroot was auto-detected. Pass OPENWRT_DIR=/path/to/openwrt/source (or set it in local.mk)."; exit 1; }
	@test -x "$(APK)" || { echo "apk tool not found or not executable at: $(APK). Set OPENWRT_DIR or APK=... ."; exit 1; }
	@test -f "$(KEY)" || { echo "signing key not found at: $(KEY). Set OPENWRT_DIR or KEY=... ."; exit 1; }
	@command -v fakeroot >/dev/null || { echo "fakeroot not found — needed to record root:root ownership in the package without sudo."; exit 1; }

# apk builds and signs the package: the binary, rootfs/ as it stands (init
# scripts executable, everything else read-only), and the manifest and catalogs
# where the shell discovers them. The payload is recorded as root:root, which
# ubusd requires of an acl.d file, inside one fakeroot session, so nothing on
# disk is actually root-owned.
apk: apk-preflight build-$(APK_GOARCH)
	rm -rf $(APK_PAYLOAD)
	install -Dm755 $(BUILDDIR)/$(NAME)-$(APK_GOARCH) $(APK_PAYLOAD)/usr/bin/$(NAME)
	cd rootfs && find . -type f | while read -r f; do \
		case "$$f" in ./etc/init.d/*) mode=755 ;; *) mode=644 ;; esac; \
		install -Dm$$mode "$$f" "$(CURDIR)/$(APK_PAYLOAD)/$$f"; \
	done
	install -Dm644 manifest.json $(APK_PAYLOAD)/usr/share/verso/plugins/$(ID)/manifest.json
	for c in i18n/*.json; do install -Dm644 "$$c" "$(APK_PAYLOAD)/usr/share/verso/plugins/$(ID)/$$c"; done
	fakeroot -- sh -c 'chown -R 0:0 "$(APK_PAYLOAD)" && "$(APK)" mkpkg \
	  --info name:$(NAME) --info version:$(VER) --info arch:$(APK_ARCH) \
	  --info "description:$(DESCRIPTION)" \
	  --info license:GPL-2.0-only --info url:https://github.com/we-are-mono/$(NAME) \
	  --info origin:$(NAME) \
	  --info "depends:$(DEPENDS)" \
	  --files "$(APK_PAYLOAD)" \
	  --script post-install:$(POSTINST) \
	  --script post-upgrade:$(POSTINST) \
	  --sign-key "$(KEY)" \
	  --output "$(APK_OUT)"'
	@echo "built and signed: $(APK_OUT)  (arch $(APK_ARCH), version $(VER))"

# apk-publish adds the package to the feed in VERSO_REPO_DIR and re-signs its
# index. It replaces this plugin's earlier packages there and leaves the rest.
apk-publish: apk
	mkdir -p $(VERSO_REPO_DIR)/$(APK_ARCH)
	rm -f $(VERSO_REPO_DIR)/$(APK_ARCH)/$(NAME)-[0-9]*.apk
	cp $(APK_OUT) $(VERSO_REPO_DIR)/$(APK_ARCH)/
	cd $(VERSO_REPO_DIR)/$(APK_ARCH) && "$(APK)" mkndx --allow-untrusted --sign-key "$(KEY)" --output packages.adb *.apk
	chmod -R a+rX $(VERSO_REPO_DIR)
	@echo "published: $(VERSO_REPO_DIR)/$(APK_ARCH)/$(notdir $(APK_OUT))  (index rebuilt)"
