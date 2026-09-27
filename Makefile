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
ICON = assets/disktree.svg
DESKTOP = packaging/disktree.desktop.in

.PHONY: help build run install uninstall bundle lint test ci fmt clean

help:
	@echo "disktree"
	@echo
	@echo "  make build       release build"
	@echo "  make run         build and run, scanning $$HOME"
	@echo "  make install     install to $(PREFIX): binary, desktop entry, icon"
	@echo "                   (macOS: disktree.app into $(APPS))"
	@echo "  make bundle      macOS: build target/bundle/disktree.app and its zip"
	@echo "  make uninstall   remove what install put there"
	@echo "  make lint        rustfmt --check and clippy -D warnings"
	@echo "  make test        core and window-harness tests"
	@echo "  make ci          lint, then test"
	@echo "  make fmt         format in place"
	@echo "  make clean       cargo clean"

# Always ask cargo: it is incremental and knows every source file, where a
# make file-target would only compare the binary against the manifest and
# happily install a stale build.
build:
	$(CARGO) build --release

run: build
	$(TARGET)

lint:
	$(CARGO) xtask lint

test:
	$(CARGO) xtask test

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
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) \
	    echo; echo "note: $(BINDIR) is not on PATH in this shell";; esac

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
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) \
	    echo; echo "note: $(BINDIR) is not on PATH in this shell";; esac

uninstall:
	rm -f $(BINDIR)/disktree $(APPDIR)/disktree.desktop $(ICONDIR)/disktree.svg
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo "removed"
endif

bundle:
	$(CARGO) xtask bundle

clean:
	$(CARGO) clean
