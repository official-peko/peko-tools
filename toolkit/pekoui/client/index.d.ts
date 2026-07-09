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

/** Options for opening a pop-up window. */
export interface PekoWindowOptions {
  /** Header title text. */
  title?: string;
  /** Desktop width in pixels or a CSS size. Default 640. Ignored on mobile. */
  width?: number | string;
  /** Desktop height in pixels or a CSS size. Default 480. Ignored on mobile. */
  height?: number | string;
  /** When true, a backdrop click does not dismiss the pop-up. */
  modal?: boolean;
  /** Native desktop window only: draw no native titlebar, so the pop-up route
   *  renders its own chrome. Omit to inherit the app's default. */
  frameless?: boolean;
  /** Native desktop window only: make the window and web view transparent, so
   *  CSS colors composite over what is behind it. Pair with a transparent HTML
   *  root. Omit to inherit the app's default. */
  transparent?: boolean;
  /** Called when the pop-up closes, with the result passed to close(). */
  onClose?: (result?: unknown) => void;
}

/** A handle to an open pop-up window. */
export interface PekoWindowHandle {
  /** The pop-up id, or null when no document was available to host it. */
  readonly id: string | null;
  /** Close the pop-up, optionally passing a result to its onClose. */
  close(result?: unknown): void;
}

/** Opens app routes as pop-up windows: draggable dialogs on desktop, full
 *  sheets on mobile. Each pop-up hosts the app at a route in a same-origin
 *  iframe that shares the opener's bridge. */
export interface PekoWindowManager {
  /** Open a route as a pop-up window. */
  open(route: string, options?: PekoWindowOptions): PekoWindowHandle;
  /** Close a pop-up by id. Called with no id (null or omitted) from inside a
   *  pop-up, it dismisses that pop-up itself. */
  close(id?: string | null, result?: unknown): void;
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
  /** Open and close pop-up windows. */
  readonly windows: PekoWindowManager;
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
      /** True when this document is a native pop-up window child. */
      popup?: boolean;
      /** The pop-up id, so it can report itself closed to the opener. */
      popupId?: string;
    };
    peko?: PekoClient;
    __peko_deeplink?: (path: string) => void;
  }
}
