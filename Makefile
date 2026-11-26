# Standard build entrypoint. Nix is only used to provide the Rust toolchain
# and C compilers: run `nix develop --command make check` from a fresh shell.

PREFIX ?= /usr/local
DESTDIR ?=

GCC ?= gcc
CLANG ?= clang
CC ?= $(GCC)
CARGO ?= cargo

BUILD_DIR := build/artifacts
INCLUDE_DIR := include
HEADERS = $(wildcard $(INCLUDE_DIR)/otel_capi*.h)
STATIC_LIB := target/release/libotel_capi.a
EXAMPLE_SRC := examples/kitchen-sink/main.c

CFLAGS ?= -std=c11 -Wall -Wextra -Werror
CPPFLAGS ?= -I$(INCLUDE_DIR)
LDFLAGS ?=
LDLIBS ?= -pthread -ldl -lm
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
SHARED_EXT := dylib
else
SHARED_EXT := so
endif
SHARED_LIB := target/release/libotel_capi.$(SHARED_EXT)
PKG_CONFIG_FILE := $(BUILD_DIR)/otel-capi.pc

.PHONY: all build test example-gcc example-clang compile-checks pkg-config-file check clean install

all: build

build:
	$(CARGO) build --release

test:
	$(CARGO) test --lib

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

example-gcc: build | $(BUILD_DIR)
	$(GCC) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-gcc
	OTEL_CAPI_EXPORTER=none $(BUILD_DIR)/kitchen-sink-gcc
	OTEL_CAPI_EXPORTER=stdout $(BUILD_DIR)/kitchen-sink-gcc > $(BUILD_DIR)/stdout-gcc.log
	grep -q 'all signals completed' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'all handles released' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'trace_id=' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'worker 3 complete' $(BUILD_DIR)/stdout-gcc.log
	grep -q ' ->  items: Int(3)' $(BUILD_DIR)/stdout-gcc.log
	grep -q ' ->  component: String' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'extracted tenant=acme' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'views: traceparent=55 tracestate=0 baggage=11 tenant=4' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'numeric ID helpers ok' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'kitchen-sink.remote-child.updated' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'Links:' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'EventName: "kitchen.sink.warning"' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'Target (Scope): "kitchen-sink.structured"' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'SeverityText: "CRITICAL"' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'structured warning' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'trace message' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'fatal message' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'length-aware info' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'fatal record arr n' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'semconv helpers ok' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'kitchen_sink.observed_temperature' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'kitchen_sink.observed_requests' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'source: callback' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'observer.kind: array' $(BUILD_DIR)/stdout-gcc.log

example-clang: build | $(BUILD_DIR)
	$(CLANG) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-clang
	OTEL_CAPI_EXPORTER=none $(BUILD_DIR)/kitchen-sink-clang
	OTEL_CAPI_EXPORTER=stdout $(BUILD_DIR)/kitchen-sink-clang > $(BUILD_DIR)/stdout-clang.log
	grep -q 'all signals completed' $(BUILD_DIR)/stdout-clang.log
	grep -q 'all handles released' $(BUILD_DIR)/stdout-clang.log
	grep -q 'trace_id=' $(BUILD_DIR)/stdout-clang.log
	grep -q 'worker 3 complete' $(BUILD_DIR)/stdout-clang.log
	grep -q ' ->  items: Int(3)' $(BUILD_DIR)/stdout-clang.log
	grep -q ' ->  component: String' $(BUILD_DIR)/stdout-clang.log
	grep -q 'extracted tenant=acme' $(BUILD_DIR)/stdout-clang.log
	grep -q 'views: traceparent=55 tracestate=0 baggage=11 tenant=4' $(BUILD_DIR)/stdout-clang.log
	grep -q 'numeric ID helpers ok' $(BUILD_DIR)/stdout-clang.log
	grep -q 'kitchen-sink.remote-child.updated' $(BUILD_DIR)/stdout-clang.log
	grep -q 'Links:' $(BUILD_DIR)/stdout-clang.log
	grep -q 'EventName: "kitchen.sink.warning"' $(BUILD_DIR)/stdout-clang.log
	grep -q 'Target (Scope): "kitchen-sink.structured"' $(BUILD_DIR)/stdout-clang.log
	grep -q 'SeverityText: "CRITICAL"' $(BUILD_DIR)/stdout-clang.log
	grep -q 'structured warning' $(BUILD_DIR)/stdout-clang.log
	grep -q 'trace message' $(BUILD_DIR)/stdout-clang.log
	grep -q 'fatal message' $(BUILD_DIR)/stdout-clang.log
	grep -q 'length-aware info' $(BUILD_DIR)/stdout-clang.log
	grep -q 'fatal record arr n' $(BUILD_DIR)/stdout-clang.log
	grep -q 'semconv helpers ok' $(BUILD_DIR)/stdout-clang.log
	grep -q 'kitchen_sink.observed_temperature' $(BUILD_DIR)/stdout-clang.log
	grep -q 'kitchen_sink.observed_requests' $(BUILD_DIR)/stdout-clang.log
	grep -q 'source: callback' $(BUILD_DIR)/stdout-clang.log
	grep -q 'observer.kind: array' $(BUILD_DIR)/stdout-clang.log

compile-checks: build | $(BUILD_DIR)
	$(CC) $(CFLAGS) $(CPPFLAGS) -fsyntax-only checks/compile-pass/*.c
	@for src in checks/compile-fail/*.c; do \
		if $(CC) $(CFLAGS) $(CPPFLAGS) -fsyntax-only $$src >/dev/null 2>&1; then \
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
	rm -rf $(BUILD_DIR)
