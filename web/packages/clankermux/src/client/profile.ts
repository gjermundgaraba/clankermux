import type { Profile, Scheme } from "../proto/index.ts";

/** `#rrggbb` from a CSS colour as a custom property holds it (hex, or
 * `rgb()` when the stylesheet writes it that way); undefined otherwise. */
export function hexColor(value: string): string | undefined {
  const text = value.trim();
  const hex = /^#([0-9a-f]{6})$/i.exec(text);
  if (hex) return `#${hex[1]!.toLowerCase()}`;
  const short = /^#([0-9a-f]{3})$/i.exec(text);
  if (short) return `#${[...short[1]!].map((c) => c + c).join("").toLowerCase()}`;
  const rgb = /^rgba?\(\s*(\d+)\s*[, ]\s*(\d+)\s*[, ]\s*(\d+)/.exec(text);
  if (!rgb) return undefined;
  const channel = (s: string) => Math.min(255, Number(s)).toString(16).padStart(2, "0");
  return `#${channel(rgb[1]!)}${channel(rgb[2]!)}${channel(rgb[3]!)}`;
}

/** The profile a page with these colours and scheme reports. */
export function profileFrom(colors: { fg: string; bg: string; cursor: string }, scheme: Scheme): Profile {
  const profile: Profile = { scheme };
  const fg = hexColor(colors.fg);
  const bg = hexColor(colors.bg);
  const cursor = hexColor(colors.cursor);
  if (fg) profile.fg = fg;
  if (bg) profile.bg = bg;
  if (cursor) profile.cursor = cursor;
  return profile;
}
