// @peko/client/react - React adapters for the Peko native bridge.
//
// Provides platform-aware window chrome: a Toolbar that adapts to the platform
// (drag + window controls only on a frameless desktop window, plain header on
// mobile and decorated windows), an HTML Menu that renders only where there is
// no native menu bar, hooks to read the platform and subscribe to native push
// events, and helpers to reuse an existing element as the drag region.
//
// Written with React.createElement rather than JSX so it ships as plain
// JavaScript and needs no build step in a consuming project.

import { createElement, useEffect, useRef } from 'react';
import { peko } from './index.js';

// The current platform info: { os, mobile, desktop, frameless, windowControls,
// nativeMenu }. It is fixed for the app's lifetime, so no re-render is needed.
export function usePlatform() {
  return peko.platform;
}

// The programmatic window controls (peko.window.minimize/maximize/close).
export function useWindowControls() {
  return peko.window;
}

// Props that mark an element as the window drag region. Spread onto any element:
//   <header {...useDraggable()}>...</header>
// Only meaningful on a frameless desktop window.
export function useDraggable() {
  return peko.platform.frameless ? { 'data-peko-drag': '' } : {};
}

// Subscribe to a native push event (navigate, menu, or an app event) for the
// lifetime of the component. The latest handler is always used without
// resubscribing, so an inline handler is fine.
export function usePekoEvent(name, handler) {
  const stored = useRef(handler);
  stored.current = handler;
  useEffect(function () {
    const unsubscribe = peko.on(name, function (data) {
      stored.current(data);
    });
    return unsubscribe;
  }, [name]);
}

// Wire native navigate events (from app.navigate, menus, or deep links) to a
// navigate callback, for example a router's navigate function.
export function useNavigate(onNavigate) {
  usePekoEvent('navigate', function (data) {
    if (data && typeof data.path === 'string') {
      onNavigate(data.path);
    }
  });
}

// A control button carrying the data-peko-<kind> attribute the webview handles.
function controlButton(kind, label, glyph) {
  return createElement(
    'button',
    {
      key: kind,
      type: 'button',
      'aria-label': label,
      className: 'peko-control peko-control-' + kind,
      ['data-peko-' + kind]: '',
    },
    glyph
  );
}

// A platform-aware app toolbar. Its content (children) always renders, so it
// doubles as a header. On a frameless desktop window it becomes the drag region
// and, unless controls is false, shows the window controls; on a decorated
// window and on mobile it stays a plain bar (native chrome handles the rest).
// Pass hideOnMobile to drop it entirely on phones.
//
//   <Toolbar>My App</Toolbar>
//   <Toolbar hideOnMobile>{tabs}</Toolbar>
export function Toolbar(props) {
  const options = props || {};
  const platform = peko.platform;
  if (options.hideOnMobile && platform.mobile) {
    return null;
  }

  const showControls = options.controls !== false && platform.windowControls;
  const draggable = platform.frameless && options.drag !== false;

  // Inset the content clear of the native controls (macOS traffic lights).
  const style = platform.titlebarInset
    ? Object.assign({ paddingLeft: platform.titlebarInset + 'px' }, options.style)
    : options.style;
  const attributes = {
    className: options.className
      ? 'peko-toolbar ' + options.className
      : 'peko-toolbar',
    style: style,
  };
  if (draggable) {
    attributes['data-peko-drag'] = '';
  }

  const content = [
    createElement(
      'div',
      { key: 'content', className: 'peko-toolbar-content' },
      options.children
    ),
  ];
  if (showControls) {
    content.push(
      createElement(
        'div',
        {
          key: 'controls',
          className: 'peko-controls',
          'data-peko-no-drag': '',
        },
        controlButton('minimize', 'Minimize', '−'),
        controlButton('maximize', 'Maximize', '□'),
        controlButton('close', 'Close', '×')
      )
    );
  }

  return createElement('header', attributes, content);
}

// Kept for compatibility: a Toolbar by its previous name.
export const Titlebar = Toolbar;

// An HTML menu rendered only where there is no native menu bar (a frameless
// desktop window or mobile), unless force is set. `items` is the menu
// definition (array of { label, items: [...] }); a stable reference is
// expected. Choosing an entry fires a "menu" event with its action id, so the
// same peko.on('menu') / usePekoEvent('menu') handler covers native and HTML
// menus alike.
//
//   <Menu items={MENU} />
export function Menu(props) {
  const options = props || {};
  const mount = useRef(null);
  useEffect(function () {
    const host = mount.current;
    if (!host) {
      return undefined;
    }
    const bar = peko.menu(options.items, { force: options.force });
    if (!bar) {
      return undefined;
    }
    host.appendChild(bar);
    return function () {
      if (bar.parentNode) {
        bar.parentNode.removeChild(bar);
      }
    };
  }, [options.items, options.force]);

  return createElement('div', { ref: mount, className: 'peko-menu-mount' });
}

export { peko };
export default peko;
