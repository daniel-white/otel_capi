# Standard build entrypoint. Nix is only used to provide the Rust toolchain
# and C compilers: run `nix develop --command make check` from a fresh shell.

PREFIX ?= /usr/local
DESTDIR ?=

GCC ?= gcc
CLANG ?= clang
MUSL_GCC ?= musl-gcc
CC ?= $(GCC)
CARGO ?= cargo

BUILD_DIR := build/artifacts
DOCS_DIR := build/docs
INCLUDE_DIR := include
HEADERS = $(wildcard $(INCLUDE_DIR)/otel_capi*.h)
STATIC_LIB := target/release/libotel_capi.a
MUSL_STATIC_LIB ?= target/x86_64-unknown-linux-musl/release/libotel_capi.a
EXAMPLE_SRC := examples/kitchen-sink/main.c
EXAMPLE_CHECK := scripts/check_kitchen_sink.sh

CFLAGS ?= -std=c11 -Wall -Wextra -Werror
CPPFLAGS ?= -I$(INCLUDE_DIR)
LDFLAGS ?=
LDLIBS ?= -pthread -ldl -lm
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
VERSION_PARTS := $(shell printf '%s\n' '$(VERSION)' | sed -E 's/^([0-9]+)\.([0-9]+)\.([0-9]+).*/\1 \2 \3/')
VERSION_MAJOR := $(word 1,$(VERSION_PARTS))
VERSION_MINOR := $(word 2,$(VERSION_PARTS))
VERSION_PATCH := $(word 3,$(VERSION_PARTS))
VERSION_CPPFLAGS := $(CPPFLAGS) \
	-DOTEL_EXPECTED_VERSION_MAJOR=$(VERSION_MAJOR) \
	-DOTEL_EXPECTED_VERSION_MINOR=$(VERSION_MINOR) \
	-DOTEL_EXPECTED_VERSION_PATCH=$(VERSION_PATCH)

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
SHARED_EXT := dylib
else
SHARED_EXT := so
endif
SHARED_LIB := target/release/libotel_capi.$(SHARED_EXT)
PKG_CONFIG_FILE := $(BUILD_DIR)/otel-capi.pc

.PHONY: all build test docs example-gcc example-clang example-musl compile-checks pkg-config-file check clean install

all: build

build:
	$(CARGO) build --release

test:
	$(CARGO) test --lib

docs: build | $(BUILD_DIR)
	python3 scripts/generate_docs.py --out-dir $(DOCS_DIR)

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

example-gcc: build | $(BUILD_DIR)
	$(GCC) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-gcc
	$(EXAMPLE_CHECK) $(BUILD_DIR)/kitchen-sink-gcc $(BUILD_DIR)/stdout-gcc.log

example-clang: build | $(BUILD_DIR)
	$(CLANG) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-clang
	$(EXAMPLE_CHECK) $(BUILD_DIR)/kitchen-sink-clang $(BUILD_DIR)/stdout-clang.log

example-musl: | $(BUILD_DIR)
	$(MUSL_GCC) $(CFLAGS) -static $(CPPFLAGS) $(EXAMPLE_SRC) $(MUSL_STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-musl
	$(EXAMPLE_CHECK) $(BUILD_DIR)/kitchen-sink-musl $(BUILD_DIR)/stdout-musl.log

compile-checks: build | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(VERSION_CPPFLAGS) -fsyntax-only checks/compile-pass/*.c
	@for src in checks/compile-fail/*.c; do \
		if $(CC) $(CFLAGS) $(VERSION_CPPFLAGS) -fsyntax-only $$src >/dev/null 2>&1; then \
			echo "unexpectedly compiled $$src"; exit 1; \
		fi; \
	done
	@echo "compile-time type checks passed"

pkg-config-file: $(PKG_CONFIG_FILE)

$(PKG_CONFIG_FILE): otel-capi.pc.in Makefile | $(BUILD_DIR)
	sed -e 's|@PREFIX@|$(PREFIX)|g' -e 's|@VERSION@|$(VERSION)|g' $< > $@

check: test example-gcc example-clang compile-checks

install: build pkg-config-file
	install -d $(DESTDIR)$(PREFIX)/include $(DESTDIR)$(PREFIX)/lib $(DESTDIR)$(PREFIX)/lib/pkgconfig
	install -m 644 $(HEADERS) $(DESTDIR)$(PREFIX)/include/
	install -m 644 $(STATIC_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(PKG_CONFIG_FILE) $(DESTDIR)$(PREFIX)/lib/pkgconfig/

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR) $(DOCS_DIR)
