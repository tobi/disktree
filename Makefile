# disktree: build, check, and install.
#
# `make install` puts the binary, a desktop entry and an icon under PREFIX
# (default: ~/.local), so disktree shows up in the Omarchy launcher and in
# "Open with" for directories. The default needs no root:
#
#   make install                         # ~/.local/bin/disktree
#   sudo make install PREFIX=/usr/local  # system-wide
#
# On macOS the same command builds disktree.app into ~/Applications
# (APPLICATIONS=/Applications for everyone) and links the binary into BINDIR.

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
APPDIR ?= $(PREFIX)/share/applications
ICONDIR ?= $(PREFIX)/share/icons/hicolor/scalable/apps

MANIFEST = Cargo.toml
CARGO ?= cargo
TARGET = target/release/disktree
ICON = assets/disktree.svg
DESKTOP = packaging/disktree.desktop.in
PLIST = packaging/Info.plist.in
APP = target/disktree.app
APP_TMP = target/app-build

# macOS gets an app bundle in ~/Applications, plus a `disktree` link in BINDIR
# so it still runs from a shell; everything else gets the XDG layout.
OS := $(shell uname -s)
APPLICATIONS ?= $(HOME)/Applications
ifeq ($(OS),Darwin)
PLATFORM = macos
else
PLATFORM = linux
endif

.PHONY: help build run install uninstall install-linux uninstall-linux \
	install-macos uninstall-macos app lint test ci fmt clean

help:
	@echo "disktree"
	@echo
	@echo "  make build       release build"
	@echo "  make run         build and run, scanning $$HOME"
	@echo "  make install     install for $(PLATFORM) (see top of Makefile)"
	@echo "  make uninstall   remove what install put there"
	@echo "  make app         macOS: build target/disktree.app"
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

install-linux: build
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

uninstall-linux:
	rm -f $(BINDIR)/disktree $(APPDIR)/disktree.desktop $(ICONDIR)/disktree.svg
	@if command -v update-desktop-database >/dev/null 2>&1; then \
	    update-desktop-database $(APPDIR) 2>/dev/null || true; \
	fi
	@echo "removed"

install: install-$(PLATFORM)

uninstall: uninstall-$(PLATFORM)

install-macos: app
	install -d $(APPLICATIONS) $(BINDIR)
	rm -rf $(APPLICATIONS)/disktree.app
	cp -R $(APP) $(APPLICATIONS)/disktree.app
	ln -sf $(APPLICATIONS)/disktree.app/Contents/MacOS/disktree \
	    $(BINDIR)/disktree
	@echo
	@echo "installed:"
	@echo "  $(APPLICATIONS)/disktree.app"
	@echo "  $(BINDIR)/disktree -> the app's binary"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; *) \
	    echo; echo "note: $(BINDIR) is not on PATH in this shell";; esac

uninstall-macos:
	rm -rf $(APPLICATIONS)/disktree.app
	rm -f $(BINDIR)/disktree
	@echo "removed"

# macOS: a self-contained bundle. Quick Look renders an SVG at its intrinsic
# size, so the icon is re-sized to 1024 before rasterising, or the mark would
# land in the corner of a blank canvas.
app: build
	rm -rf $(APP) $(APP_TMP)
	mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources \
	    $(APP_TMP)/disktree.iconset
	install -m755 $(TARGET) $(APP)/Contents/MacOS/disktree
	sed 's/width="64" height="64"/width="1024" height="1024"/' $(ICON) \
	    > $(APP_TMP)/icon.svg
	qlmanage -t -s 1024 -o $(APP_TMP) $(APP_TMP)/icon.svg >/dev/null 2>&1
	for s in 16 32 128 256 512; do \
	    sips -z $$s $$s $(APP_TMP)/icon.svg.png \
	        --out $(APP_TMP)/disktree.iconset/icon_$${s}x$${s}.png >/dev/null; \
	    d=$$((s * 2)); \
	    sips -z $$d $$d $(APP_TMP)/icon.svg.png \
	        --out $(APP_TMP)/disktree.iconset/icon_$${s}x$${s}@2x.png \
	        >/dev/null; \
	done
	iconutil -c icns $(APP_TMP)/disktree.iconset \
	    -o $(APP)/Contents/Resources/disktree.icns
	VERSION=$$(sed -n 's/^version = "\(.*\)"/\1/p' $(MANIFEST) | head -1) && \
	sed -e "s|@VERSION@|$$VERSION|" $(PLIST) > $(APP)/Contents/Info.plist
	rm -rf $(APP_TMP)
	@echo "built $(APP) — copy it to /Applications to install"

clean:
	$(CARGO) clean
