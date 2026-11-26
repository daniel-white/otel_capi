# cotel

`cotel` is a C ABI wrapper around the OpenTelemetry Rust SDK. It exposes
traces, metrics, and logs through a generated C header and caller-owned handle
structs.

## Layout

- `include/cotel.h` — generated C header; do not edit by hand.
- `src/` — Rust implementation. `build.rs` generates the header with cbindgen.
- `examples/kitchen-sink/main.c` — C example covering all signals.
- `Makefile` — standard build/test/install entrypoint.
- `flake.nix` — optional dev shell that provides Rust 1.100.0 and C compilers.

## Build and test

Rust 1.99+ is required for Rust-defined C variadics; this project pins
1.100.0. With Rust already on `PATH`:

```sh
make build
make check
```

In a fresh checkout, use the dev shell to provide the toolchain:

```sh
nix develop --command make build
nix develop --command make check
```

`make check` runs the Rust unit tests, then compiles the kitchen-sink example
with both gcc and clang using `-std=c11 -Wall -Wextra -Werror`, and runs it
with both `COTEL_EXPORTER=none` and `COTEL_EXPORTER=stdout`.

Install the library and header with:

```sh
nix develop --command make install PREFIX=/path/to/prefix
```

## Using the C API

Include `cotel.h`, declare handles as zero-initialized storage, and pass their
addresses to the matching `_new` function:

```c
#include "cotel.h"

otel_runtime_t runtime = {0};
otel_tracer_provider_t provider = {0};

otel_result_t result = otel_runtime_new(&runtime);
if (result != OTEL_OK) {
    fprintf(stderr, "%s\n", otel_last_error());
}

/* ... use provider ... */

otel_tracer_provider_release(&provider);
otel_runtime_release(&runtime);
```

Key rules:

- Handles are caller-allocated. The library writes into the struct you pass;
  it never allocates the handle itself.
- Every successfully created handle owns one reference. Call the matching
  `otel_*_release` exactly once per reference; `release` clears the struct and
  is safe on a zero-initialized one.
- Use `otel_*_retain` to create an additional reference in another
  zero-initialized handle.
- Strings are borrowed for the duration of a call only.
- All exported enums are non-exhaustive so new variants can be added without
  breaking existing callers.
- Variadic attribute calls use `otel_attribute_t` values built with
  `otel_attr_string`, `otel_attr_bool`, `otel_attr_i64`, and `otel_attr_f64`,
  terminated by `NULL`.

### Async runtime

`otel_runtime_t` is created explicitly with `otel_runtime_new` and passed to
each provider constructor. Providers retain it internally, so the original
runtime handle can be released once all providers have been created. The async
engine is an implementation detail; C callers only see the opaque handle.

## Configuration

Standard OpenTelemetry environment variables are read at provider creation:

- `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_SDK_DISABLED`
- `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER`
  - `otlp` (default), `stdout`/`console`, or `none`
- OTLP settings via `OTEL_EXPORTER_OTLP_*`, including signal-specific
  endpoint variables
- `OTEL_TRACES_SAMPLER` (`always_on`, `always_off`, `trace_id_ratio`) and
  `OTEL_TRACES_SAMPLER_ARG`
- Batch intervals: `OTEL_BSP_SCHEDULE_DELAY`, `OTEL_METRIC_EXPORT_INTERVAL`,
  `OTEL_BLRP_SCHEDULE_DELAY`

OTLP exporters require a runtime handle passed to the provider constructor;
without one, construction fails with `OTEL_ERR_CONFIG`.

## Kitchen sink

```sh
nix develop --command make example-gcc
COTEL_EXPORTER=none build/artifacts/kitchen-sink-gcc
COTEL_EXPORTER=stdout build/artifacts/kitchen-sink-gcc
```

The example creates a resource, all three providers, a correlated span/log
pair, all five metric instruments, retains/releases a span, and flushes and
shuts down every provider.
