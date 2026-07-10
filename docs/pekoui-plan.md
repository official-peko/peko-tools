# pekoui - the Peko UI package (SSG host, native bridge, core app)

Status: design, approved by Preston 2026-07-03. Formalizes Master Reference
Part III (sections 16-19) with the concrete package, the native bridge, the core
app, the native menu, and the client SDKs. Where this doc and the master
reference conflict on wording, this doc governs for the pekoui package; the master
reference still governs the wider platform.

## 1. Goals and decisions

- One package named `pekoui`, built in this repo at `toolkit/pekoui/` next to
  `toolkit/std/`. It consolidates `webview` + `assets` + `storage` + the native
  bridge + the core app + native menus. It is a separate package from `std` and
  is not auto-imported: an app opts in with `import pekoui`.
- `webview` moves out of `std` into `pekoui`. Only UI apps then link
  WebKit/WebView2/WebKitGTK, which also removes the current waste where every
  Peko binary links the webview stack.
- The three UI paths from the master reference stay: SSG, SSR, PekoUI-native.
  This wave delivers SSG end to end (React first) and lays the seam so SSR is an
  endpoint swap, not a rewrite.
- The native side and the app talk over a WebSocket RPC channel. This is the
  same shape the V1 app used and is transport-identical for local (SSG mock
  server) and remote (SSR platform server) later.
- The user writes a tiny core app. The compiler embeds project facts in a
  built-in `bundle::` module, so the app auto-detects its framework and needs no
  manual wiring. The CLI drives the whole build (detect the framework, build it,
  embed the output, link the host).

## 2. Package layout

```
toolkit/pekoui/
  peko.toml                     # [native] table, deps on std
  pekoui.lib.peko                # re-exports the public surface
  app.peko                      # the core App (lifecycle, routing, dispatch)
  bridge.peko                   # WebSocket RPC server + message protocol
  webview.peko                  # moved from std (unchanged API)
  assets.peko                   # bundled assets + loopback HTTP server (from V1)
  storage.peko                  # KV (SQLite) + keychain (from V1)
  menu.peko                     # native menu abstraction (desktop; no-op mobile)
  c/
    webview/...                 # moved verbatim from std/c/webview
    assets/...                  # ported from V1 assets C (HTTP server, bundle readers)
    storage/...                 # ported from V1 storage C (sqlite, keychain)
    menu/...                    # new: NSMenu / HMENU / GtkMenuBar backends
```

Dependencies: `pekoui` depends on `std` (sockets/websocket for the bridge, json,
crypto, fs, threads). `std::webview` is deleted after the move; its C and `.peko`
live under `pekoui`. The Android bundler's dex collection already scans reachable
packages, so the prebuilt `classes.dex` moving to `pekoui/c/webview/android/`
needs no bundler change.

Client SDKs live outside this repo and publish to their own registries:

```
@peko/client   (npm)   # JS frameworks: React/Vue/Svelte. Primary.
peko-client    (pip)   # Python, SSR only. Later.
```

## 3. The `bundle::` module (compiler-injected)

A new built-in module whose values the compiler sets at build time from the
manifest, exactly like the existing injected globals in
`peko-llvm/.../builders/globals.rs` (`storage::application_identifier`,
`assets::asset_debug_dir`, `ui::debug_*`).

Surface:

```
bundle::name           string     # [project].name
bundle::identifier     string     # [project].bundle
bundle::app_id         string     # [project].app_id (platform-assigned, may be empty)
bundle::version        string     # [project].version
bundle::framework      Framework  # enum: PekoUI | SSG | SSR
bundle::framework_name string     # "react" | "vue" | "sveltekit-static" | ...
bundle::debug          bool       # debug vs release build
```

`Framework` is an enum in `bundle`. The compiler injects the concrete values so
`pekoui::App::from_bundle()` reads them with no config. This keeps the user's core
app free of build-path branching.

## 4. The core app

The user writes a small host. For a pure SSG app it can be a single line; adding
native handlers is a few more.

```peko
import pekoui;

fn on_start() {
    let app = pekoui::App::from_bundle()      // reads bundle:: (framework, name, ...)

    // Optional: app-defined native connects, reachable from JS as peko.camera.capture()
    app.on("camera.capture", closure(params: string) => string {
        return capture_and_return_json(params)
    })

    app.run()                                // serve + open webview + route; blocks
}
```

`App::from_bundle()` selects the path from `bundle::framework`:

- SSG: start the loopback asset HTTP server (assets module), start the WS bridge,
  point the webview at `http://127.0.0.1:<port>/`, install the SPA fallback and
  deep-link routing.
- SSR (later): point the webview at the hosted URL and the bridge at the platform
  WS endpoint; everything else identical.
- PekoUI-native (later): the native rendering path.

`App` lifecycle:

1. Construct: create the webview (from `bundle::name`, a default or manifest
   size), register the built-in capabilities (storage, keychain, assets).
2. `on(method, handler)`: register an app-defined native connect under a
   `namespace.method` name.
3. `set_menu(menu)`: install a native menu (section 7).
4. `run()`: bind the injected startup script (`window.__PEKO__`), start servers,
   load the URL, and drive the webview event loop (parks the thread for the GC as
   the current webview `run()` does).

The webview, drag/frameless/controls, and transparency built earlier are the
window layer under `App`.

## 5. Native bridge: WebSocket RPC

### Transport

- The webview can only load a page over HTTP, so static assets are served over
  loopback HTTP (the V1 asset server). Interop is a separate WebSocket, because a
  socket is bidirectional and stays identical whether the page is local (SSG) or
  remote (SSR).
- The bridge is a loopback WS server built on `std`'s websocket support. It binds
  `127.0.0.1:<ephemeral port>` and mints a per-run token.
- Before the page loads, the webview injects:
  `window.__PEKO__ = { url: "ws://127.0.0.1:<port>/", token: "<token>" }`.
  The client SDK reads this. For SSR (browser, no injection) the SDK falls back to
  a same-origin `/__peko__` socket.

### Message protocol (JSON)

Request (app -> native):
```json
{ "t": "call", "id": 7, "method": "keychain.get", "params": { "key": "token" } }
```
Response (native -> app):
```json
{ "t": "reply", "id": 7, "ok": true, "result": { "value": "..." } }
{ "t": "reply", "id": 7, "ok": false, "error": { "code": "not_found", "message": "..." } }
```
Event (native -> app, unsolicited push):
```json
{ "t": "event", "name": "navigate", "data": { "path": "/settings" } }
{ "t": "event", "name": "menu", "data": { "id": "file.save" } }
```

### Dispatch

Handlers are registered under `namespace.method`. Built-ins are registered by
`App` (`storage.*`, `keychain.*`, `assets.*`); app handlers are registered by
`app.on(...)`. The dispatcher looks up the method, runs the Peko closure with the
params JSON on the bridge thread (GC rules: the bridge thread is attached and
parks between calls, matching the webview bind trampoline), and replies with the
returned JSON.

Built-in handlers marshal typed values through the `[serial]` serialization
framework rather than hand-rolling JSON.

## 6. Built-in capabilities and custom native-connects

### Built-ins (v1)

- `storage.*` - key/value on the per-app SQLite DB: `get`, `set`, `remove`,
  `keys`, `clear`.
- `keychain.*` - secure secrets via the OS keychain with the encrypted-file
  fallback: `get`, `set`, `remove`.
- `assets.*` - bundled asset access: `url(name)`, `bytes(name)`, `list(prefix)`.

The surface is intentionally open: adding a capability is registering another
namespace, no protocol change.

### Custom native-connects that feel built-in

The JS SDK is a `Proxy`, so any namespaced method a user registers natively is
reachable with the same ergonomics as a built-in, with no client-side
declaration:

```js
// user native side (Peko):
app.on("camera.capture", closure(params) => string { ... })

// user app side (JS), nothing to import or declare:
const photo = await peko.camera.capture({ facing: "front" })
```

Mechanism: `peko` is a `Proxy` whose `get(ns)` returns a second `Proxy` whose
`get(method)` returns `(params) => invoke(`${ns}.${method}`, params)`. Built-in
namespaces resolve to typed wrappers first; unknown namespaces fall through to the
generic namespaced invoker. This gives `peko.<anything>.<method>()` for free while
keeping types on the built-ins.

## 7. Native menu

`pekoui::menu` is a desktop menu abstraction; every call is a no-op on iOS and
Android.

API:
```peko
let menu = new pekoui::Menu()
let file = menu.submenu("File")
file.item("Save", "file.save", "CmdOrCtrl+S")   // label, action id, accelerator
file.separator()
file.item_role("Close", MenuRole::Close)         // standard native role
app.set_menu(menu)
```

- Backends: macOS `NSMenu`/`NSMenuItem` on the application main menu (the global
  menu bar, which macOS apps must have); Windows `HMENU` via `SetMenu` on the
  window (decorated windows; frameless apps typically draw their own bar and use
  this for the app/context menu); Linux `GtkMenuBar`.
- Roles map to native standard items (Quit, About, Copy, Paste, Minimize, Close,
  Fullscreen). Roles are handled natively.
- A plain item carries an action id. Clicking it fires an event to the app:
  `{ "t": "event", "name": "menu", "data": { "id": "file.save" } }`, so the JS app
  handles it with `peko.on("menu", e => ...)`. An item may instead take a Peko
  closure for pure-native handling.
- Accelerators are parsed cross-platform (`CmdOrCtrl` maps to Cmd on macOS, Ctrl
  elsewhere).

## 8. Routing and deep links

For an SSG SPA, routing is client-side; the native app makes it behave like a
real app:

- SPA fallback: the asset HTTP server serves `index.html` for any path that is
  not a file, so client-side routes deep-link correctly on load.
- Native -> app navigation: `app.navigate(path)` pushes a `navigate` event; the
  SDK applies it to the framework router (History API), so native menus, deep
  links, and the platform can drive navigation.
- App -> native sync: the SDK observes History changes and notifies native
  (`route.changed`) so menu state, window title, and later the platform stay in
  sync.
- OS deep links (custom scheme / universal links) resolve to `app.navigate(path)`
  through the same event, so one code path serves in-app and OS-level deep links.

## 9. Client SDK (`@peko/client`, npm)

- `connect()`: auto. Reads `window.__PEKO__` (webview/SSG); else same-origin
  `/__peko__` (SSR). Opens the WS, sends the token, resolves when ready.
- `peko`: the `Proxy` described in section 6. Built-in typed namespaces
  `peko.storage`, `peko.keychain`, `peko.assets`; generic fallthrough for the
  rest; `peko.invoke(method, params)` as the explicit escape hatch.
- `peko.on(event, cb)` / `peko.off`: native push events (`navigate`, `menu`,
  and app events).
- Framework glue: tiny optional adapters (`@peko/client/react`, `/vue`) that wire
  `navigate` to the router and mount a provider. The core stays framework-neutral.
- Window chrome abstraction: the SDK hides the raw `data-peko-*` attributes behind
  a clean interface so an app builds a native-feeling custom titlebar without
  knowing the convention.
  - Framework-neutral: `peko.titlebar(el)` marks an element as the drag region and
    `peko.noDrag(el)` / `peko.control(el, "minimize"|"maximize"|"close")` mark
    children; plus a `<peko-titlebar>` custom element for plain HTML.
  - React adapter: a `<Titlebar>` component (draggable header with optional
    built-in min/max/close buttons) and a `useDraggable()` hook returning props to
    spread onto any element. Vue gets the equivalent.
  These compile down to the `data-peko-drag` / `data-peko-no-drag` /
  `data-peko-minimize|maximize|close` attributes the webview shim already handles,
  so the native side is unchanged.
- `peko-client` (pip) mirrors the call/reply/event protocol for SSR server code.
  SSG is JS-only for now.

## 10. CLI build pipeline (SSG)

`peko build` drives everything; the user runs one command.

1. Read `[ui] framework = "ssg"` and the framework name (react/vue/...). Detect
   the JS toolchain (`package.json`, lockfile).
2. Build the framework: install deps and run its build script, producing the
   static output (`dist/` or `build/`). Failures surface as build errors.
3. Embed the output as assets through the existing asset-embedding path (the same
   machinery behind `assets::asset_debug_dir`), so the loopback server can serve
   them at runtime.
4. Inject `bundle::` values (framework, name, ids, version) into codegen.
5. Compile the host `on_start` (user-provided, or a generated default that is
   `pekoui::App::from_bundle().run()` when the project has no `.peko`) and link
   `pekoui`.
6. Bundle per platform as today (macOS `.app`, Windows `.exe`, AppImage, `.apk`,
   `.ipa`).

Setup lives in the existing `peko project new` (no separate command). The prompt
gains selectors: UI path (SSG / SSR / PekoUI-native), and for SSG the framework
(React / Vue / SvelteKit-static / ...). `project new` then scaffolds the JS app,
a one-line `main.peko` host, and the manifest `[ui]` block, so a newcomer starts
from a working native+React app.

## 11. SSR-forward seam (design only, not built now)

The pieces that make SSR an endpoint swap rather than a rewrite:

- The `App` reads its path from `bundle::framework`; SSR sets the webview URL and
  the bridge endpoint to the platform instead of loopback.
- The RPC protocol and the client SDK are transport-identical; only `connect()`'s
  endpoint differs, which it already auto-detects.
- Capability handlers are addressed by name, so the platform can serve
  server-side capabilities under the same names the device serves locally.

SSR hosting itself (AWS App Runner/Fargate, deploy, TLS) remains the master
reference Part 18 project.

## 11b. Capabilities and the manifest

- A `[capabilities]` block is added to `peko.toml` now. An app declares the
  native capabilities its JS side may reach (`storage`, `keychain`, `assets`,
  and app-defined ones like `camera`).
- Capabilities inherit up the dependency graph: a package declares the
  capabilities it uses in its own `[capabilities]`, and the app's effective set
  is the union of its own plus every reachable package's. So adding a package
  that needs a new capability auto-adds it to the app with no manual edit. The
  resolver already walks reachable packages (dex collection, native link args),
  so this reuses that walk.
- v1 exposes the declared capabilities openly over loopback; the block exists so
  per-permission gating and user-facing consent are a later addition rather than
  a breaking change.

## 11c. Compiler and CLI cleanup (prerequisite)

This wave removes two outdated mechanisms and replaces the useful part with
`bundle::`.

- Remove all compiler-injected globals: `ui::debug_styles_dir`, `ui::debug_mode`,
  `assets::asset_debug`, `assets::asset_debug_dir`, `storage::application_identifier`,
  and the codegen context fields that drive them (`compiled_styles_folder`,
  `asset_debug_folder`, `application_id`). The only compile-time project info is
  now the `bundle::` module (section 3); `bundle::identifier` replaces
  `storage::application_identifier`.
- Remove the UI hot-reload development interface: `peko run`'s "hot reload" path
  for UI projects, `.peko/incremental/run`, and the debug-folder serving of
  styles and assets. It is outdated and unneeded at this stage.
- Keep the incremental build cache (`.peko/incremental/`, `compile_project`),
  which is a separate build-speed mechanism, unless decided otherwise.

## 11d. Import resolution (package roots and grouped imports)

Today `lib.peko` is comment-only; submodules resolve as sibling files, so
`import std::core` loads `core.peko` under the `std::core` namespace, but
`import std` exposes nothing because `lib.peko` declares nothing. Two additions:

- `lib.peko` can declare its public submodules with an `export` construct
  (explicit), e.g. `export { core, collections, io }`. A package root with no
  `export` implicitly exports every sibling module (implicit). `import std` then
  registers each exported submodule as a resolvable `std::<sub>` namespace, so
  `std::core::Foo` works after just `import std`.
- Grouped submodule import: `import std::{core, collections}` desugars to
  importing each listed submodule under its `std::<sub>` namespace. It composes
  with the existing `import { * } from std::core` unpack and `import x as y`
  alias forms, which are unchanged.

This is a general language feature (not pekoui-specific) but lands here because
`pekoui` wants `import pekoui` to expose `pekoui::App`, `pekoui::storage`,
`pekoui::menu`, and friends cleanly.

## 12. Build order

Foundational compiler/CLI work comes first (it unblocks the rest), then the
package, then the SSG pipeline.

1. Cleanup (section 11c): remove the injected globals + the UI hot-reload dev
   interface; keep the build cache. Land the `bundle::` module (section 3) as the
   single compile-time project-info source.
2. Import resolution (section 11d): `export` in `lib.peko` + grouped
   `import a::{x, y}`. Convert `std`/`pekoui` roots to declare their submodules.
3. Create `toolkit/pekoui`; move `webview` from `std` (C + `.peko` + Android dex),
   delete `std::webview`, verify all five platforms still build.
4. Port `assets` and `storage` from V1 to V2 (C rewrite + `.peko.h` + `.peko`,
   following the std native-port pattern), under `pekoui`.
5. Add the `[capabilities]` manifest block + inheritance walk (section 11b).
6. Build `bridge.peko` (WS RPC server + dispatch) and a minimal `App` that serves
   a hand-placed React `dist/` over loopback + WS and opens the webview. Prove the
   channel with a round-trip.
7. Register the built-in capabilities; add `app.on`; ship `@peko/client` with the
   `Proxy` and typed built-ins. Get React calling `peko.keychain`/`peko.storage`.
8. Add routing/deep-link sync and the native menu (drawn-in-HTML on frameless
   Windows, abstracted so the user just calls `set_menu`).
9. Wire the SSG pipeline into `peko project new` + `peko build` (selectors,
   detect, build, embed) so one flow produces the native app from a React project.
10. Later waves: SSR path + `peko-client` (pip), more frameworks, PekoUI-native.

## 13. Resolved defaults

- Default SSG window size when the manifest gives none: 500x500.
- Native menu on frameless Windows: abstracted and wired so it "just works" -
  `set_menu` renders the menu in HTML for the frameless custom titlebar, while
  decorated windows still get the native `HMENU`. The user does not choose.
- Capabilities: `[capabilities]` block added now, inherited up the dependency
  graph (section 11b), exposed openly over loopback in v1.
