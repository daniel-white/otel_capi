# Standard build entrypoint. Nix is only used to provide the Rust toolchain
# and C compilers: run `nix develop --command make check` from a fresh shell.

PREFIX ?= /usr/local
DESTDIR ?=

GCC ?= gcc
CLANG ?= clang
CARGO ?= cargo

BUILD_DIR := build/artifacts
INCLUDE_DIR := include
HEADERS := $(INCLUDE_DIR)/cotel.h $(INCLUDE_DIR)/cotel_semconv.h
STATIC_LIB := target/release/libcotel.a
EXAMPLE_SRC := examples/kitchen-sink/main.c

CFLAGS ?= -std=c11 -Wall -Wextra -Werror
CPPFLAGS ?= -I$(INCLUDE_DIR)
LDFLAGS ?=
LDLIBS ?= -pthread -ldl -lm

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
SHARED_EXT := dylib
else
SHARED_EXT := so
endif
SHARED_LIB := target/release/libcotel.$(SHARED_EXT)

.PHONY: all build test example-gcc example-clang check clean install

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
	COTEL_EXPORTER=none $(BUILD_DIR)/kitchen-sink-gcc
	COTEL_EXPORTER=stdout $(BUILD_DIR)/kitchen-sink-gcc > $(BUILD_DIR)/stdout-gcc.log
	grep -q 'all signals completed' $(BUILD_DIR)/stdout-gcc.log
	grep -q 'trace_id=' $(BUILD_DIR)/stdout-gcc.log

example-clang: build | $(BUILD_DIR)
	$(CLANG) $(CFLAGS) $(CPPFLAGS) $(EXAMPLE_SRC) $(STATIC_LIB) \
		$(LDFLAGS) $(LDLIBS) -o $(BUILD_DIR)/kitchen-sink-clang
	COTEL_EXPORTER=none $(BUILD_DIR)/kitchen-sink-clang
	COTEL_EXPORTER=stdout $(BUILD_DIR)/kitchen-sink-clang > $(BUILD_DIR)/stdout-clang.log
	grep -q 'all signals completed' $(BUILD_DIR)/stdout-clang.log
	grep -q 'trace_id=' $(BUILD_DIR)/stdout-clang.log

check: test example-gcc example-clang

install: build
	install -d $(DESTDIR)$(PREFIX)/include $(DESTDIR)$(PREFIX)/lib
	install -m 644 $(HEADERS) $(DESTDIR)$(PREFIX)/include/
	install -m 644 $(STATIC_LIB) $(DESTDIR)$(PREFIX)/lib/
	install -m 644 $(SHARED_LIB) $(DESTDIR)$(PREFIX)/lib/

clean:
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
