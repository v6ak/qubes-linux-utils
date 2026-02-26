BINDIR ?= /usr/bin

# Build in release mode by default; can be overridden with DEBUG=1.
ifdef DEBUG
CARGO_FLAGS = --no-default-features
CARGO_TARGET = debug
else
CARGO_FLAGS = --release
CARGO_TARGET = release
endif

# Build offline using crates installed by DNF/APT (from /usr/share/cargo/registry).
# The .cargo/config.toml in this directory configures cargo to use that system
# registry.  Set ONLINE=1 to allow fetching from crates.io (for development on
# machines that do not have the system crate packages installed).
ifndef ONLINE
CARGO_FLAGS += --offline
endif

BINARY = target/$(CARGO_TARGET)/qubes-meminfo-writer-ng

.PHONY: all install clean check

all: $(BINARY)

$(BINARY): src/main.rs src/xenstore.rs Cargo.toml
	cargo build $(CARGO_FLAGS)

check:
	cargo test --no-default-features --offline

install: $(BINARY)
	install -D $(BINARY) $(DESTDIR)/$(BINDIR)/qubes-meminfo-writer-ng
	install -D -m 0644 meminfo-writer-ng.conf \
		$(DESTDIR)/etc/qubes/meminfo-writer-ng.conf
ifeq (1,${DEBIANBUILD})
	install -d $(DESTDIR)/lib/systemd/system/
	install -m 0644 qubes-meminfo-writer-ng.service \
		$(DESTDIR)/lib/systemd/system/
else
	install -d $(DESTDIR)/usr/lib/systemd/system/
	install -m 0644 qubes-meminfo-writer-ng.service \
		qubes-meminfo-writer-ng-dom0.service \
		$(DESTDIR)/usr/lib/systemd/system/
endif

clean:
	cargo clean
