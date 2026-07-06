// Type declarations for @peko/client.

/** A native handler namespace: peko.storage.get(params), etc. */
export interface PekoNamespace {
  [method: string]: (params?: unknown) => Promise<unknown>;
}

/** Programmatic window controls for a frameless window. */
export interface PekoWindow {
  minimize(): void;
  maximize(): void;
  close(): void;
}

/** What the app is running on, so the UI can adapt. */
export interface PekoPlatform {
  /** 'macos' | 'windows' | 'linux' | 'android' | 'ios' | 'unknown'. */
  readonly os: string;
  /** True on a phone or tablet. */
  readonly mobile: boolean;
  /** True on a desktop OS. */
  readonly desktop: boolean;
  /** True when the window has no native titlebar (custom chrome needed).
   *  Always false on mobile, which has no movable window. */
  readonly frameless: boolean;
  /** True when the OS draws the window controls itself over the frameless
   *  content (macOS traffic lights). */
  readonly nativeControls: boolean;
  /** True when the app should render custom window controls: a frameless
   *  desktop window where the OS draws none. */
  readonly windowControls: boolean;
  /** Left inset in pixels so titlebar content clears the native controls
   *  (the macOS traffic lights); 0 when no inset is needed. */
  readonly titlebarInset: number;
  /** True when a native menu bar exists; otherwise render an HTML menu. */
  readonly nativeMenu: boolean;
}

/** Options for enhancing an element as the app toolbar. */
export interface PekoToolbarOptions {
  /** Append window controls on a frameless desktop window. */
  controls?: boolean;
  /** Make it the drag region on a frameless window. Default true. */
  drag?: boolean;
  /** Keep it visible on mobile (hidden by default). */
  keepOnMobile?: boolean;
}

/** One entry in an HTML menu dropdown. */
export type PekoMenuEntry =
  | { label: string; action?: string; accelerator?: string; onClick?: () => void }
  | { separator: true };

/** A top-level menu in an HTML menu definition. */
export interface PekoMenuTop {
  label: string;
  items: PekoMenuEntry[];
}

/** Options for rendering an HTML menu. */
export interface PekoMenuOptions {
  /** Render even where a native menu bar exists. */
  force?: boolean;
  /** An element or selector to append the menu to. */
  mount?: string | Element;
}

export interface PekoClient {
  /** Resolves when the bridge connection is authenticated and ready. */
  readonly ready: Promise<void>;
  /** Call a native handler by "namespace.method" name. */
  invoke(method: string, params?: unknown): Promise<unknown>;
  /** Subscribe to a native push event. Returns an unsubscribe function. */
  on(event: string, callback: (data: unknown) => void): () => void;
  /** Remove a previously registered event listener. */
  off(event: string, callback: (data: unknown) => void): void;
  /** Open the bridge connection. Called automatically on import. */
  connect(): void;
  /** What the app is running on. */
  readonly platform: PekoPlatform;
  /** Marks an element as the window drag region (data-peko-drag). */
  titlebar<T extends Element>(element: T): T;
  /** Enhance an existing element as the app toolbar (drag + optional controls). */
  toolbar<T extends Element>(element: T, options?: PekoToolbarOptions): T;
  /** Render an HTML menu where there is no native menu bar. */
  menu(definition: PekoMenuTop[], options?: PekoMenuOptions): Element | null;
  /** Opts an element out of the drag region (data-peko-no-drag). */
  noDrag<T extends Element>(element: T): T;
  /** Marks an element as a window control button. */
  control<T extends Element>(element: T, kind: 'minimize' | 'maximize' | 'close'): T;
  /** Programmatic window controls. */
  readonly window: PekoWindow;
  /** Any other property is a handler namespace. */
  [namespace: string]: PekoNamespace | unknown;
}

export const peko: PekoClient;
export default peko;

declare global {
  interface Window {
    __PEKO__?: {
      url: string;
      token: string | null;
      initialRoute?: string;
      frameless?: boolean;
      nativeControls?: boolean;
      htmlMenu?: boolean;
      devtools?: boolean;
    };
    peko?: PekoClient;
    __peko_deeplink?: (path: string) => void;
  }
}
