# cotel

`cotel` is a C ABI wrapper around the OpenTelemetry Rust SDK. It exposes
traces, metrics, and logs through a generated C header and caller-owned handle
structs.

## Layout

- `include/cotel.h` — generated C header; do not edit by hand.
- `include/cotel_semconv.h` — generated semantic-convention attribute macros.
- `src/` — Rust implementation. `build.rs` generates the header with cbindgen.
- `examples/kitchen-sink/main.c` — C example covering all signals.
- `Makefile` — standard build/test/install entrypoint.
- `checks/` — compile-pass and compile-fail checks for the C API.
- `cotel.pc.in` — pkg-config metadata template.
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

With direnv and nix-direnv installed, the repo's `.envrc` enters that dev
shell automatically. Run `direnv allow` once after cloning, then plain
`make build` / `make check` work in that directory.

`make check` runs the Rust unit tests, compiles the kitchen-sink example with
both gcc and clang using `-std=c11 -Wall -Wextra -Werror`, runs it with both
`COTEL_EXPORTER=none` and `COTEL_EXPORTER=stdout`, and verifies the
compile-pass/compile-fail checks in `checks/`.

Install the library, headers, and pkg-config metadata with:

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
    fprintf(stderr, "%s: %s\n", otel_strerror(result), otel_last_error());
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
- All exported enums have explicit numeric values and `OTEL_<ENUM>_SENTINEL`
  macros, so new variants can be added without disturbing existing values.
- Status, severity, and attribute-kind inputs are plain `int32_t` values that
  the library validates; passing an out-of-range integer returns
  `OTEL_ERR_INVALID_ARGUMENT`.
- Use `OTEL_<TYPE>_INIT` for zero-initialized handle storage, and
  `otel_<type>_is_valid()` to check whether a handle holds an object.
- Variadic attribute calls use `otel_attribute_t` values built with
  `otel_attr_string`, `otel_attr_bool`, `otel_attr_i64`, and `otel_attr_f64`,
  terminated by `OTEL_ATTR_END`.
- `otel_version()` returns the library version; `otel_strerror()` provides a
  stable description for result codes. `otel_last_error()` is thread-local and
  valid only until the next fallible call on that thread.

`cotel.h` includes `cotel_semconv.h`, which `build.rs` generates from the
`opentelemetry-semantic-conventions` crate at build time. Macro names are
`OTEL_<MODULE>_<CONSTANT>`, for example `OTEL_ATTRIBUTE_CLIENT_ADDRESS`,
`OTEL_ATTRIBUTE_RPC_SYSTEM`, and `OTEL_METRIC_HTTP_SERVER_REQUEST_DURATION`.

Where the generated docs make the value type unambiguous, it also emits a
typed `static inline` helper such as `otel_attr_client_address("example.org")`
or `otel_attr_http_response_status_code(200)`. These are functions rather than
macros, so C checks that you pass a string, `int64_t`, `double`, or `int` as
appropriate. The original key macros remain available so you can still choose
the type explicitly.

Metric constants with complete semconv metadata get matching helpers such as
`otel_metric_http_server_request_duration(&meter, &histogram)`, which uses the
standard instrument, unit, and description from the semantic conventions.

### Structured logs

`otel_logger_log_record` takes an `otel_log_record_options_t` initialized with
`OTEL_LOG_RECORD_OPTIONS_INIT`. It can set an event name, instrumentation
target, severity text override, explicit event/observed timestamps in Unix
nanoseconds, and a span for trace correlation. Like `otel_logger_log`, it
accepts variadic attributes terminated by `OTEL_ATTR_END`. Event names and
severity text are interned for the process lifetime because the SDK requires
`'static` strings for those fields.

### Propagation and span options

`otel_context_t` carries W3C trace context and baggage. Use
`otel_context_extract` / `otel_context_inject` for HTTP-style carriers,
`otel_context_set_baggage` / `otel_context_get_baggage` to edit baggage, and
`otel_context_attach` / `otel_context_detach` to make a context current on a
thread. `otel_context_from_span` builds a context from a live span.

`otel_span_options_t` (initialize with `OTEL_SPAN_OPTIONS_INIT`) can set a
parent context, span kind, and start timestamp for
`otel_tracer_span_start_with_options`. Started spans also support
`otel_span_update_name`, timestamped events, links, exception events, and
timestamped ends via the `otel_span_*` APIs.

Resources can include caller attributes with
`otel_resource_new_with_attributes`; environment-derived attributes are still
applied.

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
pair, a structured log record with event name/target/timestamps, all five
metric instruments, retains/releases a span, and flushes and shuts down every
provider. It extracts and injects a remote W3C context, attaches baggage to
the current thread, and starts a child span with an explicit parent, kind,
link, timestamped event, and exception event. It also starts four pthread
workers that create
concurrent spans, correlated logs, and counter updates against shared scopes,
which exercises handle reference counting and instrument thread safety.
