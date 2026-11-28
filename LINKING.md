# Static and dynamic linking

`otel-capi` is built in two core variants from the same source, with the same
generated headers:

| Variant | Linux / macOS | Windows (MSVC) |
| --- | --- | --- |
| Static | `libotel_capi.a` | `otel_capi.lib` |
| Shared | `libotel_capi.so` / `libotel_capi.dylib` | `otel_capi.dll` plus `otel_capi.dll.lib` (import library) |

The optional HTTP, gRPC, and stdout exporters are shared libraries only. Their
headers are always included; their binaries ship with the shared variant.

## Release archives

Each GitHub release publishes three archives per target, each with a `.sha256`
sidecar:

- `otel-capi-<version>-<target>.tar.gz` (`.zip` on Windows) — both core
  variants, the optional exporters, headers, and pkg-config metadata.
- `otel-capi-<version>-<target>-static.tar.gz` — headers, pkg-config metadata,
  and the static core library only.
- `otel-capi-<version>-<target>-shared.tar.gz` — headers, pkg-config metadata,
  the shared core library, and the optional exporter libraries.

Published targets are `x86_64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, `x86_64-apple-darwin`,
`aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`. The musl build ships the
core library and the stdout exporter; the HTTP and gRPC exporters require
native TLS and are not built for musl. Archives are not interchangeable across
targets, and a static archive built for musl cannot be linked by glibc.

Every archive has the same layout:

```text
otel-capi-<version>-<target>[-static|-shared]/
├── include/            otel_capi*.h
├── lib/                core library (and import libraries on Windows)
│   └── pkgconfig/      otel-capi.pc
├── bin/                DLLs on Windows
├── LICENSE
├── README.md
├── CHANGELOG.md
└── LINKING.md
```

Extract the archive for your target and use the headers and library from that
same directory. Do not mix headers and binaries from different releases, and
do not link both core variants into one process: each copy has its own runtime
state and handle tracking.

## Linux (glibc)

Dynamic linking (archive `otel-capi-<version>-x86_64-unknown-linux-gnu-shared.tar.gz`):

<!-- x-release-please-start-version -->
```sh
tar xzf otel-capi-0.2.1-x86_64-unknown-linux-gnu-shared.tar.gz
cd otel-capi-0.2.1-x86_64-unknown-linux-gnu
cc -std=c11 -Iinclude app.c -Llib -lotel_capi -pthread -ldl -lm \
  -Wl,-rpath,'$ORIGIN/../lib' -o app
```
<!-- x-release-please-end -->

The rpath is optional; without it, set `LD_LIBRARY_PATH` to the extracted
`lib/` directory or install the library system-wide.

Static linking (archive `...-x86_64-unknown-linux-gnu-static.tar.gz`):

```sh
cc -std=c11 -Iinclude app.c lib/libotel_capi.a -pthread -ldl -lm -o app
```

With `-Llib`, you can also force static selection when both variants are
present: `-Wl,-Bstatic -lotel_capi -Wl,-Bdynamic`. A fully static executable
needs the musl archive below; the glibc archive still depends on the system C
library at run time.

## Linux (musl)

The musl archive is intended for static, self-contained binaries:

<!-- x-release-please-start-version -->
```sh
tar xzf otel-capi-0.2.1-x86_64-unknown-linux-musl-static.tar.gz
cd otel-capi-0.2.1-x86_64-unknown-linux-musl
musl-gcc -static -std=c11 -Iinclude app.c lib/libotel_capi.a -pthread -lm -o app
```
<!-- x-release-please-end -->

Use the matching musl toolchain; `gcc` cannot link the musl archive into a
glibc executable.

## macOS

Pick `x86_64-apple-darwin` for Intel or `aarch64-apple-darwin` for Apple
Silicon. Dynamic linking:

<!-- x-release-please-start-version -->
```sh
tar xzf otel-capi-0.2.1-aarch64-apple-darwin-shared.tar.gz
cd otel-capi-0.2.1-aarch64-apple-darwin
clang -std=c11 -Iinclude app.c -Llib -lotel_capi -pthread -lm \
  -Wl,-rpath,@executable_path/../lib -o app
```
<!-- x-release-please-end -->

Use `@loader_path` instead of `@executable_path` when the rpath should be
relative to a library rather than the executable. `DYLD_LIBRARY_PATH` is the
runtime alternative to an rpath.

Static linking (archive `...-aarch64-apple-darwin-static.tar.gz`):

```sh
clang -std=c11 -Iinclude app.c lib/libotel_capi.a -pthread -lm -o app
```

Check the architecture with `file lib/libotel_capi.a`; a static archive cannot
be linked into an executable of the other architecture. To support both
architectures, build/install each target separately or combine the static
libraries with `lipo`.

## Windows (MSVC)

Use an **x64 Native Tools Command Prompt**. Static linking uses
`otel_capi.lib` from the `-static` zip:

<!-- x-release-please-start-version -->
```powershell
Expand-Archive otel-capi-0.2.1-x86_64-pc-windows-msvc-static.zip
cl /std:c11 /Iotel-capi-0.2.1-x86_64-pc-windows-msvc-static\include `
  app.c otel-capi-0.2.1-x86_64-pc-windows-msvc-static\lib\otel_capi.lib
```
<!-- x-release-please-end -->

Dynamic linking uses the import library `otel_capi.dll.lib`, not the static
`otel_capi.lib`. Copy `otel_capi.dll` next to the executable (or put it on
`PATH`):

<!-- x-release-please-start-version -->
```powershell
Expand-Archive otel-capi-0.2.1-x86_64-pc-windows-msvc-shared.zip
cl /std:c11 /Iotel-capi-0.2.1-x86_64-pc-windows-msvc-shared\include `
  app.c otel-capi-0.2.1-x86_64-pc-windows-msvc-shared\lib\otel_capi.dll.lib
copy otel-capi-0.2.1-x86_64-pc-windows-msvc-shared\bin\otel_capi.dll .
```
<!-- x-release-please-end -->

`otel_capi.lib` and `otel_capi.dll.lib` are different files with different
purposes; linking the wrong one produces duplicate symbols or an unresolved
import. The MSVC `.lib` files cannot be consumed by MinGW; build from source
for a GNU target instead.

## pkg-config and CMake

Every archive includes a relocatable `otel-capi.pc`. For a compiler that uses
pkg-config:

```sh
export PKG_CONFIG_PATH="$PWD/lib/pkgconfig"
cc app.c $(pkg-config --cflags --libs otel-capi) -o app
```

The `.pc` file contains `-lotel_capi`, so the linker selects the shared
library when both variants are installed in the same directory. For static
linking, pass the `.a` / `.lib` path explicitly or use `-Wl,-Bstatic` on Linux.

For CMake, point `CMAKE_PREFIX_PATH` at the extracted prefix:

```sh
cmake -S . -B build -DCMAKE_PREFIX_PATH=/path/to/otel-capi-<version>-<target>
```

```cmake
find_package(otel-capi CONFIG REQUIRED)
target_link_libraries(my_app PRIVATE otel-capi::otel-capi)
```

The installed config resolves `-lotel_capi` through `find_library`. To select
one variant explicitly, set `OTEL_CAPI_LIBRARY` before `find_package`:

```sh
cmake -S . -B build \
  -DCMAKE_PREFIX_PATH=/path/to/otel-capi-<version>-<target> \
  -DOTEL_CAPI_LIBRARY=/path/to/otel-capi-<version>-<target>/lib/libotel_capi.a
```

On Windows, use the full path to `otel_capi.lib` for static linking or
`otel_capi.dll.lib` for dynamic linking.

## Loadable exporters

The HTTP, gRPC, and stdout exporter libraries are shared objects/DLLs. Copy
them next to the executable (or set `LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH`, or
`PATH`) and load them with `dlopen` / `LoadLibrary`, then call the matching
`otel_*_exporter_new`, `otel_*_exporter_register`, and
`otel_*_exporter_release` symbols. Their headers are in every archive; their
binaries are only in the shared and combined archives.

On Linux and macOS, exporters resolve core symbols such as
`otel_runtime_set_exporter` and the allocation-tracking hooks through
process-wide dynamic lookup. When the core library is linked statically,
export the executable's symbols so that lookup succeeds:

```sh
# Linux
cc -rdynamic -std=c11 -Iinclude app.c lib/libotel_capi.a -pthread -ldl -lm -o app
# macOS
clang -Wl,-export_dynamic -std=c11 -Iinclude app.c lib/libotel_capi.a -pthread -lm -o app
```

When the core library is linked dynamically, no extra flag is needed, but the
shared core must be loadable at run time. On Windows, the loadable exporters
cannot resolve the core setter dynamically: `otel_*_exporter_register` returns
`OTEL_ERR_INTERNAL`. Register a callback exporter with
`otel_runtime_set_exporter` directly, as shown in the README.

## Building the variants from source

<!-- x-release-please-start-version -->
```sh
make build                    # core static + shared libraries and exporters
make install PREFIX=/path/to/prefix
python3 scripts/package_release.py \
  --target x86_64-unknown-linux-gnu --version v0.2.1 \
  --target-dir target/x86_64-unknown-linux-gnu/release
```
<!-- x-release-please-end -->

The packaging script writes the combined, `-static`, and `-shared` archives
plus checksums into `dist/`. The release workflow uploads all of them as
release assets.
