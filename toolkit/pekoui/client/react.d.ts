// Type declarations for @peko/client/react.

import type { ReactNode, CSSProperties } from 'react';
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

/** Props to spread onto an element to make it the window drag region. */
export function useDraggable(): { 'data-peko-drag'?: '' };

/** Subscribe to a native push event for the component's lifetime. */
export function usePekoEvent(name: string, handler: (data: unknown) => void): void;

/** Wire native navigate events to a navigate callback (e.g. a router). */
export function useNavigate(onNavigate: (path: string) => void): void;

export interface ToolbarProps {
  children?: ReactNode;
  /** Show window controls on a frameless desktop window. Default true. */
  controls?: boolean;
  /** Act as the drag region on a frameless window. Default true. */
  drag?: boolean;
  /** Render nothing on mobile. */
  hideOnMobile?: boolean;
  className?: string;
  style?: CSSProperties;
}

/** A platform-aware app toolbar / titlebar. */
export function Toolbar(props: ToolbarProps): JSX.Element | null;

/** Kept for compatibility: a Toolbar by its previous name. */
export const Titlebar: typeof Toolbar;

export interface MenuProps {
  /** The menu definition (a stable reference is expected). */
  items: PekoMenuTop[];
  /** Render even where a native menu bar exists. */
  force?: boolean;
}

/** An HTML menu, rendered only where there is no native menu bar. */
export function Menu(props: MenuProps): JSX.Element;

export const peko: PekoClient;
export default peko;
