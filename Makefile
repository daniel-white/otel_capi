# Standard build entrypoint. Nix is only used to provide the Rust toolchain
# and C compilers: run `nix develop --command make check` from a fresh shell.

PREFIX ?= /usr/local
DESTDIR ?=

GCC ?= gcc
CLANG ?= clang
MUSL_GCC ?= musl-gcc
CC ?= $(GCC)
CARGO ?= cargo
CARGO_MIRI ?= cargo +nightly miri
CARGO_LLVM_COV ?= cargo llvm-cov
# The core tests read the real clock and process environment.
MIRIFLAGS ?= -Zmiri-disable-isolation
CLIPPY_LINTS := -W clippy::undocumented_unsafe_blocks

BUILD_DIR := build/artifacts
COVERAGE_DIR := build/coverage
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
# Expose statically linked core symbols so dlsym-based transport registration
# and allocation tracking work in the kitchen-sink example.
EXAMPLE_EXPORT_LDFLAGS := -Wl,-export_dynamic
else
SHARED_EXT := so
EXAMPLE_EXPORT_LDFLAGS := -rdynamic
endif
SHARED_LIB := target/release/libotel_capi.$(SHARED_EXT)
GRPC_SHARED_LIB := target/release/libotel_capi_grpc_transport.$(SHARED_EXT)
HTTP_SHARED_LIB := target/release/libotel_capi_http_transport.$(SHARED_EXT)
STDOUT_SHARED_LIB := target/release/libotel_capi_stdout_transport.$(SHARED_EXT)
PKG_CONFIG_FILE := $(BUILD_DIR)/otel-capi.pc

.PHONY: all build test fmt fmt-check clippy lint miri coverage docs example-gcc example-clang example-musl compile-checks transport-checks pkg-config-file check clean install

all: build

build:
	$(CARGO) build --release -p otel-capi
	$(CARGO) build --release -p otel-capi-grpc-transport
	$(CARGO) build --release -p otel-capi-http-transport
	$(CARGO) build --release -p otel-capi-stdout-transport

test:
	$(CARGO) test --workspace --lib

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets --locked -- $(CLIPPY_LINTS)

lint: fmt-check clippy

miri:
	$(CARGO_MIRI) setup
	MIRIFLAGS="$(MIRIFLAGS)" $(CARGO_MIRI) test --workspace --lib

# Miri interprets MIR and cannot emit LLVM coverage data, so validate the same
# tests in every crate with Miri first, then measure them with instrumented
# test binaries.
coverage: miri | $(BUILD_DIR)
	mkdir -p $(COVERAGE_DIR)
	$(CARGO_LLVM_COV) --locked --workspace --lib --no-report
	$(CARGO_LLVM_COV) report --summary-only --ignore-filename-regex 'build\.rs'
	$(CARGO_LLVM_COV) report --fail-under-lines 90 --lcov --ignore-filename-regex 'build\.rs' --output-path $(COVERAGE_DIR)/lcov.info
	$(CARGO_LLVM_COV) report --html --ignore-filename-regex 'build\.rs' --output-dir $(COVERAGE_DIR)

docs: build | $(BUILD_DIR)
	python3 scripts/generate_docs.py --out-dir $(DOCS_DIR)

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

example-gcc: build | $(BUILD_DIR)
	$(GCC) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(EXAMPLE_EXPORT_LDFLAGS) $(LDLIBS) \
		-o $(BUILD_DIR)/kitchen-sink-gcc
	$(EXAMPLE_CHECK) $(BUILD_DIR)/kitchen-sink-gcc $(BUILD_DIR)/stdout-gcc.log

example-clang: build | $(BUILD_DIR)
	$(CLANG) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(EXAMPLE_EXPORT_LDFLAGS) $(LDLIBS) \
		-o $(BUILD_DIR)/kitchen-sink-clang
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

transport-checks: build | $(BUILD_DIR)
	@test -f $(GRPC_SHARED_LIB)
	@nm -g $(GRPC_SHARED_LIB) | grep -q 'otel_grpc_transport_send'
	@nm -g $(GRPC_SHARED_LIB) | grep -q 'otel_grpc_transport_new'
	@nm -g $(GRPC_SHARED_LIB) | grep -q 'otel_grpc_transport_register'
	@nm -g $(GRPC_SHARED_LIB) | grep -q 'otel_grpc_transport_release'
	@test -f $(HTTP_SHARED_LIB)
	@nm -g $(HTTP_SHARED_LIB) | grep -q 'otel_http_transport_send'
	@nm -g $(HTTP_SHARED_LIB) | grep -q 'otel_http_transport_new'
	@nm -g $(HTTP_SHARED_LIB) | grep -q 'otel_http_transport_register'
	@nm -g $(HTTP_SHARED_LIB) | grep -q 'otel_http_transport_release'
	@test -f $(STDOUT_SHARED_LIB)
	@nm -g $(STDOUT_SHARED_LIB) | grep -q 'otel_stdout_transport_send'
	@nm -g $(STDOUT_SHARED_LIB) | grep -q 'otel_stdout_transport_new'
	@nm -g $(STDOUT_SHARED_LIB) | grep -q 'otel_stdout_transport_register'
	@nm -g $(STDOUT_SHARED_LIB) | grep -q 'otel_stdout_transport_release'
	@for lib in $(GRPC_SHARED_LIB) $(HTTP_SHARED_LIB) $(STDOUT_SHARED_LIB); do \
		if nm -g $$lib | grep -Eq ' T otel_(runtime_|handle_|live_handle_count|ensure_all_released)'; then \
			echo "$$lib exports core symbols"; exit 1; \
		fi; \
	done
	@echo "loadable transport exports verified"

check: test lint example-gcc example-clang compile-checks transport-checks

install: build pkg-config-file
	install -d $(DESTDIR)$(PREFIX)/include $(DESTDIR)$(PREFIX)/lib $(DESTDIR)$(PREFIX)/lib/pkgconfig
	install -m 644 $(HEADERS) $(DESTDIR)$(PREFIX)/include/
	install -m 644 $(STATIC_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(GRPC_SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(HTTP_SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(STDOUT_SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(PKG_CONFIG_FILE) $(DESTDIR)$(PREFIX)/lib/pkgconfig/

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR) $(DOCS_DIR)
