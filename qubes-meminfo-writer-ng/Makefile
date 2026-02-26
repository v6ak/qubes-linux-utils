BINDIR ?= /usr/bin

# Build in release mode by default; can be overridden with DEBUG=1.
ifdef DEBUG
CARGO_FLAGS =
CARGO_TARGET = debug
else
CARGO_FLAGS = --release
CARGO_TARGET = release
endif

BINARY = target/$(CARGO_TARGET)/qubes-meminfo-writer-ng

.PHONY: all install clean check

all: $(BINARY)

$(BINARY): src/main.rs src/xenstore.rs Cargo.toml
	cargo build $(CARGO_FLAGS)

check:
	cargo test $(CARGO_FLAGS)

install: $(BINARY)
	install -D $(BINARY) $(DESTDIR)/$(BINDIR)/qubes-meminfo-writer-ng
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
