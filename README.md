# otel-capi

`otel-capi` is a C API for OpenTelemetry. It exposes traces, metrics, and logs
through generated headers and caller-owned handle structs.

## Layout

- `include/otel_capi*.h` — generated into `include/` by each crate's `build.rs`
  during `make build` and gitignored. Do not edit by hand. `otel_capi.h` is the
  umbrella header; it includes the core area headers plus the generated
  `otel_capi_grpc_exporter.h`, `otel_capi_http_exporter.h`, and
  `otel_capi_stdout_exporter.h`, so Doxygen sees the exporter APIs in the same
  documentation set. Area headers can also be included individually.
  Semantic-convention helpers live in `otel_capi_semconv.h`, split into
  `otel_capi_semconv_attributes.h` and `otel_capi_semconv_metrics.h`.
- `crates/otel-capi/` — shared ABI, allocation tracking, runtime, and C API.
- `crates/otel-capi-*-exporter/` — optional loadable exporter libraries.
- `crates/otel-capi-http-transport/` — shared blocking HTTP sender used by
  the HTTP and gRPC exporters.
- `examples/kitchen-sink/main.c` — C example covering all signals.
- `Makefile` — standard build/test/install entrypoint.
- `LINKING.md` — static and dynamic linking instructions for Linux, macOS, and
  Windows.
- `CMakeLists.txt`, `cmake/` — CMake build/install with `find_package` support.
- `checks/` — compile-pass and compile-fail checks for the C API.
- `otel-capi.pc.in` — pkg-config metadata template.
- `scripts/package_release.py` — packages Cargo release output into archives.
- `scripts/generate_docs.py` — renders Doxygen HTML from the generated headers.
- `.github/workflows/` — CI, PR docs previews, release-please, and release
  artifact builds.
- `Formula/`, `bucket/`, `conanfile.py`, `ports/otel-capi/` — package-manager
  manifests for Homebrew, Scoop, Conan, and vcpkg.
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

`make check` runs the Rust unit tests for every workspace crate, checks formatting
with `cargo fmt`, runs `cargo clippy` with `-D warnings` (including `undocumented_unsafe_blocks`),
compiles the kitchen-sink example with both gcc and clang using
`-std=c11 -Wall -Wextra -Werror`, runs it with both `OTEL_CAPI_EXPORTER=none`
and `OTEL_CAPI_EXPORTER=stdout`, and verifies the compile-pass/compile-fail
checks in `checks/`. The warning policy lives in `.cargo/config.toml`.

Run Miri to validate the unsafe code with the nightly toolchain from the dev
shell:

```sh
nix develop --command make miri
```

Both `make test` and `make miri` run the unit tests in the core crate, the
shared HTTP transport, and all three loadable exporters. The Miri target disables
isolation so the tests can read the real clock and process environment.

Collect coverage for the same tests with:

```sh
nix develop --command make coverage
```

`make coverage` runs Miri across every crate first and then measures the same
tests with LLVM instrumentation. Miri interprets MIR and cannot emit LLVM
coverage data itself, so the report reflects the workspace test suite that Miri
validated. The target prints a workspace summary, excludes `build.rs` from the
report, fails below 90% line coverage, and writes the report to
`build/coverage/html` and `build/coverage/lcov.info`, which are gitignored.

Generate HTML API documentation from the generated headers with:

```sh
nix develop --command make docs
```

The output is in `build/docs/html`. Doxygen is provided by the dev shell.

On pull requests, the **Docs preview** workflow generates the same HTML and
posts a comment with a link to the uploaded artifact; the link is refreshed on
each push to the PR. For PRs from forks, GitHub gives the workflow a read-only
token, so the artifact is still uploaded but the comment is skipped.

Install the library, optional exporter libraries, headers, and pkg-config metadata with:

```sh
nix develop --command make install PREFIX=/path/to/prefix
```

## Using the C API

See [LINKING.md](LINKING.md) for the static and shared library variants, the
per-platform include/link commands, and the release archives that contain them.

Include `otel_capi.h`, declare handles as zero-initialized storage, and pass their
addresses to the matching `_new` function:

```c
#include "otel_capi.h"

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
- Use `OTEL_<TYPE>_INIT` for zero-initialized handle storage; generated INIT
  macros use designated initializers. Use `otel_<type>_is_valid()` to check
  whether a handle holds an object.
- On GCC and Clang, add `OTEL_AUTO_<TYPE>` to a handle declaration to release
  it automatically at scope exit, for example
  `otel_span_t span OTEL_AUTO_SPAN = OTEL_SPAN_T_INIT;`. The macro is empty on
  other compilers, where explicit `release` calls are still required.
- `OTEL_VERSION_MAJOR`, `OTEL_VERSION_MINOR`, `OTEL_VERSION_PATCH`,
  `OTEL_VERSION_STRING`, `OTEL_VERSION_CODE`, and `OTEL_VERSION_AT_LEAST(...)`
  support compile-time version checks.
- `OTEL_RETURN_IF_ERROR(call)` returns the failing status from a function that
  itself returns `otel_result_t`.
- Variadic attribute calls use `otel_attribute_t` values built with
  `otel_attr_string`, `otel_attr_bool`, `otel_attr_i64`, and `otel_attr_f64`,
  terminated by `OTEL_ATTR_END`.
- Every variadic attribute API also has an `_arr` variant that takes
  `const otel_attribute_t *attributes[], size_t count` for dynamically built
  lists. Up to 1024 attributes are accepted per call.
- `otel_version()` returns the library version; `otel_strerror()` provides a
  stable description for result codes. `otel_last_error()` is thread-local and
  valid only until the next fallible call on that thread.
- `otel_live_handle_count()` reports how many handles are currently live;
  `otel_ensure_all_released()` returns `OTEL_OK` only when that count is zero,
  otherwise `OTEL_ERR_LEAK` with the count in `otel_last_error()`.

The API is C11 only. Generated headers do not add C++ compatibility guards; use
the standard OpenTelemetry C++ library for C++ projects.

### Naming conventions

Suffixes are consistent across the generated headers:

- `_n` takes `otel_str_t` length-aware views for string arguments. This applies
  to signal APIs, attribute builders, semconv helpers, and the structured log
  REC macros.
- `_arr` takes an explicit `const otel_attribute_t *attributes[], size_t count`
  instead of a `OTEL_ATTR_END`-terminated variadic list.
- `_from_str` parses a NUL-terminated C string into numeric ID or typed
  propagation storage; `_to_str` formats a numeric ID back into a caller buffer.
- `_str` returns a borrowed NUL-terminated `const char *` from typed
  propagation storage; `_view` returns a borrowed `otel_str_t` with length.

`otel_capi.h` includes the generated semantic-convention headers, which `build.rs`
generates from the `opentelemetry-semantic-conventions` crate at build time.
Macro names are
`OTEL_<MODULE>_<CONSTANT>`, for example `OTEL_ATTRIBUTE_CLIENT_ADDRESS`,
`OTEL_ATTRIBUTE_RPC_SYSTEM`, and `OTEL_METRIC_HTTP_SERVER_REQUEST_DURATION`.

Where the generated docs make the value type unambiguous, it also emits a
typed `static inline` helper such as `otel_attr_client_address("example.org")`
or `otel_attr_http_response_status_code(200)`. These are functions rather than
macros, so C checks that you pass a string, `int64_t`, `double`, or `int` as
appropriate. The original key macros remain available so you can still choose
the type explicitly. Each typed helper also has an `_n` variant, such as
`otel_attr_client_address_n(OTEL_STR("example.org"))`, for length-aware input.

Metric constants with complete semconv metadata get matching helpers such as
`otel_metric_http_server_request_duration(&meter, &histogram)`, which uses the
standard instrument, unit, and description from the semantic conventions. The
`_n` variant, `otel_metric_http_server_request_duration_n`, uses `otel_str_t`
views for the name, unit, and description.

`otel_meter_observable_gauge_f64` and `otel_meter_observable_counter_u64`
register C callbacks that are invoked during collection. The callback receives
an `otel_observe_context_t`, which must be passed back to `otel_observe_f64` or
`otel_observe_u64` with variadic attributes while the callback is running. The
context is invalid after the callback returns. `user_data` is borrowed by the
provider and must remain valid and thread-safe until the meter provider is
shut down.

### Length-aware strings

`otel_str_t` is a borrowed `{ const char *ptr; size_t len; }` view. It removes
the need for NUL termination and allows embedded bytes. Use `OTEL_STR("literal")`
for string literals, `OTEL_STR_VIEW(text)` for a C string of unknown length, or
`OTEL_STR_NULL` for omitted optional fields.

APIs that take names, bodies, or descriptions also have `_n` variants that
accept `otel_str_t`; for example `otel_tracer_span_start_n`,
`otel_span_add_event_n`, `otel_logger_log_n`, and
`otel_meter_counter_u64_n`. The `_n` attribute builders
(`otel_attr_string_n`, `otel_attr_bool_n`, `otel_attr_i64_n`,
`otel_attr_f64_n`) take length-aware key and value views. The original
`const char *` functions remain available and now accept non-NUL-terminated
attribute keys/values through the attribute struct's `key_len` and `value_len`
fields.

Typed propagation buffers also expose `otel_traceparent_view`,
`otel_tracestate_view`, `otel_baggage_view`, and `otel_baggage_value_view`. They
return an `otel_str_t` pointing into the same storage, with the length already
computed. The returned view is borrowed and valid only while the buffer is.

### Structured logs

`otel_logger_log_record` takes an `otel_log_record_options_t` initialized with
`OTEL_LOG_RECORD_OPTIONS_INIT`. It can set an event name, instrumentation
target, severity text override, explicit event/observed timestamps in Unix
nanoseconds, and a span for trace correlation. Like `otel_logger_log`, it
accepts variadic attributes terminated by `OTEL_ATTR_END`. Event names and
severity text are interned for the process lifetime because the SDK requires
`'static` strings for those fields.

For simple messages, `OTEL_LOG_TRACE`, `OTEL_LOG_DEBUG`, `OTEL_LOG_INFO`,
`OTEL_LOG_WARN`, `OTEL_LOG_ERROR`, and `OTEL_LOG_FATAL` bind the severity and
append `OTEL_ATTR_END` for you:

```c
CHECK(OTEL_LOG_INFO(&logger, &span, "work complete",
                    otel_attr_i64("items", 3)));
```

The `OTEL_LOG_*_N` variants take an `otel_str_t` body, for example
`OTEL_LOG_INFO_N(&logger, &span, OTEL_STR("work complete"))`.

For structured records, the `OTEL_LOG_*_REC` macros bind the severity on an
`otel_log_record_options_t`, copying the caller's options and overriding `level`:

```c
otel_log_record_options_t options = OTEL_LOG_RECORD_OPTIONS_INIT;
options.event_name = "work.complete";
CHECK(OTEL_LOG_INFO_REC(&logger, &options, "work complete",
                        otel_attr_i64("items", 3)));
```

`OTEL_LOG_*_REC_N` takes an `otel_str_t` body, while `OTEL_LOG_*_REC_ARR` and
`OTEL_LOG_*_REC_ARR_N` take an explicit attribute array and count. All four
families exist for `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`, and `FATAL`.

### Propagation and span options

`otel_context_t` carries W3C trace context and baggage. Propagation values use
fixed-size structs instead of raw buffers: `otel_traceparent_t` (56 bytes),
`otel_tracestate_t` (513 bytes), `otel_baggage_t` (1025 bytes), and
`otel_baggage_value_t` (257 bytes). Their capacities include the NUL terminator.
Initialize them with `OTEL_TRACEPARENT_INIT`, `OTEL_BAGGAGE_VALUE_INIT`, and so
on, or with the `_INIT_STR("...")` forms for string literals; the literal forms
fail to compile when the value does not fit. `otel_traceparent_from_str` and the
other `_from_str` functions validate/copy runtime strings. `_str` accessors
return the stored C string.

Use `otel_context_extract` / `otel_context_inject` with those typed buffers,
`otel_context_set_baggage` / `otel_context_get_baggage` to edit baggage values,
and `otel_context_attach` / `otel_context_detach` to make a context current on
a thread. `otel_context_from_span` builds a context from a live span.
Trace and span IDs are numeric, not strings. `otel_trace_id_t` holds the 16-byte
trace ID as two big-endian `uint64_t` fields (`hi` and `lo`), and
`otel_span_id_t` holds the 8-byte span ID in a single `uint64_t`. This avoids
non-standard `unsigned __int128` while keeping the full ID width portable.
`otel_span_trace_id` / `otel_span_span_id` write those structs;
`otel_trace_id_from_str` / `otel_span_id_from_str` parse 32/16 hex digits;
`otel_trace_id_to_str` / `otel_span_id_to_str` format them back into a caller
buffer. `otel_traceparent_trace_id` and `otel_traceparent_span_id` parse the IDs
directly from a validated `otel_traceparent_t`. Use `otel_trace_id_is_valid`,
`otel_span_id_is_valid`, `otel_trace_id_equal`, and `otel_span_id_equal` to test
values without formatting them. Links take the numeric IDs directly:

```c
CHECK(otel_span_add_link(&span, trace_id, span_id,
                         otel_attr_string("link.kind", "parent"),
                         OTEL_ATTR_END));
```

`otel_span_add_link_from_traceparent` and
`otel_span_add_link_from_traceparent_arr` parse the IDs from a validated
`otel_traceparent_t` instead of taking separate ID structs.

`otel_span_options_t` (initialize with `OTEL_SPAN_OPTIONS_INIT`) can set a
parent context, span kind, start timestamp, and `end_on_release`. With
`end_on_release` set, the span is ended automatically when the last handle
reference is released, so C callers can use release as a deferred close.
Started spans also support
`otel_span_update_name`, timestamped events, links, exception events, and
timestamped ends via the `otel_span_*` APIs.

Resources can include caller attributes with
`otel_resource_new_with_attributes`; environment-derived attributes are still
applied.

### Runtime handle

`otel_runtime_t` is a small exporter holder created with `otel_runtime_new` and
passed to each provider constructor. Providers retain it internally, so the
original handle can be released once all providers have been created. The core
library does not own a Tokio runtime or create runtime worker threads.
`otel_runtime_new_with_threads` is retained for source compatibility; its count
is ignored. OpenTelemetry's own batch processors and metric readers still create
their exporter threads.

### Application exporter callback

The library does not embed an HTTP client. Register a generic callback on the
runtime before creating providers. The callback's `otel_exporter_t` descriptor
carries the name the core should use, so `otel_runtime_set_exporter` does not
take a protocol name as a separate argument:

```c
static otel_result_t send_exporter(const otel_exporter_request_t *request,
                                    otel_exporter_response_t *response,
                                    void *user_data) {
  /* Use your HTTP stack; request->timeout_ms is the configured timeout. */
  *response = (otel_exporter_response_t)OTEL_EXPORTER_RESPONSE_INIT;
  response->status_code = 200;
  return OTEL_OK;
}

otel_runtime_t runtime = OTEL_RUNTIME_T_INIT;
otel_exporter_t exporter = OTEL_EXPORTER_INIT;
exporter.name = OTEL_STR("http");
exporter.send = send_exporter;
otel_runtime_new(&runtime);
otel_runtime_set_exporter(&runtime, &exporter);
```

The callback may run concurrently on OpenTelemetry background threads. Request
pointers are borrowed for the call only; response bodies and headers are copied
after the callback returns, so their storage must remain valid until the next
callback on the same thread. Only 2xx statuses are treated as success; other
statuses are reported as export failures. Return a non-`OTEL_OK` result to
report an exporter failure. `user_data` must stay valid until the runtime is
released.

### Loadable exporters

The core library does not link Tokio, tonic, reqwest, ureq, or a concrete
stdout client. Each optional exporter is a separate shared library with its
own typed handle, options struct, and descriptor. The descriptor is embedded in
the handle, so the exporter supplies its own name to the core runtime:

- `libotel_capi_http_exporter` uses the shared blocking HTTP transport.
- `libotel_capi_grpc_exporter` sends raw gRPC frames over that same transport.
- `libotel_capi_stdout_exporter` decodes and summarizes each OTLP batch.

The HTTP and gRPC exporters both depend on the internal
`otel-capi-http-transport` crate. It configures one `ureq` client with
`native-tls` and the platform trust store: Security Framework on macOS,
Schannel on Windows, and OpenSSL on Linux and other Unix systems. Install the
system TLS development headers (for example `libssl-dev` on Debian/Ubuntu or
`openssl-devel` on Fedora) when building those exporters from source. The
gRPC `worker_count` option is retained for ABI compatibility but ignored.

```c
/* POSIX sketch generated from the typed exporter headers. */
otel_http_exporter_t http = OTEL_HTTP_EXPORTER_T_INIT;
otel_http_exporter_options_t options = OTEL_HTTP_EXPORTER_OPTIONS_INIT;
otel_http_exporter_new(&http, &options);
otel_http_exporter_register(&runtime, &http); /* reads http.exporter.name */
/* ... create providers, emit telemetry, shut down ... */
otel_http_exporter_release(&http);
```

The gRPC and stdout libraries use the same pattern with
`OTEL_GRPC_EXPORTER_T_INIT` / `OTEL_GRPC_EXPORTER_OPTIONS_INIT` and
`OTEL_STDOUT_EXPORTER_T_INIT` / `OTEL_STDOUT_EXPORTER_OPTIONS_INIT`. Their
`*_register` helpers call `otel_runtime_set_exporter` through a resolved core
symbol, so loadable libraries do not need a link-time dependency on the core
shared library. Handles from all libraries are included in
`otel_live_handle_count()` and `otel_ensure_all_released()`.

### In-memory item exporter

Set `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, and
`OTEL_LOGS_EXPORTER` to `item` to receive telemetry in-process instead of
encoding an OTLP request. Register callbacks on the runtime before creating
providers:

```c
otel_item_exporter_t exporter = OTEL_ITEM_EXPORTER_INIT;
exporter.on_span = on_span;      // otel_span_item_fn
exporter.on_metric = on_metric;  // otel_metric_item_fn
exporter.on_log = on_log;        // otel_log_item_fn
exporter.on_encoded = on_encoded; // otel_encoded_item_fn
otel_runtime_set_item_exporter(&runtime, &exporter);
```

Typed callbacks receive one finished span, log record, or aggregated metric
stream at a time; all nested strings, attributes, events, links, and data points
are borrowed and valid only until the callback returns. The encoded callback
receives OTLP protobuf bytes for one signal and may be used alongside the typed
callbacks for exact wire fidelity. Your callbacks own batching, buffering, and
forwarding. They may run concurrently on OpenTelemetry background threads, so
they and `user_data` must be thread-safe.

## Configuration

Standard OpenTelemetry environment variables are read at provider creation:

- `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_SDK_DISABLED`
- `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER`
  - `otlp` (default), `item`/`memory`, `stdout`/`console`, or `none`
- OTLP settings via `OTEL_EXPORTER_OTLP_*`, including signal-specific
  endpoint variables (used verbatim when set; the generic endpoint gets the
  signal path appended), protocol, timeout, headers, and compression. Headers
  prefer signal-specific variables and are percent-decoded; unavailable
  compression values are warned about and ignored. Metric temporality follows
  `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE`.
- `OTEL_TRACES_SAMPLER` (`always_on`, `always_off`, `trace_id_ratio`) and
  `OTEL_TRACES_SAMPLER_ARG`
- Batch intervals: `OTEL_BSP_SCHEDULE_DELAY`, `OTEL_METRIC_EXPORT_INTERVAL`,
  `OTEL_BLRP_SCHEDULE_DELAY`

OTLP exporters require a runtime handle passed to the provider constructor;
without one, construction fails with `OTEL_ERR_CONFIG`.

### Explicit providers instead of globals

`otel-capi` does not install process-global tracer, meter, or logger providers.
Handles make initialization order, exporter/runtime lifetime, and shutdown
deterministic, and avoid hidden global state when the library is embedded.
Pass provider and scope handles through your C call graph; the standard
environment variables above still configure each provider when it is created.
The only implicit context is the thread-local one managed by
`otel_context_attach` / `otel_context_detach`.

## Kitchen sink

```sh
nix develop --command make example-gcc
OTEL_CAPI_EXPORTER=none build/artifacts/kitchen-sink-gcc
OTEL_CAPI_EXPORTER=stdout build/artifacts/kitchen-sink-gcc
OTEL_CAPI_EXPORTER=item build/artifacts/kitchen-sink-gcc
```

The example creates a resource, all three providers, a correlated span/log
pair, a structured log record with event name/target/timestamps, all five
metric instruments plus an observable gauge/counter pair with C callbacks,
retains/releases a span, and flushes and shuts down every provider. It extracts
and injects a remote W3C context, attaches baggage to
the current thread, and starts a child span with an explicit parent, kind,
link, timestamped event, exception event, and deferred close on release. It
also starts four pthread workers that create
concurrent spans, correlated logs, and counter updates against shared scopes,
which exercises handle reference counting and instrument thread safety. Handles
use `OTEL_AUTO_<TYPE>` declarations so they are released when the example’s
inner scope exits. It finishes with `otel_ensure_all_released()` to verify that
every handle was released.

## Release archives

Each GitHub release publishes archives for Linux (x86_64 glibc and musl), macOS
(x86_64 and arm64), and Windows (x86_64), with a `.sha256` sidecar per archive.
Every target has three variants:

- `otel-capi-<version>-<target>.tar.gz` (`.zip` on Windows) — the generated
  headers, static and shared core libraries, optional exporters, a relocatable
  `otel-capi.pc`, `LICENSE`, `README.md`, `CHANGELOG.md`, and `LINKING.md`.
- `otel-capi-<version>-<target>-static.tar.gz` — the same headers and metadata
  with only the static core library.
- `otel-capi-<version>-<target>-shared.tar.gz` — the same headers and metadata
  with the shared core library and optional exporters.

See [LINKING.md](LINKING.md) for which variant to use and the include/link
commands on each platform. A separate `otel-capi-<version>-docs.tar.gz`
contains the Doxygen HTML reference for the generated headers and the linking
guide. The same reference is published on release at
<https://daniel-white.github.io/otel_capi/>. For example:

```sh
version=0.2.1
target=x86_64-unknown-linux-gnu
curl -LO "https://github.com/daniel-white/otel_capi/releases/download/v${version}/otel-capi-${version}-${target}-static.tar.gz"
tar xzf "otel-capi-${version}-${target}-static.tar.gz"

curl -LO "https://github.com/daniel-white/otel_capi/releases/download/v${version}/otel-capi-${version}-docs.tar.gz"
tar xzf "otel-capi-${version}-docs.tar.gz"
```

## Package managers

The repo includes manifests for common package managers. Each one builds from
the published tag or release using the standard CMake install rules, so no
prebuilt binary is required.

- Homebrew tap:

  ```sh
  brew tap daniel-white/otel_capi https://github.com/daniel-white/otel_capi.git
  brew install otel-capi
  ```

- vcpkg overlay port:

  ```sh
  vcpkg install otel-capi --overlay-ports=/path/to/otel-capi/ports
  ```

- Conan recipe:

  ```sh
  conan create . --version 0.1.0
  ```

- Scoop bucket (Windows):

  ```sh
  scoop bucket add otel-capi https://github.com/daniel-white/otel_capi.git
  scoop install otel-capi
  ```

Downstream CMake projects can use the installed package directly:

```cmake
find_package(otel-capi CONFIG REQUIRED)
target_link_libraries(my_app PRIVATE otel-capi::otel-capi)
```

pkg-config is also installed:

```sh
cc app.c $(pkg-config --cflags --libs otel-capi)
```

## Releasing

Versioning and changelogs are handled by
[release-please](https://github.com/googleapis/release-please). Use Conventional
Commit messages for commits and pull request titles; see
[CONTRIBUTING.md](CONTRIBUTING.md). Release-please opens a release PR that
updates `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and the package-manager
manifests. Merging that PR creates the tag and GitHub release and dispatches
the cross-platform artifact and documentation workflow.

If GitHub Actions is not allowed to create pull requests in this repository,
add a PAT with repository write access as the `RELEASE_PLEASE_TOKEN` secret, or
enable that permission under **Settings → Actions → General**.

## License

Licensed under the [Apache License, Version 2.0](LICENSE), the same license used
by OpenTelemetry.
