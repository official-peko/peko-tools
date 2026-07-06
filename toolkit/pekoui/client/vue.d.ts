// Type declarations for @peko/client/vue.

import type { DefineComponent } from 'vue';
import type {
  PekoClient,
  PekoPlatform,
  PekoWindow,
  PekoMenuTop,
} from './index.js';

/** Read the current platform info. */
export function usePlatform(): PekoPlatform;

/** The programmatic window controls (minimize/maximize/close). */
export function useWindowControls(): PekoWindow;

/** Subscribe to a native push event for the component's lifetime. */
export function usePekoEvent(name: string, handler: (data: unknown) => void): void;

/** Wire native navigate events to a navigate callback (e.g. a router). */
export function useNavigate(onNavigate: (path: string) => void): void;

/** A platform-aware app toolbar / titlebar. */
export const Toolbar: DefineComponent<{
  controls?: boolean;
  drag?: boolean;
  hideOnMobile?: boolean;
}>;

/** Kept for parity with the React adapter. */
export const Titlebar: typeof Toolbar;

/** An HTML menu, rendered only where there is no native menu bar. */
export const Menu: DefineComponent<{
  items?: PekoMenuTop[];
  force?: boolean;
}>;

export const peko: PekoClient;
export default peko;
