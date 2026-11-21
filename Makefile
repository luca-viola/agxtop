# agxtop — build as your user, install with sudo.
#   make            build
#   make install    build, copy to $(PREFIX)/bin and grant capabilities (asks for sudo)
#   make uninstall  remove it

PREFIX ?= /usr/local
BIN     = $(PREFIX)/bin/agxtop
CAPS    = cap_perfmon,cap_dac_read_search+ep

.PHONY: all build install uninstall test clean

all: build

build:
	cargo build --release

test:
	cargo test --release

install: build
	sudo install -m755 target/release/agxtop $(BIN)
	sudo setcap $(CAPS) $(BIN)
	@getcap $(BIN)

uninstall:
	sudo rm -f $(BIN)

clean:
	cargo clean
