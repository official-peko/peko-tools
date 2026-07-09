# pekoui

The PekoUI framework: a native webview host and UI runtime for PekoScript.

pekoui builds cross-platform desktop and mobile apps whose interface is a web
front end running in the platform's native webview, driven by PekoScript over a
message bridge. macOS uses WKWebView, Linux uses WebKitGTK, Windows uses
WebView2, and there are iOS and Android backends.

pekoui is a standalone package and is not auto-imported. Add it as a dependency,
then import the whole package under one alias or reach individual modules:

```peko
import pekoui as ui;

let app = ui::app::from_bundle();
```

## Modules

- `app` - the application entry point and window lifecycle: create the window
  from the bundled web assets, run the event loop, and manage windows.
- `webview` - the native webview surface: navigation, custom window controls,
  transparency, and the platform web engine.
- `bridge` - the message channel between PekoScript and the web front end.
  Register handlers with `application.on(name, closure)` and push events with
  `application.emit(name, json)`; the web calls `peko.invoke` and `peko.on`.
- `assets` - serving the bundled web assets to the webview.
- `storage` - persistent key-value storage for the app.
- `keychain` - secure secret storage backed by the OS keychain.
- `menu` - the native application menu bar, with accelerators, on each platform.
- `dialog` - native file and message dialogs.

## Client SDK

The `client/` directory holds the JavaScript client SDK (`@peko/client`) that
the web front end imports to talk to the bridge, read platform information, and
control native window chrome. React and Vue adapters are included.

## Native code

The webview host, asset server, dialogs, menus, and deep-link handling compile
from C, Objective-C, and C++ sources under `c/`. Only a project that depends on
pekoui compiles and links these, so a command-line or non-UI binary never pulls
in the platform WebView. The platform WebView frameworks (WebKit and Cocoa on
macOS, WebKit on iOS, WebKitGTK on Linux, and the WebView2 loader on Windows)
are linked only into UI binaries.

## License

MIT. Authored by Preston Brown.
