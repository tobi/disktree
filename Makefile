# disktree: build, check, and install.
#
# `make install` puts the binary, a desktop entry and an icon under PREFIX
# (default: ~/.local), so disktree shows up in the Omarchy launcher and in
# "Open with" for directories. The default needs no root:
#
#   make install                         # ~/.local/bin/disktree
#   sudo make install PREFIX=/usr/local  # system-wide
#
# On macOS it installs disktree.app into ~/Applications instead (APPS to
# change that), plus a `disktree` command in BINDIR that runs the app's
# binary, so both Spotlight and a terminal find it.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications
ICONDIR ?= $(PREFIX)/share/icons/hicolor/scalable/apps

APPS ?= $(HOME)/Applications
BUNDLE = target/bundle/disktree.app
UNAME := $(shell uname -s)

MANIFEST = Cargo.toml
CARGO ?= cargo
TARGET = target/release/disktree
ifeq ($(UNAME),Linux)
TARGET = target/portable/disktree
endif
ICON = assets/disktree.svg
DESKTOP = packaging/disktree.desktop.in

# Being on PATH is not enough: an older package can win earlier in it.
define check_install_path
	@resolved=$$(command -v $(1) 2>/dev/null || true); \
	if [ -n "$$resolved" ] && [ "$$resolved" -ef "$(BINDIR)/$(1)" ]; then \
	    :; \
	else \
	    echo; \
	    if [ -n "$$resolved" ]; then \
	        echo "warning: $(1) still resolves to $$resolved"; \
	        echo "         that hides the build in $(BINDIR)"; \
	    else \
	        echo "warning: $(BINDIR) is not on PATH"; \
	    fi; \
	    printf 'Run in your shell: export PATH="%s:$$PATH"\n' "$(BINDIR)"; \
	    echo "Then add that PATH setting to your shell configuration."; \
	fi
endef

.PHONY: help build run install install-scan uninstall bundle dock-icons lint test test-ssh ci fmt clean

help:
	@echo "disktree"
	@echo
	@echo "  make build       release build"
	@echo "  make run         build and open the source/root chooser"
	@echo "  make install-scan  build/install the headless SSH helper only"
	@echo "  make install     install to $(PREFIX): binary, desktop entry, icon"
	@echo "                   (macOS: disktree.app into $(APPS))"
	@echo "  make bundle      macOS: build target/bundle/disktree.app and its zip"
	@echo "  make dock-icons  macOS 26: redraw the Dock icon of an unbundled run"
	@echo "  make uninstall   remove what install put there"
	@echo "  make lint        rustfmt --check and clippy -D warnings"
	@echo "  make test        core and window-harness tests"
	@echo "  make test-ssh    real localhost SSH tests (Linux: bun, openssh)"
	@echo "  make ci          lint, then test"
	@echo "  make fmt         format in place"
	@echo "  make clean       cargo clean"

# Always ask cargo: it is incremental and knows every source file, where a
# make file-target would only compare the binary against the manifest and
# happily install a stale build.
build:
ifeq ($(UNAME),Linux)
	$(CARGO) xtask portable
else
	$(CARGO) build --release
endif

# For servers: no desktop, GPU, GPUI build, or elevated permissions needed.
install-scan:
	$(CARGO) build --release -p disktree-scan
	install -d "$(BINDIR)"
	install -m755 target/release/disktree-scan "$(BINDIR)/disktree-scan"
	$(call check_install_path,disktree-scan)

run: build
	$(TARGET)

lint:
	$(CARGO) xtask lint

test:
	$(CARGO) xtask test

# Isolated keys and high localhost port; never uses a real server or root.
test-ssh:
	bun scripts/test-ssh.ts

ci: lint test

fmt:
	$(CARGO) xtask fmt-fix

ifeq ($(UNAME),Darwin)
# Replaced whole rather than copied over: a stale file left inside a signed
# bundle breaks its signature.
install:
	$(CARGO) xtask bundle
	install -d "$(APPS)" "$(BINDIR)"
	rm -rf "$(APPS)/disktree.app"
	ditto "$(BUNDLE)" "$(APPS)/disktree.app"
	ln -sf "$(APPS)/disktree.app/Contents/MacOS/disktree" "$(BINDIR)/disktree"
	@echo
	@echo "installed:"
	@echo "  $(APPS)/disktree.app"
	@echo "  $(BINDIR)/disktree -> the app's binary"
	$(call check_install_path,disktree)

uninstall:
	rm -rf "$(APPS)/disktree.app"
	@if [ -L "$(BINDIR)/disktree" ]; then rm -f "$(BINDIR)/disktree"; fi
	@echo "removed"
else
install: build
	install -d $(BINDIR) $(APPDIR) $(ICONDIR)
	install -m755 $(TARGET) $(BINDIR)/disktree
	install -m644 $(ICON) $(ICONDIR)/disktree.svg
	VERSION=$$(sed -n 's/^version = "\(.*\)"/\1/p' $(MANIFEST) | head -1) && \
	sed -e 's|@BINDIR@|$(BINDIR)|' -e "s|@VERSION@|$$VERSION|" \
	    $(DESKTOP) > $(APPDIR)/disktree.desktop && \
	chmod 644 $(APPDIR)/disktree.desktop
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo
	@echo "installed:"
	@echo "  $(BINDIR)/disktree"
	@echo "  $(APPDIR)/disktree.desktop"
	@echo "  $(ICONDIR)/disktree.svg"
	@if command -v desktop-file-validate >/dev/null 2>&1; then \
	    desktop-file-validate $(APPDIR)/disktree.desktop || true; \
	fi
	$(call check_install_path,disktree)

uninstall:
	rm -f $(BINDIR)/disktree $(APPDIR)/disktree.desktop $(ICONDIR)/disktree.svg
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo "removed"
endif

bundle:
	$(CARGO) xtask bundle

# The Dock icon a run outside the bundle shows, in the flat look of macOS 15
# and the Liquid Glass of macOS 26, which only that macOS can draw: needs
# macOS 26 and Xcode 26. Run after either SVG in packaging/macos changes.
dock-icons:
	swift packaging/macos/render-dock-icons.swift

clean:
	$(CARGO) clean
