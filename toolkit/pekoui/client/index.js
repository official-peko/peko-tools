// @peko/client - client SDK for the Peko native bridge.
//
// The native App host injects `window.__PEKO__ = { url, token }` before the
// page loads. Importing this module connects to that loopback WebSocket,
// authenticates with the token, and exposes a `peko` object.
//
// Call native handlers as peko.<namespace>.<method>(params):
//
//   import { peko } from '@peko/client'
//   const pong = await peko.sys.ping({ hello: 'world' })
//
// Any handler the native side registers under "namespace.method" is reachable
// this way with no client-side declaration. peko.invoke(method, params) is the
// explicit escape hatch, and peko.on(event, cb) / peko.off subscribe to native
// push events (navigate, menu, and app events).

let socket = null;
let nextId = 1;
const pending = new Map();     // call id -> { resolve, reject }
const listeners = new Map();   // event name -> Set<callback>

// The launch route, when the OS opened the app with a deep-link URL. It arrives
// two ways: injected into window.__PEKO__.initialRoute before load (desktop and
// Android), or fetched from the bridge once connected (iOS, whose launch URL is
// only known after the UI starts). Either way it is delivered to the navigate
// subscribers, or held for the first one that appears.
let pendingInitialRoute = null;

function deliverInitialRoute(path) {
  if (!path || typeof path !== 'string') {
    return;
  }
  const set = listeners.get('navigate');
  if (set && set.size > 0) {
    set.forEach(function (callback) {
      try {
        callback({ path: path });
      } catch (error) {
        // A listener throwing must not stop the others.
      }
    });
  } else {
    pendingInitialRoute = path;
  }
}

let resolveReady, rejectReady;
const ready = new Promise((resolve, reject) => {
  resolveReady = resolve;
  rejectReady = reject;
});

// Resolve the bridge endpoint: the injected config in a native webview, or a
// same-origin socket for a server-rendered page in a plain browser.
function endpoint() {
  const injected = (typeof window !== 'undefined') ? window.__PEKO__ : null;
  if (injected && injected.url) {
    return { url: injected.url, token: injected.token || null };
  }
  if (typeof location !== 'undefined' && location.host) {
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return { url: scheme + '//' + location.host + '/__peko__', token: null };
  }
  return null;
}

function connect() {
  const target = endpoint();
  if (!target) {
    rejectReady(new Error('no Peko bridge endpoint'));
    return;
  }

  socket = new WebSocket(target.url);

  socket.addEventListener('open', function () {
    socket.send(JSON.stringify({ t: 'auth', token: target.token }));
  });

  socket.addEventListener('message', function (event) {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch (error) {
      return;
    }

    if (message.t === 'ready') {
      resolveReady();
      // Fetch a launch route the platform delivers after connect (iOS). On the
      // platforms that injected it into the boot config, take_initial is already
      // consumed, so this resolves empty and delivers nothing.
      invoke('deeplink.initial').then(deliverInitialRoute).catch(function () {});
      return;
    }

    if (message.t === 'reply') {
      const waiter = pending.get(message.id);
      if (!waiter) {
        return;
      }
      pending.delete(message.id);
      if (message.ok) {
        waiter.resolve(message.result);
      } else {
        const info = message.error || {};
        const failure = new Error(info.message || 'call failed');
        failure.code = info.code;
        waiter.reject(failure);
      }
      return;
    }

    if (message.t === 'event') {
      const set = listeners.get(message.name);
      if (set) {
        set.forEach(function (callback) {
          try {
            callback(message.data);
          } catch (error) {
            // A listener throwing must not break dispatch to the others.
          }
        });
      }
      return;
    }

    if (message.t === 'error') {
      rejectReady(new Error(message.error || 'bridge error'));
    }
  });

  socket.addEventListener('close', function () {
    socket = null;
  });
}

// Call a native handler by "namespace.method" name. Resolves with the handler
// result, rejects with an Error carrying the native error code.
function invoke(method, params) {
  return ready.then(function () {
    const id = nextId++;
    return new Promise(function (resolve, reject) {
      pending.set(id, { resolve: resolve, reject: reject });
      socket.send(JSON.stringify({
        t: 'call',
        id: id,
        method: method,
        params: params === undefined ? null : params,
      }));
    });
  });
}

// Subscribe to a native push event. Returns an unsubscribe function.
function on(event, callback) {
  let set = listeners.get(event);
  if (!set) {
    set = new Set();
    listeners.set(event, set);
  }
  set.add(callback);
  // Replay a pending launch route to the first navigate subscriber, deferred so
  // the subscriber finishes registering first.
  if (event === 'navigate' && pendingInitialRoute !== null) {
    const route = pendingInitialRoute;
    pendingInitialRoute = null;
    Promise.resolve().then(function () {
      callback({ path: route });
    });
  }
  return function () {
    off(event, callback);
  };
}

function off(event, callback) {
  const set = listeners.get(event);
  if (set) {
    set.delete(callback);
  }
}

// Route sync. The app's route lives in the web UI's history, so native code
// (menus, deep links, window state) can only know it if the UI reports it.
// Patching pushState/replaceState and listening for popstate/hashchange covers
// every history-based router, and reportRoute pushes the new path to native.
let lastRoute = null;

function currentPath() {
  if (typeof location === 'undefined') {
    return null;
  }
  return location.pathname + location.search + location.hash;
}

function reportRoute() {
  const path = currentPath();
  if (path === null || path === lastRoute) {
    return;
  }
  lastRoute = path;
  // Swallow the rejection when no bridge is present (a plain browser), so an
  // unconnected page does not log an unhandled rejection on every navigation.
  invoke('route.changed', { path: path }).catch(function () {});
}

function startRouteSync() {
  if (typeof window === 'undefined' || typeof history === 'undefined') {
    return;
  }
  const wrap = function (name) {
    const original = history[name];
    if (typeof original !== 'function' || original.__peko_wrapped) {
      return;
    }
    const wrapped = function () {
      const result = original.apply(this, arguments);
      reportRoute();
      return result;
    };
    wrapped.__peko_wrapped = true;
    history[name] = wrapped;
  };
  wrap('pushState');
  wrap('replaceState');
  window.addEventListener('popstate', reportRoute);
  window.addEventListener('hashchange', reportRoute);
  reportRoute();
}

// Window chrome. These set the data-peko-* attributes the native webview shim
// handles: a data-peko-drag element moves the window, data-peko-no-drag opts a
// region back out, and data-peko-minimize/maximize/close run a window control.
// They only take effect in a frameless window (webview set_decorations(false)).
function titlebar(element) {
  if (element && element.setAttribute) {
    element.setAttribute('data-peko-drag', '');
  }
  return element;
}

function noDrag(element) {
  if (element && element.setAttribute) {
    element.setAttribute('data-peko-no-drag', '');
  }
  return element;
}

function control(element, kind) {
  if (element && element.setAttribute &&
      (kind === 'minimize' || kind === 'maximize' || kind === 'close')) {
    element.setAttribute('data-peko-' + kind, '');
  }
  return element;
}

// Programmatic window controls, wrapping the low-level native bindings.
const windowControls = {
  minimize: function () {
    if (typeof window !== 'undefined' && window.__peko_minimize) {
      window.__peko_minimize();
    }
  },
  maximize: function () {
    if (typeof window !== 'undefined' && window.__peko_maximize) {
      window.__peko_maximize();
    }
  },
  close: function () {
    if (typeof window !== 'undefined' && window.__peko_close) {
      window.__peko_close();
    }
  },
};

// -- Platform --------------------------------------------------------------
// What the app runs on, so the UI can adapt: hide window chrome on mobile,
// draw a custom titlebar only on a frameless desktop window, and fall back to
// an HTML menu where there is no native menu bar. The OS is read from the user
// agent; whether the window is frameless is injected by the native host.
const platform = (function () {
  const injected =
    typeof window !== 'undefined' && window.__PEKO__ ? window.__PEKO__ : {};
  const ua =
    typeof navigator !== 'undefined' && navigator.userAgent
      ? navigator.userAgent
      : '';
  let os = 'unknown';
  if (/Android/i.test(ua)) os = 'android';
  else if (/iPhone|iPad|iPod/i.test(ua)) os = 'ios';
  else if (/Macintosh|Mac OS X/i.test(ua)) os = 'macos';
  else if (/Windows/i.test(ua)) os = 'windows';
  else if (/Linux|X11/i.test(ua)) os = 'linux';
  const mobile = os === 'android' || os === 'ios';
  // A mobile app has no movable, resizable window, so it is never frameless in
  // the chrome sense even though the native view has no titlebar. Treating it
  // as frameless would add a drag region that swallows taps.
  const frameless = !mobile && !!injected.frameless;
  // Whether the OS draws native window controls over the frameless content.
  // macOS keeps its traffic lights on a frameless window unless they are
  // hidden; Windows and Linux draw none. The native host reports this.
  const nativeControls = frameless && !!injected.nativeControls;
  // Whether the app chose an HTML menu over the native one. When set the native
  // menu is not installed, so the HTML menu renders on every desktop.
  const htmlMenu = !!injected.htmlMenu;
  // Whether a native menu bar is present. macOS keeps a global bar even on a
  // frameless window, and Linux packs its menu bar above the web view even
  // without decorations, so both always have one. Windows draws its menu in the
  // non-client area, so a frameless window has none. Mobile never has one. When
  // the app opts into an HTML menu there is no native bar anywhere.
  const nativeMenu = htmlMenu
    ? false
    : mobile
      ? false
      : os === 'macos'
        ? true
        : os === 'linux'
          ? true
          : !frameless;
  return {
    os: os,
    mobile: mobile,
    desktop: !mobile,
    frameless: frameless,
    // Whether the OS draws the window controls itself (macOS traffic lights).
    nativeControls: nativeControls,
    // A custom titlebar draws its own window controls only on a frameless
    // desktop window where the OS draws none: a decorated window has native
    // ones, macOS keeps its traffic lights, and mobile has none.
    windowControls: frameless && !nativeControls,
    // Left inset in pixels so titlebar content clears the native controls. The
    // macOS traffic lights sit at the top left of a frameless window.
    titlebarInset: nativeControls && os === 'macos' ? 78 : 0,
    nativeMenu: nativeMenu,
  };
})();

// Build a window-control group (minimize, maximize, close) as an opt-out drag
// region carrying the data-peko-* attributes the webview shim runs.
function buildControls() {
  const bar = document.createElement('div');
  bar.className = 'peko-controls';
  bar.setAttribute('data-peko-no-drag', '');
  [
    ['minimize', '−'],
    ['maximize', '□'],
    ['close', '×'],
  ].forEach(function (pair) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'peko-control peko-control-' + pair[0];
    button.setAttribute('data-peko-' + pair[0], '');
    button.setAttribute('aria-label', pair[0]);
    button.textContent = pair[1];
    bar.appendChild(button);
  });
  return bar;
}

// Enhance an element as the app toolbar/titlebar, adapting to the platform. On
// a frameless desktop window it becomes the drag region and, when controls are
// requested, hosts the window controls; on mobile it hides unless keepOnMobile
// is set; on a decorated window it stays as ordinary content, since the native
// titlebar drags. Idempotent. `element` may be an existing navbar to reuse.
function toolbar(element, options) {
  options = options || {};
  if (!element || !element.setAttribute || element.__pekoToolbar) {
    return element;
  }
  element.__pekoToolbar = true;

  if (platform.mobile && !options.keepOnMobile) {
    element.style.display = 'none';
    return element;
  }
  if (platform.frameless && options.drag !== false) {
    element.setAttribute('data-peko-drag', '');
  }
  // Inset the content clear of the native controls (macOS traffic lights).
  if (platform.titlebarInset) {
    element.style.paddingLeft = platform.titlebarInset + 'px';
  }
  if (options.controls && platform.windowControls) {
    element.appendChild(buildControls());
  }
  return element;
}

// Deliver a locally-generated event (an HTML menu selection) to the same
// listeners a native push event would reach, so peko.on('menu', ...) handles
// native and HTML menus alike.
function dispatchLocal(name, data) {
  const set = listeners.get(name);
  if (set) {
    set.forEach(function (callback) {
      try {
        callback(data);
      } catch (error) {
        // A listener throwing must not stop the others.
      }
    });
  }
}

// Parse an accelerator string like "CmdOrCtrl+Shift+S" into a spec matched
// against a keydown event. CmdOrCtrl matches either Control or Command. Returns
// null when no key was found, so a label-only entry registers no shortcut.
function parseAccelerator(accel) {
  const spec = { ctrl: false, shift: false, alt: false, key: '' };
  String(accel)
    .split('+')
    .forEach(function (raw) {
      const part = raw.trim().toLowerCase();
      if (
        part === 'cmdorctrl' ||
        part === 'cmd' ||
        part === 'command' ||
        part === 'ctrl' ||
        part === 'control' ||
        part === 'super' ||
        part === 'meta'
      ) {
        spec.ctrl = true;
      } else if (part === 'shift') {
        spec.shift = true;
      } else if (part === 'alt' || part === 'option') {
        spec.alt = true;
      } else if (part.length > 0) {
        spec.key = part;
      }
    });
  return spec.key ? spec : null;
}

// Whether a keydown event matches an accelerator spec. CmdOrCtrl is satisfied by
// either the Control or the Command modifier.
function acceleratorMatches(spec, event) {
  if (spec.ctrl !== (event.ctrlKey || event.metaKey)) {
    return false;
  }
  if (spec.shift !== event.shiftKey) {
    return false;
  }
  if (spec.alt !== event.altKey) {
    return false;
  }
  const key = (event.key || '').toLowerCase();
  return key === spec.key;
}

// The accelerators of the current HTML menu bar. A single keydown listener
// consults this, so the shortcuts shown in the menu actually fire their action.
// Building a menu bar replaces the set; the native menu path leaves it empty.
let menuAccelerators = [];
let acceleratorListenerInstalled = false;

function installAcceleratorListener() {
  if (acceleratorListenerInstalled || typeof document === 'undefined') {
    return;
  }
  acceleratorListenerInstalled = true;
  document.addEventListener('keydown', function (event) {
    if (!menuAccelerators.length) {
      return;
    }
    for (let i = 0; i < menuAccelerators.length; i++) {
      const entry = menuAccelerators[i];
      if (acceleratorMatches(entry.spec, event)) {
        event.preventDefault();
        if (entry.action) {
          dispatchLocal('menu', { id: entry.action });
        }
        if (typeof entry.onClick === 'function') {
          entry.onClick();
        }
        return;
      }
    }
  });
}

// Build an HTML menu bar from a definition: an array of top-level menus, each
// { label, items: [ { label, action, accelerator? } | { separator: true } ] }.
// Choosing an item fires a "menu" event carrying its action id, matching the
// native menu, and runs an optional onClick. Item accelerators are registered
// so their keyboard shortcut fires the same action.
function buildMenuBar(definition) {
  const bar = document.createElement('div');
  bar.className = 'peko-menubar';
  bar.setAttribute('data-peko-no-drag', '');

  // Registered anew for this bar, so its keyboard shortcuts fire their action.
  const accelerators = [];

  const closeAll = function () {
    const open = bar.querySelectorAll('.peko-menu-dropdown');
    for (let i = 0; i < open.length; i++) {
      open[i].hidden = true;
    }
  };

  (definition || []).forEach(function (top) {
    const group = document.createElement('div');
    group.className = 'peko-menu';
    const title = document.createElement('button');
    title.type = 'button';
    title.className = 'peko-menu-title';
    title.textContent = top.label || '';
    const dropdown = document.createElement('div');
    dropdown.className = 'peko-menu-dropdown';
    dropdown.hidden = true;

    (top.items || []).forEach(function (item) {
      if (item.separator) {
        const separator = document.createElement('div');
        separator.className = 'peko-menu-separator';
        dropdown.appendChild(separator);
        return;
      }
      const entry = document.createElement('button');
      entry.type = 'button';
      entry.className = 'peko-menu-item';
      const label = document.createElement('span');
      label.textContent = item.label || '';
      entry.appendChild(label);
      if (item.accelerator) {
        const accel = document.createElement('span');
        accel.className = 'peko-menu-accel';
        accel.textContent = item.accelerator;
        entry.appendChild(accel);
        const spec = parseAccelerator(item.accelerator);
        if (spec) {
          accelerators.push({
            spec: spec,
            action: item.action,
            onClick: item.onClick,
          });
        }
      }
      entry.addEventListener('click', function () {
        closeAll();
        if (item.action) {
          dispatchLocal('menu', { id: item.action });
        }
        if (typeof item.onClick === 'function') {
          item.onClick();
        }
      });
      dropdown.appendChild(entry);
    });

    title.addEventListener('click', function (event) {
      event.stopPropagation();
      const wasHidden = dropdown.hidden;
      closeAll();
      dropdown.hidden = !wasHidden;
    });

    group.appendChild(title);
    group.appendChild(dropdown);
    bar.appendChild(group);
  });

  document.addEventListener('click', closeAll);
  menuAccelerators = accelerators;
  installAcceleratorListener();
  return bar;
}

// Render an HTML menu from a definition, but only where there is no native menu
// bar (a frameless desktop window or mobile), unless options.force is set. When
// options.mount (an element or selector) is given the bar is appended to it.
// Returns the menu element, or null when a native menu is used.
function menu(definition, options) {
  options = options || {};
  if (typeof document === 'undefined') {
    return null;
  }
  if (platform.nativeMenu && !options.force) {
    return null;
  }
  const bar = buildMenuBar(definition);
  if (options.mount) {
    const target =
      typeof options.mount === 'string'
        ? document.querySelector(options.mount)
        : options.mount;
    if (target) {
      target.appendChild(bar);
    }
  }
  return bar;
}

// Minimal layout styles for the built-in chrome, injected once. Colors are left
// to the app's theme (the elements inherit them); only structure is set here.
function injectChromeStyles() {
  if (typeof document === 'undefined' || document.getElementById('peko-chrome-styles')) {
    return;
  }
  const style = document.createElement('style');
  style.id = 'peko-chrome-styles';
  style.textContent =
    // The toolbar is a flex row whether it comes from the <peko-toolbar> custom
    // element or a framework <header class="peko-toolbar">, so its content and
    // window controls sit on one line rather than stacking.
    'peko-toolbar,.peko-toolbar{display:flex;align-items:center;gap:8px}' +
    '.peko-toolbar-content{display:flex;align-items:center;gap:12px;flex:1;min-width:0}' +
    '.peko-controls{display:flex;gap:2px;margin-left:auto}' +
    '.peko-control{font:inherit;line-height:1;border:0;background:transparent;' +
    'color:inherit;cursor:pointer;padding:6px 10px;border-radius:6px;' +
    'user-select:none;-webkit-user-select:none}' +
    '.peko-control:hover{background:rgba(127,127,127,0.2)}' +
    '.peko-control-close:hover{background:#e81123;color:#fff}' +
    // The menu chrome is not text-selectable, so a press on a label reads as a
    // button click rather than the start of a text selection.
    '.peko-menubar{display:flex;align-items:center;gap:2px;' +
    'user-select:none;-webkit-user-select:none}' +
    '.peko-menu{position:relative}' +
    // Inner label and accelerator spans do not take pointer events, so a click
    // on the text always targets the enclosing button.
    '.peko-menu-title *,.peko-menu-item *{pointer-events:none}' +
    '.peko-menu-title{font:inherit;border:0;background:transparent;color:inherit;' +
    'cursor:pointer;padding:4px 10px;border-radius:6px}' +
    '.peko-menu-title:hover{background:rgba(127,127,127,0.2)}' +
    '.peko-menu-dropdown{position:absolute;top:100%;left:0;min-width:180px;z-index:1000;' +
    'display:flex;flex-direction:column;padding:4px;border-radius:8px;' +
    'background:Canvas;color:CanvasText;box-shadow:0 6px 24px rgba(0,0,0,0.25);' +
    'border:1px solid rgba(127,127,127,0.3)}' +
    // A dropdown carries the hidden attribute when collapsed; the class rule
    // above sets display, so restore display:none for the hidden state.
    '.peko-menu-dropdown[hidden]{display:none}' +
    '.peko-menu-item{display:flex;justify-content:space-between;gap:24px;font:inherit;' +
    'border:0;background:transparent;color:inherit;cursor:pointer;text-align:left;' +
    'padding:6px 10px;border-radius:6px}' +
    '.peko-menu-item:hover{background:rgba(127,127,127,0.2)}' +
    '.peko-menu-accel{opacity:0.6}' +
    '.peko-menu-separator{height:1px;margin:4px 6px;background:rgba(127,127,127,0.3)}' +
    // On mobile the toolbar extends its background into the top safe area (the
    // notch or camera cutout) and insets its content below it, so buttons are
    // not under an unclickable region while the bar keeps a native look.
    '.peko-mobile .peko-toolbar{padding-top:env(safe-area-inset-top,0px)}';
  (document.head || document.documentElement).appendChild(style);
}

// Register the <peko-toolbar> and <peko-menu> custom elements, and enhance any
// element carrying data-peko-toolbar. This lets a project reuse its own navbar
// (<nav data-peko-toolbar="controls">) or drop in <peko-toolbar controls>.
function definePekoElements() {
  if (typeof customElements === 'undefined') {
    return;
  }
  if (!customElements.get('peko-toolbar')) {
    customElements.define(
      'peko-toolbar',
      class extends HTMLElement {
        connectedCallback() {
          toolbar(this, {
            controls: this.hasAttribute('controls'),
            keepOnMobile: this.hasAttribute('mobile'),
            drag: !this.hasAttribute('no-drag'),
          });
        }
      }
    );
  }
  if (!customElements.get('peko-menu')) {
    // A wrapper for a hand-authored HTML menu: hidden where a native menu bar
    // exists (unless forced), shown on a frameless desktop window or mobile.
    customElements.define(
      'peko-menu',
      class extends HTMLElement {
        connectedCallback() {
          if (platform.nativeMenu && !this.hasAttribute('force')) {
            this.style.display = 'none';
          }
        }
      }
    );
  }
}

function enhanceToolbars() {
  if (typeof document === 'undefined') {
    return;
  }
  const nodes = document.querySelectorAll('[data-peko-toolbar]');
  for (let i = 0; i < nodes.length; i++) {
    const node = nodes[i];
    toolbar(node, {
      controls:
        node.getAttribute('data-peko-toolbar') === 'controls' ||
        node.hasAttribute('data-peko-controls'),
      keepOnMobile: node.hasAttribute('data-peko-mobile'),
    });
  }
}

// Render a console argument as a string for forwarding to the devtools window.
function stringifyConsoleArg(arg) {
  if (typeof arg === 'string') {
    return arg;
  }
  if (arg instanceof Error) {
    return arg.stack || arg.message || String(arg);
  }
  try {
    return JSON.stringify(arg);
  } catch (error) {
    return String(arg);
  }
}

// When the devtools window is attached (peko run --devtools), forward the web
// console and uncaught errors to it over the bridge. The original console still
// prints, so the browser inspector is unaffected. A production build never sets
// the flag, so this is inert.
function installDevtoolsConsole() {
  const injected =
    typeof window !== 'undefined' && window.__PEKO__ ? window.__PEKO__ : null;
  if (!injected || !injected.devtools || typeof console === 'undefined') {
    return;
  }
  const forward = function (level, text) {
    invoke('devtools.log', { level: level, text: text }).catch(function () {});
  };
  ['log', 'info', 'warn', 'error'].forEach(function (level) {
    const original =
      typeof console[level] === 'function' ? console[level].bind(console) : null;
    console[level] = function () {
      const text = Array.prototype.map
        .call(arguments, stringifyConsoleArg)
        .join(' ');
      forward(level, text);
      if (original) {
        original.apply(console, arguments);
      }
    };
  });
  if (typeof window.addEventListener === 'function') {
    window.addEventListener('error', function (event) {
      forward('error', String((event && (event.message || event.error)) || event));
    });
    window.addEventListener('unhandledrejection', function (event) {
      forward('error', 'Unhandled rejection: ' + String(event && event.reason));
    });
  }

  // The devtools window drives the interactive console and view source by asking
  // the page to run something. A "devtools:run" event carries { id, kind, code }:
  // kind "eval" evaluates the code in the page, kind "source" reads the current
  // DOM. The outcome goes back as a devtools.result call, which the native side
  // relays to the window.
  on('devtools:run', function (request) {
    if (!request) {
      return;
    }
    const id = request.id;
    const kind = request.kind;
    let ok = true;
    let result = '';
    try {
      if (kind === 'source') {
        result =
          typeof document !== 'undefined' && document.documentElement
            ? document.documentElement.outerHTML
            : '';
      } else {
        // Indirect eval runs in global scope. Show the value, not [object].
        const value = (0, eval)(request.code);
        result = stringifyConsoleArg(value);
      }
    } catch (error) {
      ok = false;
      result = error && error.stack ? error.stack : String(error);
    }
    invoke('devtools.result', {
      id: id,
      kind: kind,
      ok: ok,
      result: result,
    }).catch(function () {});
  });
}

// Tag the document root with the platform so app CSS can target it, for
// example `.peko-mobile .my-bar { ... }` or `.peko-os-macos { ... }`.
function applyPlatformClasses() {
  if (typeof document === 'undefined' || !document.documentElement) {
    return;
  }
  const root = document.documentElement;
  root.classList.add('peko-os-' + platform.os);
  if (platform.mobile) {
    root.classList.add('peko-mobile');
  }
  if (platform.frameless) {
    root.classList.add('peko-frameless');
  }
}

// Make the viewport cover the whole screen on mobile, so the safe-area insets
// (env(safe-area-inset-*)) report the notch and home-indicator regions. Without
// viewport-fit=cover those insets are zero and the toolbar sits under the notch.
function ensureViewportFit() {
  if (typeof document === 'undefined' || !platform.mobile) {
    return;
  }
  let meta = document.querySelector('meta[name="viewport"]');
  if (!meta) {
    meta = document.createElement('meta');
    meta.setAttribute('name', 'viewport');
    meta.setAttribute(
      'content',
      'width=device-width, initial-scale=1, viewport-fit=cover'
    );
    (document.head || document.documentElement).appendChild(meta);
    return;
  }
  const content = meta.getAttribute('content') || '';
  if (content.indexOf('viewport-fit') === -1) {
    meta.setAttribute('content', content + (content ? ', ' : '') + 'viewport-fit=cover');
  }
}

const core = {
  invoke: invoke,
  on: on,
  off: off,
  ready: ready,
  connect: connect,
  platform: platform,
  titlebar: titlebar,
  toolbar: toolbar,
  menu: menu,
  noDrag: noDrag,
  control: control,
  window: windowControls,
};

// A namespace proxy: peko.storage.get(params) calls invoke("storage.get", params).
function namespace(name) {
  return new Proxy({}, {
    get: function (_target, method) {
      if (typeof method !== 'string') {
        return undefined;
      }
      return function (params) {
        return invoke(name + '.' + method, params);
      };
    },
  });
}

// The root proxy: core members pass through; any other property is a namespace.
export const peko = new Proxy(core, {
  get: function (target, property) {
    if (typeof property !== 'string') {
      return target[property];
    }
    if (property in target) {
      return target[property];
    }
    if (property === 'then') {
      return undefined;
    }
    return namespace(property);
  },
});

// Auto-connect in a browser or webview, and expose the object globally so a
// plain HTML page can use it without importing.
if (typeof window !== 'undefined') {
  window.peko = peko;
  // The native host pushes a deep-link route here when a URL arrives while the
  // page is loaded (used on iOS, whose URLs are delivered after connect).
  window.__peko_deeplink = deliverInitialRoute;
  const injected = window.__PEKO__;
  if (injected && injected.initialRoute) {
    deliverInitialRoute(injected.initialRoute);
  }
  connect();
  startRouteSync();
  installDevtoolsConsole();

  // Register the chrome custom elements and default styles, tag the platform,
  // and make the mobile viewport cover the screen, then enhance any
  // data-peko-toolbar element once the DOM is ready.
  definePekoElements();
  injectChromeStyles();
  applyPlatformClasses();
  ensureViewportFit();
  if (typeof document !== 'undefined') {
    const onReady = function () {
      applyPlatformClasses();
      ensureViewportFit();
      enhanceToolbars();
    };
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', onReady);
    } else {
      onReady();
    }
  }
}

export default peko;
