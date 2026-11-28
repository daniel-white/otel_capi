# Changelog

## [0.3.0](https://github.com/daniel-white/otel_capi/compare/v0.2.1...v0.3.0) (2026-11-28)


### Features

* **http:** add shared native-tls HTTP client ([2f816f3](https://github.com/daniel-white/otel_capi/commit/2f816f35ed9a3eeee2ec1b0a8bad7e2b1b0b261e))
* **transports:** add loadable HTTP, gRPC and stdout libraries ([8e95059](https://github.com/daniel-white/otel_capi/commit/8e95059f8ca554f162b28c46871f0508d3244af5))


### Bug Fixes

* restore CI checks after exporter refactor ([c882a9b](https://github.com/daniel-white/otel_capi/commit/c882a9befca0f9360d3056dd1654b86a65e1671c))
* use Windows-compatible timestamp test ([eddd9c0](https://github.com/daniel-white/otel_capi/commit/eddd9c0015e245a1f9547a3adf573a06af02cc38))


### Code Refactoring

* **core:** move C API into workspace crate ([3fef1aa](https://github.com/daniel-white/otel_capi/commit/3fef1aade1f5705e33dfa5df4f295ba03cb9727a))
* rename transports to exporters ([a58b86a](https://github.com/daniel-white/otel_capi/commit/a58b86ab814dfaa692be2f629d671ca68c20c263))


### Build System

* configure system TLS for source builds ([b111572](https://github.com/daniel-white/otel_capi/commit/b11157224db5020de5457fd8c2735530c88b8939))
* enforce strict linting and Miri validation ([80276b7](https://github.com/daniel-white/otel_capi/commit/80276b71f035dbafce746340569147ae65de11ff))


### Tests

* add coverage for Miri-validated tests ([ce2e16a](https://github.com/daniel-white/otel_capi/commit/ce2e16a6b6329d47f14275c51924e3dd9b56c322))
* expand workspace-wide Rust coverage ([213e291](https://github.com/daniel-white/otel_capi/commit/213e291a5babf16ab5e81b9ec3a598931da897a2))
* reach 90% workspace coverage without build scripts ([458c315](https://github.com/daniel-white/otel_capi/commit/458c315ff4af3fb8777cbc4b7db1ff43981ab07d))


### Miscellaneous Chores

* document and publish static/shared library variants ([#4](https://github.com/daniel-white/otel_capi/issues/4)) ([ce67ef4](https://github.com/daniel-white/otel_capi/commit/ce67ef4409e1fd1a7e4ede6ea2c67b2e6c18c032))
* sync rebased workspace lockfile ([17c59d3](https://github.com/daniel-white/otel_capi/commit/17c59d3dd7e483dc8f89111112c2588db1a0f4d4))

## [0.2.1](https://github.com/daniel-white/otel_capi/compare/v0.2.0...v0.2.1) (2026-11-26)


### Bug Fixes

* **ci:** inline release asset uploads ([e6d32db](https://github.com/daniel-white/otel_capi/commit/e6d32db1f8e264901363a14042a9bea1daa1dda0))
* **ci:** upload release assets with gh CLI ([d488ebf](https://github.com/daniel-white/otel_capi/commit/d488ebf9a13d5f2bcccf4b111b83cc8e02a7a7ac))
* **ci:** use inline gh release upload commands ([554190e](https://github.com/daniel-white/otel_capi/commit/554190e1b917d93315ef06850e4c35a1dfb25ffe))


### Documentation

* clarify release workflow trigger ([b0244d6](https://github.com/daniel-white/otel_capi/commit/b0244d66462a2d1cb89dd769e327142cb00fc76f))


### Continuous Integration

* dispatch release builds after tagging ([b7dba8a](https://github.com/daniel-white/otel_capi/commit/b7dba8a91cc81f104908a04575ef6f29b0438f6e))

## [0.2.0](https://github.com/daniel-white/otel_capi/compare/v0.1.0...v0.2.0) (2026-11-26)


### Features

* add Arc-backed caller-owned handles with retain/release ([084dfdb](https://github.com/daniel-white/otel_capi/commit/084dfdb292e039049728bb3e746b30f5c4bb8148))
* add array-based attribute APIs ([9d31e8e](https://github.com/daniel-white/otel_capi/commit/9d31e8e33947ad593a8f99dad905760c06ee98e6))
* add C attribute builders and NULL-terminated varargs parsing ([9aa8dbc](https://github.com/daniel-white/otel_capi/commit/9aa8dbc2765daef5d8be4f5948634b2270f3fb36))
* add explicit runtime handle and trace APIs ([9bcff3f](https://github.com/daniel-white/otel_capi/commit/9bcff3fe5c824311570e69c67d57ea67f647f4ae))
* add FFI result codes and thread-local error buffer ([77ceb89](https://github.com/daniel-white/otel_capi/commit/77ceb8938051fbb4d63f1a5f5b67e37f7ad31650))
* add idiomatic C header helpers ([5bde5d2](https://github.com/daniel-white/otel_capi/commit/5bde5d2f791ff7a65e27499a58218622dbb027bb))
* add length-aware strings, numeric IDs, and log macros ([c95dd12](https://github.com/daniel-white/otel_capi/commit/c95dd1279569ca429fe1d0203cf7f899f8a4c5b5))
* add logger provider and span-correlated log emission ([57b9777](https://github.com/daniel-white/otel_capi/commit/57b9777deffb2b7956b2aca021a7206270a4f5a5))
* add metric provider and instrument APIs ([2774e45](https://github.com/daniel-white/otel_capi/commit/2774e457d3cfe5362eac7320ce385300649a8bfe))
* add observable metric instruments ([c59422b](https://github.com/daniel-white/otel_capi/commit/c59422bc8c7c6c540727ecd064a56a36a39a08f7))
* add propagation, span options, and fix attribute varargs ([85b24ba](https://github.com/daniel-white/otel_capi/commit/85b24ba0c35554c2547c20249896758f76452ca1))
* add structured log records ([fb4659d](https://github.com/daniel-white/otel_capi/commit/fb4659de361e1ae29cf06835999451af20270faa))
* add typed helpers for semantic attributes ([3dc14f3](https://github.com/daniel-white/otel_capi/commit/3dc14f38475bb7826a41b8eb4f4706f83c037e99))
* **c-api:** harden ABI and add C ergonomics ([72892c8](https://github.com/daniel-white/otel_capi/commit/72892c8c072c1d383ff9ed4adb7e32fc455a459e))
* deferred span close and cleanup validation ([f39e1f6](https://github.com/daniel-white/otel_capi/commit/f39e1f61802401486a13f8ff3f65d9c34bc76c62))
* generate C semantic-convention macros at build time ([155ca99](https://github.com/daniel-white/otel_capi/commit/155ca99db77570c07cfff5b95eecfba185b7d890))
* generate type-safe semconv attribute and metric helpers ([505f261](https://github.com/daniel-white/otel_capi/commit/505f26135c3c230f2f9c1afa9bab221b3ff44910))
* read standard OTel exporter environment configuration ([6f4e8d6](https://github.com/daniel-white/otel_capi/commit/6f4e8d68d1ba77f0b1870db505393bef7c5fb710))
* use typed buffers for W3C propagation ([4fc5b70](https://github.com/daniel-white/otel_capi/commit/4fc5b7010848dbd26caf60658cf2feb46e43ca6d))


### Bug Fixes

* **build:** derive expected version macros from Cargo.toml ([e6305f7](https://github.com/daniel-white/otel_capi/commit/e6305f76f2cb87dd74a02b614e23eba47658a031))
* **ci:** allow release-please to open pull requests ([c6d2d6d](https://github.com/daniel-white/otel_capi/commit/c6d2d6d9fad4b6181ec02cbb25699576e85824d0))
* **ci:** link musl kitchen sink in Alpine container ([61f8b18](https://github.com/daniel-white/otel_capi/commit/61f8b184af9c28911a37f3ccf566be9a959a253d))
* **ci:** use supported macOS runners and serialize trace env tests ([7c0c776](https://github.com/daniel-white/otel_capi/commit/7c0c776a13417f2509ac19d65dd08779f9da853e))
* **example:** declare POSIX feature macro for setenv ([5fbe7c6](https://github.com/daniel-white/otel_capi/commit/5fbe7c64b93685661e5e66a7894449069857dc46))


### Code Refactoring

* compose generated header by API area ([9f6073b](https://github.com/daniel-white/otel_capi/commit/9f6073b760927279582f99fbced12ea2d10b873b))
* drop C++ support and expand C ABI consistency helpers ([65dc8b1](https://github.com/daniel-white/otel_capi/commit/65dc8b1b2391f93901c428554a189b75a2455c14))
* use a tagged union for attribute values ([f5bb58b](https://github.com/daniel-white/otel_capi/commit/f5bb58be20efd64f9fb3b3dea0ac5ca55445fa8c))


### Documentation

* add kitchen-sink C program for all signals ([fb5bcd4](https://github.com/daniel-white/otel_capi/commit/fb5bcd401a4207d3f53e55e4018ae1d7fb9a5d91))
* add threaded kitchen-sink workload ([f496a34](https://github.com/daniel-white/otel_capi/commit/f496a34a0e6b3e3448592c4f3d5f93237d585357))
* document C ergonomics and threaded example ([f924ccc](https://github.com/daniel-white/otel_capi/commit/f924cccbb712ffdef3915406c9ed9ef0bf81b5eb))
* document explicit providers and expand API checks ([e9f71a0](https://github.com/daniel-white/otel_capi/commit/e9f71a06a306c28ed6d08f00f45abd0a1486893f))
* hide internal handle pointers from API docs ([4786c79](https://github.com/daniel-white/otel_capi/commit/4786c79fb3f34df5161f0592e1ab889d85a08f68))
* make caller-owned handle convention explicit in API ([cc455e8](https://github.com/daniel-white/otel_capi/commit/cc455e8aa9e07bb2c21c6af47f1ff0990fdf5271))
* publish generated API docs with releases ([9beed13](https://github.com/daniel-white/otel_capi/commit/9beed13b1d276272a9cc71724a6ed3051d13d62c))


### Build System

* add direnv flake integration ([f8d9b19](https://github.com/daniel-white/otel_capi/commit/f8d9b1916f2ac05281f5b36920cd2d36c216832b))
* add Makefile and README ([9256cd2](https://github.com/daniel-white/otel_capi/commit/9256cd22420a775eac94a98b0e7146225c528ac5))
* add nix flake with fenix rust toolchain ([b5a7cfb](https://github.com/daniel-white/otel_capi/commit/b5a7cfbe83455e877e2a02bcce1e0b459e1421cb))
* ignore direnv cache ([4111c3b](https://github.com/daniel-white/otel_capi/commit/4111c3bda2ed3189bfe094a9c67b9d9cb1233016))
* lock nix flake inputs ([3bc57cd](https://github.com/daniel-white/otel_capi/commit/3bc57cd76a47ed51bd764240f33a510e97e06791))
* scaffold Rust library and cbindgen header generation ([1cb2119](https://github.com/daniel-white/otel_capi/commit/1cb2119e97d6110746784d72ef42503696a223bd))
* stop tracking generated headers ([45a7dae](https://github.com/daniel-white/otel_capi/commit/45a7dae3168bb14ed902fd6fce86ca5ac78f1626))
* support x86_64-darwin in flake outputs ([6258d79](https://github.com/daniel-white/otel_capi/commit/6258d79aab0d4f3c926a98dcfb5e0886e4e537af))
* use default nix systems ([079c1c8](https://github.com/daniel-white/otel_capi/commit/079c1c8bc35f0280064d637c4c06cca1a4c3e23a))
* use rust-overlay toolchain and expose gcc/clang ([aaef553](https://github.com/daniel-white/otel_capi/commit/aaef55306914ab182dc54adbd9b40aebeea4eb51))


### Continuous Integration

* add musl x86_64 release archive ([5d4ddad](https://github.com/daniel-white/otel_capi/commit/5d4ddadce3a05087fbdedfe0c9b2a2d69888126f))
* add release automation and package manifests ([1c18c02](https://github.com/daniel-white/otel_capi/commit/1c18c024819c87011d93bb22014281ada6aa904b))
* publish docs to GitHub Pages on release ([203b443](https://github.com/daniel-white/otel_capi/commit/203b443075796596dbad93fa54c8e9524bf60ec1))


### Tests

* add musl kitchen sink check ([cd238e0](https://github.com/daniel-white/otel_capi/commit/cd238e0d445d0f74ed555a237851290bf9bc503a))


### Miscellaneous Chores

* clean up Rust compiler warnings ([ec7864c](https://github.com/daniel-white/otel_capi/commit/ec7864c32bd54c96faa3ef4b0904fc33b0f51bf9))
* rename project to otel-capi ([fd4bfd0](https://github.com/daniel-white/otel_capi/commit/fd4bfd00439759e00f1d8e2efc2994fffb39745f))
* update flake.lock ([c49d37d](https://github.com/daniel-white/otel_capi/commit/c49d37d5c0f05700ad8c33e1c23f145edb17a3f3))
* update flake.lock ([cc4369c](https://github.com/daniel-white/otel_capi/commit/cc4369ccea5462e1a8228e740b052ce9f4a08c5a))
* use Apache-2.0 license ([dfb3330](https://github.com/daniel-white/otel_capi/commit/dfb3330a126e71133cf18747c07f6ea0aafcda39))
