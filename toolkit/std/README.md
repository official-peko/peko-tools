# std

The PekoScript standard library.

`std` is a normal PekoScript package, but it is installed once at the global
Peko root during toolchain setup and is always available. Projects do not list
it as an ordinary dependency; its version is tied to the installed
toolchain and compiler. Modules are reached through the `std::` prefix, for
example `std::collections` or `std::webview`.

## Modules

- `core` - the base types and the language prelude: numbers, strings,
  optionals, the object model, and the logical and comparison traits.
- `collections` - dynamic arrays, maps, and the common collection traits.
- `crypto` - hashing and cryptographic primitives, backed by a vendored
  libsodium build.
- `fs` - files and directories: reading, writing, and path operations.
- `io` - console input and output.
- `json` - JSON parsing and serialization.
- `xml` - XML parsing and serialization.
- `random` - random number generation.
- `sockets` - TCP and UDP sockets, a TLS transport backed by BearSSL, and a
  WebSocket client.
- `threads` - native threads and synchronization primitives.
- `webview` - a native webview window: WKWebView on macOS, WebKitGTK on Linux,
  and WebView2 on Windows, with iOS and Android backends.
- `runtime` - low-level bridges to the garbage collector and runtime.
- `lexer` - a lexer used by tooling.

## Native code

Several modules compile from C, Objective-C, and C++ sources under `c/`. The
garbage collector and runtime are built from `c/runtime`. Prebuilt static
libraries for libsodium and BearSSL are vendored per platform and architecture.
The desktop webview links the platform WebView frameworks at the final link.

## License

MIT. Authored by Preston Brown.
