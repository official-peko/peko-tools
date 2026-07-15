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

// The bridge token most recently in hand. Starts as the one the native host
// injected (or none for a plain same-origin page) and is replaced by refreshes
// (see scheduleTokenRefresh), so a reconnect after the ~15-minute expiry uses a
// fresh token rather than a dead one.
let currentToken = null;
let tokenInitialized = false;
let refreshTimer = null;
let firstReady = true;

// Resolve the bridge endpoint: the injected config in a native webview, or a
// same-origin socket for a server-rendered page in a plain browser. The URL is
// the base (no token); connect() appends the current token as a query param,
// which the hosted /__peko__ verifier reads (the local loopback bridge reads the
// auth frame instead and ignores the query).
function endpoint() {
  // injectedConfig also returns the opener's config for a pop-up iframe, so the
  // pop-up connects to the same bridge as the window that opened it.
  const injected = injectedConfig();
  if (injected && injected.url) {
    // Strip any token baked into the injected URL; connect() re-appends the
    // current (possibly refreshed) one.
    const base = injected.url.split('?')[0];
    return { url: base, token: injected.token || null };
  }
  if (typeof location !== 'undefined' && location.host) {
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    return { url: scheme + '//' + location.host + '/__peko__', token: null };
  }
  return null;
}

// Compose the connect URL, carrying the token in the query for the hosted edge
// verifier. A tokenless page (same-origin browser) connects to the bare URL.
function connectUrl(base, token) {
  if (!token) {
    return base;
  }
  return base + (base.indexOf('?') === -1 ? '?' : '&') + 'token=' + encodeURIComponent(token);
}

function handleMessage(event) {
  let message;
  try {
    message = JSON.parse(event.data);
  } catch (error) {
    return;
  }

  if (message.t === 'ready') {
    resolveReady();
    if (firstReady) {
      firstReady = false;
      // Fetch a launch route the platform delivers after connect (iOS). On the
      // platforms that injected it into the boot config, take_initial is already
      // consumed, so this resolves empty and delivers nothing.
      invoke('deeplink.initial').then(deliverInitialRoute).catch(function () {});
      scheduleTokenRefresh();
    }
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
    // A protocol/auth error on the very first connect is fatal (nothing to fall
    // back to); on a later reconnect the close handler just retries.
    if (firstReady) {
      rejectReady(new Error(message.error || 'bridge error'));
    }
  }
}

let reconnectDelay = 500;

function connect() {
  const target = endpoint();
  if (!target) {
    rejectReady(new Error('no Peko bridge endpoint'));
    return;
  }
  if (!tokenInitialized) {
    currentToken = target.token;
    tokenInitialized = true;
  }

  socket = new WebSocket(connectUrl(target.url, currentToken));

  socket.addEventListener('open', function () {
    reconnectDelay = 500;
    socket.send(JSON.stringify({ t: 'auth', token: currentToken }));
  });

  socket.addEventListener('message', handleMessage);

  socket.addEventListener('close', function () {
    socket = null;
    // Reconnect with capped backoff so a network blip, a provider restart, or a
    // token rotation recovers on its own. A plain page with no bridge keeps
    // retrying cheaply; the app never sees the churn.
    setTimeout(connect, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 10000);
  });

  socket.addEventListener('error', function () {
    // 'close' fires next and owns the reconnect.
  });
}

// Ask the native provider for a fresh bridge token before the current one
// expires (~15 min), so the next reconnect authenticates. The provider mints it
// via the CLI; a plain page (no provider) just gets method_not_found, ignored.
function scheduleTokenRefresh() {
  if (refreshTimer || typeof setInterval === 'undefined') {
    return;
  }
  refreshTimer = setInterval(function () {
    invoke('bridge.token')
      .then(function (result) {
        if (result && typeof result.token === 'string' && result.token) {
          currentToken = result.token;
        }
      })
      .catch(function () {
        // No provider / not hosted: nothing to refresh.
      });
  }, 12 * 60 * 1000);
}

// Call a native handler by "namespace.method" name. Resolves with the handler
// result, rejects with an Error carrying the native error code.
function invoke(method, params) {
  return ready.then(function () {
    const id = nextId++;
    return new Promise(function (resolve, reject) {
      // The socket may be briefly down mid-reconnect; fail fast so the caller can
      // retry rather than throwing on a null/closing socket.
      if (!socket || socket.readyState !== WebSocket.OPEN) {
        reject(new Error('bridge not connected'));
        return;
      }
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
// The native host injects window.__PEKO__ into the main frame. A pop-up window
// is a same-origin iframe of the app at another route; it has no injection of
// its own, so it inherits the opener's config (bridge endpoint, platform) from
// the parent frame.
function injectedConfig() {
  if (typeof window === 'undefined') {
    return {};
  }
  if (window.__PEKO__) {
    return window.__PEKO__;
  }
  if (window.parent && window.parent !== window) {
    try {
      if (window.parent.__PEKO__) {
        return window.parent.__PEKO__;
      }
    } catch (error) {
      // Cross-origin parent: no access. Fall through.
    }
  }
  return {};
}

const platform = (function () {
  const injected = injectedConfig();
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

// Render a console argument as a readable string for the devtools console.
// A top-level string prints as-is; everything else is formatted by
// formatConsoleValue, which handles DOM nodes (JSON.stringify renders those as
// {}), functions, errors, Map/Set, circular references, and nests with
// indentation so the panel can pretty-print and highlight it.
function stringifyConsoleArg(arg) {
  if (typeof arg === 'string') {
    return arg;
  }
  try {
    return formatConsoleValue(arg, '', new (typeof WeakSet !== 'undefined' ? WeakSet : Array)());
  } catch (error) {
    return String(arg);
  }
}

// A short one-line summary of a DOM element: its opening tag with id and class.
function describeElement(el) {
  var tag = (el.tagName || 'node').toLowerCase();
  var id = el.id ? '#' + el.id : '';
  var cls = el.className && typeof el.className === 'string' ? '.' + el.className.trim().replace(/\s+/g, '.') : '';
  return '<' + tag + id + cls + '>';
}

// Format one value for the console. `indent` is the current line prefix; `seen`
// guards against circular references.
function formatConsoleValue(value, indent, seen) {
  var type = typeof value;
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  if (type === 'string') return JSON.stringify(value);
  if (type === 'number' || type === 'boolean' || type === 'bigint') return String(value);
  if (type === 'symbol') return value.toString();
  if (type === 'function') {
    var kind = String(value).indexOf('class') === 0 ? 'class' : 'Function';
    return '[' + kind + ': ' + (value.name || 'anonymous') + ']';
  }
  if (value instanceof Error) return value.stack || value.name + ': ' + value.message;

  // DOM nodes: JSON.stringify collapses these to {}, so render them usefully.
  if (typeof Node !== 'undefined' && value instanceof Node) {
    if (value.nodeType === 1) {
      var html = value.outerHTML || describeElement(value);
      return html.length > 5000 ? html.slice(0, 5000) + '\n... (truncated)' : html;
    }
    if (value.nodeType === 3) return '#text ' + JSON.stringify(value.textContent);
    if (value.nodeType === 9) return '#document';
    return '#' + (value.nodeName || 'node').toLowerCase();
  }

  if (seen.has && seen.has(value)) return '[Circular]';
  if (seen.add) seen.add(value);

  var next = indent + '  ';
  var out;
  if (Array.isArray(value)) {
    if (value.length === 0) {
      out = '[]';
    } else {
      var items = value.map(function (item) {
        return next + formatConsoleValue(item, next, seen);
      });
      out = '[\n' + items.join(',\n') + '\n' + indent + ']';
    }
  } else if (typeof Map !== 'undefined' && value instanceof Map) {
    var mapEntries = [];
    value.forEach(function (v, k) {
      mapEntries.push(next + formatConsoleValue(k, next, seen) + ' => ' + formatConsoleValue(v, next, seen));
    });
    out = 'Map(' + value.size + ') {' + (mapEntries.length ? '\n' + mapEntries.join(',\n') + '\n' + indent : '') + '}';
  } else if (typeof Set !== 'undefined' && value instanceof Set) {
    var setEntries = [];
    value.forEach(function (v) {
      setEntries.push(next + formatConsoleValue(v, next, seen));
    });
    out = 'Set(' + value.size + ') {' + (setEntries.length ? '\n' + setEntries.join(',\n') + '\n' + indent : '') + '}';
  } else {
    var keys = Object.keys(value);
    var ctor = value.constructor && value.constructor.name;
    var prefix = ctor && ctor !== 'Object' ? ctor + ' ' : '';
    if (keys.length === 0) {
      out = prefix + '{}';
    } else {
      var parts = keys.map(function (key) {
        return next + JSON.stringify(key) + ': ' + formatConsoleValue(value[key], next, seen);
      });
      out = prefix + '{\n' + parts.join(',\n') + '\n' + indent + '}';
    }
  }

  if (seen.delete) seen.delete(value);
  return out;
}

// The property names available on a base expression, including inherited ones,
// for console completion. An empty base returns the global scope's names. The
// list is sorted and de-duplicated.
function completionNames(base) {
  var obj;
  if (!base) {
    obj =
      typeof window !== 'undefined'
        ? window
        : typeof globalThis !== 'undefined'
          ? globalThis
          : {};
  } else {
    obj = (0, eval)('(' + base + ')');
  }
  if (obj === null || obj === undefined) {
    return [];
  }
  var current = typeof obj === 'object' || typeof obj === 'function' ? obj : Object(obj);
  var seen = Object.create(null);
  var out = [];
  var depth = 0;
  while (current !== null && current !== undefined && depth < 30) {
    var names = Object.getOwnPropertyNames(current);
    for (var i = 0; i < names.length; i++) {
      var name = names[i];
      if (!seen[name]) {
        seen[name] = true;
        out.push(name);
      }
    }
    current = Object.getPrototypeOf(current);
    depth += 1;
  }
  out.sort();
  return out;
}

// Classify a resource URL into a source-panel group.
function resourceType(url, initiator) {
  var clean = url.split('?')[0].toLowerCase();
  if (initiator === 'css' || clean.endsWith('.css') || clean.endsWith('.scss')) return 'style';
  if (initiator === 'script' || clean.endsWith('.js') || clean.endsWith('.mjs') || clean.endsWith('.jsx') || clean.endsWith('.ts') || clean.endsWith('.tsx')) return 'script';
  if (initiator === 'img' || /\.(png|jpe?g|gif|webp|avif|bmp|ico|svg)$/.test(clean)) return 'image';
  if (clean.endsWith('.json')) return 'json';
  if (/\.(woff2?|ttf|otf|eot)$/.test(clean)) return 'font';
  return 'other';
}

// The resources the page has loaded (scripts, styles, images, ...), from the
// Performance API, plus the document itself.
function pageResources() {
  var out = [];
  var seen = Object.create(null);
  if (typeof document !== 'undefined' && location) {
    out.push({ url: location.href, type: 'document' });
    seen[location.href] = true;
  }
  try {
    var entries = performance.getEntriesByType('resource');
    for (var i = 0; i < entries.length && out.length < 400; i++) {
      var url = entries[i].name;
      if (seen[url]) continue;
      seen[url] = true;
      out.push({ url: url, type: resourceType(url, entries[i].initiatorType) });
    }
  } catch (e) {
    // Performance API unavailable; the document entry still stands.
  }
  return out;
}

// A snapshot of the running page for the devtools page inspector: route, url,
// title, load state, viewport metrics, current document source, and resources.
function pageSnapshot() {
  var loc = typeof location !== 'undefined' ? location : {};
  var doc = typeof document !== 'undefined' ? document : {};
  var html = doc.documentElement ? doc.documentElement.outerHTML : '';
  return {
    route: (loc.pathname || '') + (loc.search || '') + (loc.hash || ''),
    url: loc.href || '',
    origin: loc.origin || '',
    title: doc.title || '',
    referrer: doc.referrer || '',
    readyState: doc.readyState || '',
    width: typeof window !== 'undefined' ? window.innerWidth : 0,
    height: typeof window !== 'undefined' ? window.innerHeight : 0,
    scrollX: typeof window !== 'undefined' ? Math.round(window.scrollX) : 0,
    scrollY: typeof window !== 'undefined' ? Math.round(window.scrollY) : 0,
    elements: doc.getElementsByTagName ? doc.getElementsByTagName('*').length : 0,
    html: html.length > 200000 ? html.slice(0, 200000) + '\n<!-- truncated -->' : html,
    resources: pageResources(),
  };
}

// Fetch one same-origin (or CORS-permitting) text resource for the source
// viewer, returning {url, mime, text}. The page must fetch it because the IDE
// webview is a different origin and cannot read the response body.
function fetchResource(url) {
  return fetch(url)
    .then(function (response) {
      var mime = response.headers.get('content-type') || '';
      return response.text().then(function (text) {
        return {
          url: url,
          mime: mime,
          text: text.length > 400000 ? text.slice(0, 400000) + '\n/* truncated */' : text,
        };
      });
    })
    .catch(function (error) {
      return { url: url, mime: '', text: '', error: String(error) };
    });
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
    // Resource fetch is async: fetch the body, then reply, and skip the sync
    // reply below.
    if (kind === 'resource') {
      fetchResource(request.code)
        .then(function (payload) {
          invoke('devtools.result', { id: id, kind: 'resource', ok: !payload.error, result: JSON.stringify(payload) }).catch(function () {});
        })
        .catch(function (error) {
          invoke('devtools.result', { id: id, kind: 'resource', ok: false, result: JSON.stringify({ url: request.code, error: String(error) }) }).catch(function () {});
        });
      return;
    }
    try {
      if (kind === 'source') {
        result =
          typeof document !== 'undefined' && document.documentElement
            ? document.documentElement.outerHTML
            : '';
      } else if (kind === 'complete') {
        // List the property names on the base expression, walking the prototype
        // chain, so a console can complete `expr.` An empty base lists globals.
        result = JSON.stringify({
          base: request.code || '',
          names: completionNames(request.code),
        });
      } else if (kind === 'page') {
        result = JSON.stringify(pageSnapshot());
      } else {
        // Indirect eval runs in global scope. Show the value, not [object].
        const value = (0, eval)(request.code);
        result = stringifyConsoleArg(value);
      }
    } catch (error) {
      ok = false;
      // Report the error's name and message. WebKit's error.stack omits the
      // message and lists only the SDK's own frames, which reads as noise in
      // the console.
      result = String(error);
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

// ---------------------------------------------------------------------------
// Pop-up windows
//
// peko.windows.open(route, options) shows another route of the same app as a
// pop-up. In a native desktop app it is a real OS window: a child process of
// the same app pointed at the route and sharing this window's bridge, opened
// through the windows.open native handler. In a browser or on mobile it is an
// in-page surface: a draggable dialog on a desktop browser, a full-height sheet
// on mobile, hosting the route in a same-origin iframe that inherits this
// window's bridge. peko.windows.close(id) closes a pop-up; from inside a pop-up,
// peko.windows.close() dismisses itself.
// ---------------------------------------------------------------------------

let popupSeq = 0;
const openPopups = new Map();
let popupStylesInjected = false;

// onClose callbacks for native pop-up windows, keyed by the id the windows.open
// handler returned. The opener fires them when it receives peko:window-closed.
const nativeOnClose = new Map();
let nativeCloseWired = false;

// True when a native windows.open handler is present: a native desktop app that
// is not itself a pop-up child. A browser (no injected bridge) and mobile use
// the in-page surface instead.
function nativeWindowsAvailable() {
  const injected = injectedConfig();
  return !!(injected && injected.url && !injected.popup && platform.desktop);
}

// Subscribe once to the opener-side close event, so a pop-up that closes (by
// request, by self-report, or by its window closing) fires the stored onClose.
function wireNativeClose() {
  if (nativeCloseWired) {
    return;
  }
  nativeCloseWired = true;
  on('peko:window-closed', function (data) {
    if (!data || data.id == null) {
      return;
    }
    const callback = nativeOnClose.get(data.id);
    if (callback) {
      nativeOnClose.delete(data.id);
      try {
        callback(data.result);
      } catch (error) {
        // A close handler throwing must not break dispatch.
      }
    }
  });
}

// Open a route as a real OS window through the native handler. The id is known
// only once the async call resolves, so the returned handle defers close and
// onClose registration until then.
function openNativeWindow(route, options) {
  wireNativeClose();
  let realId = null;
  let closedEarly = false;
  const opened = invoke('windows.open', {
    route: route,
    title: options.title || '',
    width: options.width || 640,
    height: options.height || 480,
    frameless: options.frameless,
    transparent: options.transparent,
  }).then(function (result) {
    realId = result && result.id;
    if (!realId) {
      return;
    }
    if (closedEarly) {
      invoke('windows.close', { id: realId }).catch(function () {});
      return;
    }
    if (typeof options.onClose === 'function') {
      nativeOnClose.set(realId, options.onClose);
    }
  }).catch(function () {});
  return {
    id: null,
    close: function (result) {
      closedEarly = true;
      opened.then(function () {
        if (realId) {
          invoke('windows.close', { id: realId, result: result }).catch(function () {});
        }
      });
    },
  };
}

function injectPopupStyles() {
  if (popupStylesInjected || typeof document === 'undefined') {
    return;
  }
  popupStylesInjected = true;
  const style = document.createElement('style');
  style.textContent = [
    '.peko-modal-backdrop{position:fixed;inset:0;background:rgba(0,0,0,.4);z-index:2147483000;display:flex;align-items:center;justify-content:center;}',
    '.peko-modal{background:#fff;color:#111;border-radius:10px;box-shadow:0 12px 48px rgba(0,0,0,.35);display:flex;flex-direction:column;overflow:hidden;max-width:96vw;max-height:92vh;position:relative;}',
    '.peko-modal-header{display:flex;align-items:center;gap:8px;padding:8px 12px;cursor:move;user-select:none;border-bottom:1px solid rgba(0,0,0,.1);font:600 13px system-ui,-apple-system,sans-serif;}',
    '.peko-modal-title{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}',
    '.peko-modal-close{border:none;background:transparent;font-size:18px;line-height:1;cursor:pointer;color:#666;padding:2px 8px;border-radius:6px;}',
    '.peko-modal-close:hover{background:rgba(0,0,0,.08);}',
    '.peko-modal-body{flex:1;min-height:0;}',
    '.peko-modal-body iframe{width:100%;height:100%;border:none;display:block;}',
    '.peko-mobile .peko-modal{width:100vw;height:92vh;max-width:100vw;border-radius:16px 16px 0 0;align-self:flex-end;}',
    '.peko-mobile .peko-modal-header{cursor:default;}',
  ].join('');
  (document.head || document.documentElement).appendChild(style);
}

function makePopupDraggable(modal, handle) {
  let dragging = false;
  let startX = 0;
  let startY = 0;
  let originX = 0;
  let originY = 0;
  handle.addEventListener('mousedown', function (event) {
    if (event.target && String(event.target.className).indexOf('peko-modal-close') !== -1) {
      return;
    }
    dragging = true;
    startX = event.clientX;
    startY = event.clientY;
    const match = /translate\(([-\d.]+)px,\s*([-\d.]+)px\)/.exec(modal.style.transform || '');
    originX = match ? parseFloat(match[1]) : 0;
    originY = match ? parseFloat(match[2]) : 0;
    event.preventDefault();
  });
  document.addEventListener('mousemove', function (event) {
    if (!dragging) {
      return;
    }
    modal.style.transform =
      'translate(' + (originX + event.clientX - startX) + 'px,' + (originY + event.clientY - startY) + 'px)';
  });
  document.addEventListener('mouseup', function () {
    dragging = false;
  });
}

function openWindow(route, options) {
  options = options || {};
  const path = String(route == null ? '/' : route);
  // A native desktop app opens a real OS window; a browser or mobile falls
  // through to the in-page surface below.
  if (nativeWindowsAvailable()) {
    return openNativeWindow(path, options);
  }
  if (typeof document === 'undefined' || !document.body) {
    return { id: null, close: function () {} };
  }
  injectPopupStyles();
  const id = 'pw' + ++popupSeq;
  const origin = typeof location !== 'undefined' ? location.origin : '';
  const url = origin + (path.charAt(0) === '/' ? path : '/' + path);

  const backdrop = document.createElement('div');
  backdrop.className = 'peko-modal-backdrop';
  backdrop.setAttribute('data-peko-window', id);

  const modal = document.createElement('div');
  modal.className = 'peko-modal';
  if (!platform.mobile) {
    const width = options.width || 640;
    const height = options.height || 480;
    modal.style.width = typeof width === 'number' ? width + 'px' : width;
    modal.style.height = typeof height === 'number' ? height + 'px' : height;
  }

  const header = document.createElement('div');
  header.className = 'peko-modal-header';
  const title = document.createElement('span');
  title.className = 'peko-modal-title';
  title.textContent = options.title || '';
  const close = document.createElement('button');
  close.className = 'peko-modal-close';
  close.textContent = '×';
  close.addEventListener('click', function () {
    closeWindow(id);
  });
  header.appendChild(title);
  header.appendChild(close);

  const body = document.createElement('div');
  body.className = 'peko-modal-body';
  const iframe = document.createElement('iframe');
  iframe.src = url;
  iframe.setAttribute('data-peko-window', id);
  body.appendChild(iframe);

  modal.appendChild(header);
  modal.appendChild(body);
  backdrop.appendChild(modal);
  document.body.appendChild(backdrop);

  // A click on the dimmed backdrop closes the pop-up unless it is modal.
  if (!options.modal) {
    backdrop.addEventListener('mousedown', function (event) {
      if (event.target === backdrop) {
        closeWindow(id);
      }
    });
  }
  if (!platform.mobile) {
    makePopupDraggable(modal, header);
  }

  openPopups.set(id, { backdrop: backdrop, iframe: iframe, onClose: options.onClose });
  return {
    id: id,
    close: function (result) {
      closeWindow(id, result);
    },
  };
}

function closeWindow(id, result) {
  const injected = injectedConfig();

  // No id from inside a native pop-up window: report the result to the opener
  // over the bridge, then close this OS window.
  if (!id && injected && injected.popup && injected.popupId) {
    invoke('windows.notify_closed', { id: injected.popupId, result: result }).catch(function () {});
    if (typeof window !== 'undefined' && typeof window.__peko_close === 'function') {
      window.__peko_close();
    }
    return;
  }

  // No id from inside an iframe pop-up: ask the opener (the parent frame).
  if (!id && typeof window !== 'undefined' && window.parent && window.parent !== window) {
    try {
      window.parent.postMessage({ __peko_window: 'close', result: result }, '*');
    } catch (error) {
      // Cross-origin parent: cannot post. Nothing to do.
    }
    return;
  }

  // An id that is not an in-page pop-up is a native window: close it by id.
  if (id && !openPopups.has(id)) {
    invoke('windows.close', { id: id, result: result }).catch(function () {});
    return;
  }

  const popup = openPopups.get(id);
  if (!popup) {
    return;
  }
  openPopups.delete(id);
  if (popup.backdrop && popup.backdrop.parentNode) {
    popup.backdrop.parentNode.removeChild(popup.backdrop);
  }
  if (typeof popup.onClose === 'function') {
    try {
      popup.onClose(result);
    } catch (error) {
      // A close handler throwing must not break teardown.
    }
  }
}

// A pop-up dismisses itself by posting to its opener; match the message to the
// pop-up whose iframe sent it.
if (typeof window !== 'undefined' && typeof window.addEventListener === 'function') {
  window.addEventListener('message', function (event) {
    const data = event.data;
    if (!data || data.__peko_window !== 'close') {
      return;
    }
    openPopups.forEach(function (popup, id) {
      if (popup.iframe && popup.iframe.contentWindow === event.source) {
        closeWindow(id, data.result);
      }
    });
  });

  // A native pop-up window that closes by the OS titlebar reports it, so the
  // opener's onClose runs with no explicit result.
  const bootConfig = injectedConfig();
  if (bootConfig && bootConfig.popup && bootConfig.popupId) {
    window.addEventListener('pagehide', function () {
      invoke('windows.notify_closed', { id: bootConfig.popupId, result: null }).catch(function () {});
    });
  }
}

const windowManager = { open: openWindow, close: closeWindow };

const core = {
  invoke: invoke,
  on: on,
  off: off,
  ready: ready,
  connect: connect,
  platform: platform,
  // Coarse bridge health the native host injected at boot: 'local' (loopback
  // bridge), 'ok', 'no-session' (the build was not logged in), 'mint-failed'
  // (token mint failed — check the [peko-bridge] logs), or null in a plain
  // browser. Readable even when the bridge itself cannot connect.
  bridgeStatus: (injectedConfig() && injectedConfig().bridgeStatus) || null,
  titlebar: titlebar,
  toolbar: toolbar,
  menu: menu,
  noDrag: noDrag,
  control: control,
  window: windowControls,
  windows: windowManager,
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
