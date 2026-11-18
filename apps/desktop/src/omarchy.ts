// Follows the Omarchy theme: t3boat's colour tokens, its monospace font and the pixel sea all take
// their colours from the theme Omarchy applied (its colors.toml), and change with each
// `omarchy theme set`. Off Omarchy, and in the browser mock, nothing changes.
import { forceTheme } from "./pixel/clock";
import { setPaletteOverride, type BoatColors, type Palette } from "./pixel/palette";
import { rgb } from "./pixel/px";
import type { OmarchyTheme } from "./types";

type Rgb = [number, number, number];

const toRgb = (hex: string): Rgb => {
  const n = parseInt(hex.slice(1), 16);
  return [(n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff];
};
const toHex = (c: Rgb) => `#${c.map((v) => Math.round(v).toString(16).padStart(2, "0")).join("")}`;

/** `a` moved `t` of the way to `b` (0: a, 1: b). */
function mix(a: string, b: string, t: number): string {
  const [x, y] = [toRgb(a), toRgb(b)];
  return toHex([0, 1, 2].map((i) => x[i] + (y[i] - x[i]) * t) as Rgb);
}

/** The theme's colours, with t3boat's fallbacks for the ones a theme may leave out. */
function swatch(t: OmarchyTheme) {
  const c = t.colors;
  const bg = c.background;
  const fg = c.foreground;
  const accent = c.accent;
  return {
    bg,
    fg,
    accent,
    blue: c.blue ?? accent,
    red: c.red ?? "#e06c75",
    muted: c.muted ?? mix(fg, bg, 0.45),
    bright: c.bright_foreground ?? c.light_foreground ?? fg,
    light: c.light_foreground ?? fg,
    darker: c.darker_background ?? mix(bg, "#000000", 0.35),
    darkBg: c.dark_background ?? mix(bg, "#000000", 0.18),
  };
}

/** t3boat's tokens (brand.css) in the theme's colours. */
function tokens(t: OmarchyTheme): Record<string, string> {
  const s = swatch(t);
  const tint = (k: number) => mix(s.bg, s.fg, k);
  const out: Record<string, string> = {
    "--bg": s.bg,
    "--fg": s.fg,
    "--muted": s.muted,
    "--line": tint(0.12),
    "--frame": tint(0.3),
    "--panel": tint(0.06),
    "--accent": s.accent,
    "--accent-fg": s.bg,
    "--accent-strong": mix(s.accent, s.fg, 0.35),
    "--accent-text": s.accent,
    "--danger": s.red,
    "--shadow": s.darker,
    "--tag-bg": s.fg,
    "--tag-fg": s.bg,
    "--tag-dim-bg": tint(0.15),
    "--tag-dim-fg": s.muted,
    "--blue-200": tint(0.25),
    "--blue-300": tint(0.4),
    "--blue-400": tint(0.55),
  };
  if (t.font) out["--font-mono"] = `"${t.font}", ui-monospace, monospace`;
  return out;
}

const hexes = (list: string[]) => list.map(rgb);

function boat(sail: string, sailShade: string, deck: string, hull: string, keel: string, mast: string): BoatColors {
  return { sail: rgb(sail), sailShade: rgb(sailShade), deck: rgb(deck), hull: rgb(hull), keel: rgb(keel), mast: rgb(mast) };
}

/** The pixel sea in the theme's colours: water and sky from its blue, the light from its accent. */
function palette(t: OmarchyTheme): Palette {
  const s = swatch(t);
  const toBg = (c: string, k: number) => mix(c, s.bg, k);
  if (t.mode === "light") {
    return {
      bg: rgb(s.bg),
      sky: hexes([s.blue, s.blue, ...[0.15, 0.3, 0.5, 0.7, 0.85, 0.93].map((k) => toBg(s.blue, k))]),
      water: hexes([s.bg, ...[0.88, 0.75, 0.55, 0.35, 0.18].map((k) => toBg(s.blue, k)), s.blue]),
      waterFrom: 2,
      waterTo: 6,
      waterFade: 0.6,
      shore: rgb(toBg(s.blue, 0.4)),
      tower: rgb(mix(s.blue, s.fg, 0.6)),
      towerStripe: rgb(s.bg),
      lamp: rgb(s.bg),
      glow: hexes([s.bg, toBg(s.accent, 0.9), toBg(s.accent, 0.8)]),
      star: [],
      cloud: hexes([s.bg, toBg(s.blue, 0.95), toBg(s.blue, 0.88), toBg(s.blue, 0.75)]),
      foam: rgb(s.bg),
      flag: rgb(s.accent),
      reflection: rgb(mix(s.blue, s.fg, 0.6)),
      sailReflection: rgb(toBg(s.blue, 0.75)),
      boat: boat(s.bg, toBg(s.blue, 0.75), toBg(s.blue, 0.2), s.accent, mix(s.accent, s.fg, 0.5), mix(s.fg, s.bg, 0.2)),
      boatAsleep: boat(toBg(s.blue, 0.88), toBg(s.blue, 0.75), toBg(s.blue, 0.4), toBg(s.accent, 0.3), mix(s.accent, s.fg, 0.4), mix(s.fg, s.bg, 0.3)),
    };
  }
  return {
    bg: rgb(s.bg),
    sky: hexes([s.darker, s.darkBg, s.bg, ...[0.12, 0.22, 0.35, 0.5].map((k) => mix(s.bg, s.blue, k))]),
    water: hexes([
      mix(s.blue, s.bright, 0.55),
      mix(s.blue, s.bright, 0.3),
      s.blue,
      ...[0.25, 0.45, 0.6, 0.72, 0.82].map((k) => toBg(s.blue, k)),
    ]),
    waterFrom: 4,
    waterTo: 7,
    waterFade: 7,
    shore: rgb(s.darker),
    tower: rgb(s.darker),
    towerStripe: rgb(s.accent),
    lamp: rgb(s.bright),
    glow: hexes([mix(s.bright, s.accent, 0.25), toBg(s.accent, 0.15), toBg(s.accent, 0.5)]),
    star: hexes([s.bright, mix(s.fg, s.bg, 0.35), mix(s.fg, s.bg, 0.6)]),
    cloud: [],
    foam: rgb(mix(s.fg, s.blue, 0.3)),
    flag: rgb(s.accent),
    reflection: rgb(s.darker),
    sailReflection: rgb(toBg(s.blue, 0.35)),
    boat: boat(s.bright, mix(s.fg, s.bg, 0.3), mix(s.fg, s.blue, 0.5), s.accent, toBg(s.accent, 0.45), s.light),
    boatAsleep: boat(
      mix(s.fg, s.bg, 0.25),
      mix(s.fg, s.bg, 0.45),
      toBg(mix(s.fg, s.blue, 0.5), 0.3),
      toBg(s.accent, 0.35),
      toBg(s.accent, 0.6),
      mix(s.fg, s.bg, 0.45),
    ),
  };
}

let applied: string[] = [];

/** Applies `t`, or goes back to t3boat's own look with `null`. */
export function applyOmarchyTheme(t: OmarchyTheme | null) {
  const root = document.documentElement;
  applied.forEach((name) => root.style.removeProperty(name));
  applied = [];
  if (!t) {
    root.removeAttribute("data-omarchy-theme");
    root.style.removeProperty("color-scheme");
    setPaletteOverride(null);
    forceTheme(null);
    return;
  }
  for (const [name, value] of Object.entries(tokens(t))) {
    root.style.setProperty(name, value);
    applied.push(name);
  }
  root.style.setProperty("color-scheme", t.mode === "light" ? "light" : "dark");
  root.setAttribute("data-omarchy-theme", t.name);
  setPaletteOverride(palette(t));
  forceTheme(t.mode === "light" ? "light" : "dark");
}

/** In the app: applies the current Omarchy theme and every later one. */
export async function followOmarchyTheme() {
  if (!("__TAURI_INTERNALS__" in window)) return;
  const [{ invoke }, { listen }] = await Promise.all([import("@tauri-apps/api/core"), import("@tauri-apps/api/event")]);
  await listen<OmarchyTheme | null>("omarchy-theme", (e) => applyOmarchyTheme(e.payload));
  applyOmarchyTheme(await invoke<OmarchyTheme | null>("omarchy_theme"));
}
