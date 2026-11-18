// One shared, throttled animation clock for every pixel canvas. Pixel art looks
// best stepped, so we tick at 12 fps. Reduced motion: no clock at all.
import { useEffect, useSyncExternalStore } from "react";
import type { Theme } from "./palette";

const FPS = 12;
const subs = new Set<(t: number) => void>();
let raf = 0;
let last = 0;
/** How many times faster than real time the clock runs, and the seconds that has put it ahead. */
let rate = 1;
let ahead = 0;

const clock = () => performance.now() / 1000 + ahead;

function frame(now: number) {
  raf = requestAnimationFrame(frame);
  if (document.hidden || now - last < 1000 / FPS) return;
  ahead += ((now - last) / 1000) * (rate - 1);
  last = now;
  const t = clock();
  subs.forEach((f) => f(t));
}

/** Runs the clock `times` as fast as real time, until told 1 again: the mock's fast-forward. */
export function timeLapse(times: number) {
  rate = times;
}

function subscribe(f: (t: number) => void) {
  subs.add(f);
  if (subs.size === 1) raf = requestAnimationFrame(frame);
  return () => {
    subs.delete(f);
    if (subs.size === 0) cancelAnimationFrame(raf);
  };
}

// Stores read the window only when used, so the site can render these components ahead of time.
const off = () => false;

function mediaStore(query: string) {
  return {
    subscribe: (cb: () => void) => {
      const mq = window.matchMedia(query);
      mq.addEventListener("change", cb);
      return () => mq.removeEventListener("change", cb);
    },
    get: () => window.matchMedia(query).matches,
  };
}

const motionStore = mediaStore("(prefers-reduced-motion: reduce)");
const darkStore = mediaStore("(prefers-color-scheme: dark)");

export function useReducedMotion(): boolean {
  return useSyncExternalStore(motionStore.subscribe, motionStore.get, off);
}

// re-armed on every change: a resolution query only matches the ratio it was made for
const dprStore = {
  subscribe: (cb: () => void) => {
    let mq: MediaQueryList;
    const watch = () => {
      mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
      mq.addEventListener("change", changed);
    };
    const changed = () => {
      mq.removeEventListener("change", changed);
      watch();
      cb();
    };
    watch();
    return () => mq.removeEventListener("change", changed);
  },
  get: () => window.devicePixelRatio,
};

/** Device px per CSS px; follows the window between screens. */
export function useDevicePixelRatio(): number {
  return useSyncExternalStore(dprStore.subscribe, dprStore.get, () => 1);
}

/** A theme that wins over the system's light/dark setting (the Omarchy theme's mode). */
let forced: Theme | null = null;
let forcedVersion = 0;
const forcedSubs = new Set<() => void>();
const forcedStore = {
  subscribe: (cb: () => void) => {
    forcedSubs.add(cb);
    return () => forcedSubs.delete(cb);
  },
  // a version, not the mode: another theme with the same mode must repaint too
  get: () => forcedVersion,
};

/** Sets the theme the pixel art draws in, or `null` to follow the system again. */
export function forceTheme(theme: Theme | null) {
  forced = theme;
  forcedVersion += 1;
  forcedSubs.forEach((cb) => cb());
}

export function useTheme(): Theme {
  useSyncExternalStore(forcedStore.subscribe, forcedStore.get, () => 0);
  const dark = useSyncExternalStore(darkStore.subscribe, darkStore.get, off);
  return forced ?? (dark ? "dark" : "light");
}

/**
 * Calls `draw(t)` on every tick, or once with `null` under reduced motion.
 * Redraws whenever `deps` change.
 */
export function useAnimation(draw: (t: number | null) => void, deps: unknown[]) {
  const reduced = useReducedMotion();
  useEffect(() => {
    if (reduced) {
      draw(null);
      return;
    }
    draw(clock());
    return subscribe(draw);
  }, [reduced, ...deps]);
}
