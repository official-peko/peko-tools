// @peko/client/vue - Vue adapters for the Peko native bridge.
//
// The Vue counterpart of @peko/client/react: a platform-aware Toolbar, an HTML
// Menu that renders only where there is no native menu bar, and composables to
// read the platform and subscribe to native push events. Written with Vue's h
// render function so it ships as plain JavaScript and needs no build step.

import { h, ref, onMounted, onUnmounted } from 'vue';
import { peko } from './index.js';

// The current platform info: { os, mobile, desktop, frameless, windowControls,
// nativeMenu }. Fixed for the app's lifetime.
export function usePlatform() {
  return peko.platform;
}

// The programmatic window controls (peko.window.minimize/maximize/close).
export function useWindowControls() {
  return peko.window;
}

// Subscribe to a native push event for the lifetime of the component.
export function usePekoEvent(name, handler) {
  let unsubscribe = null;
  onMounted(function () {
    unsubscribe = peko.on(name, handler);
  });
  onUnmounted(function () {
    if (unsubscribe) {
      unsubscribe();
    }
  });
}

// Wire native navigate events to a navigate callback, for example a router's.
export function useNavigate(onNavigate) {
  usePekoEvent('navigate', function (data) {
    if (data && typeof data.path === 'string') {
      onNavigate(data.path);
    }
  });
}

function controlButton(kind, label, glyph) {
  return h(
    'button',
    {
      type: 'button',
      'aria-label': label,
      class: 'peko-control peko-control-' + kind,
      ['data-peko-' + kind]: '',
    },
    glyph
  );
}

// A platform-aware app toolbar. Its slot content always renders. On a frameless
// desktop window it becomes the drag region and, unless :controls="false",
// shows the window controls; on a decorated window and on mobile it stays a
// plain bar. Pass :hide-on-mobile to drop it entirely on phones.
//
//   <Toolbar>My App</Toolbar>
export const Toolbar = {
  name: 'PekoToolbar',
  props: {
    controls: { type: Boolean, default: true },
    drag: { type: Boolean, default: true },
    hideOnMobile: { type: Boolean, default: false },
  },
  setup(props, context) {
    return function () {
      const platform = peko.platform;
      if (props.hideOnMobile && platform.mobile) {
        return null;
      }

      const showControls = props.controls && platform.windowControls;
      const draggable = platform.frameless && props.drag;

      const data = { class: 'peko-toolbar' };
      if (draggable) {
        data['data-peko-drag'] = '';
      }
      // Inset the content clear of the native controls (macOS traffic lights).
      if (platform.titlebarInset) {
        data.style = { paddingLeft: platform.titlebarInset + 'px' };
      }

      const slot = context.slots.default ? context.slots.default() : [];
      const children = [h('div', { class: 'peko-toolbar-content' }, slot)];
      if (showControls) {
        children.push(
          h('div', { class: 'peko-controls', 'data-peko-no-drag': '' }, [
            controlButton('minimize', 'Minimize', '−'),
            controlButton('maximize', 'Maximize', '□'),
            controlButton('close', 'Close', '×'),
          ])
        );
      }
      return h('header', data, children);
    };
  },
};

// Kept for parity with the React adapter.
export const Titlebar = Toolbar;

// An HTML menu rendered only where there is no native menu bar (unless :force).
// `items` is the menu definition (array of { label, items: [...] }). Choosing an
// entry fires a "menu" event with its action id, matching the native menu.
//
//   <Menu :items="MENU" />
export const Menu = {
  name: 'PekoMenu',
  props: {
    items: {
      type: Array,
      default: function () {
        return [];
      },
    },
    force: { type: Boolean, default: false },
  },
  setup(props) {
    const mount = ref(null);
    let bar = null;
    onMounted(function () {
      bar = peko.menu(props.items, { force: props.force });
      if (bar && mount.value) {
        mount.value.appendChild(bar);
      }
    });
    onUnmounted(function () {
      if (bar && bar.parentNode) {
        bar.parentNode.removeChild(bar);
      }
    });
    return function () {
      return h('div', { ref: mount, class: 'peko-menu-mount' });
    };
  },
};

export { peko };
export default peko;
