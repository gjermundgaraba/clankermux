import { expect, test } from "vitest";

import { hexColor, profileFrom, schemeOf } from "../../src/client/profile.ts";

test("computed colours normalise to #rrggbb", () => {
  expect(hexColor("rgb(40, 44, 52)")).toBe("#282c34");
  expect(hexColor("rgba(255, 0, 0, 0.5)")).toBe("#ff0000");
  expect(hexColor("rgb(40 44 52 / 1)")).toBe("#282c34");
  expect(hexColor("#ABCDEF")).toBe("#abcdef");
  expect(hexColor("#fff")).toBe("#ffffff");
  expect(hexColor("transparent")).toBeUndefined();
});

test("the profile carries only the colours that parsed", () => {
  expect(profileFrom({ fg: "rgb(1, 2, 3)", bg: "nope", cursor: "#fff" }, "dark")).toEqual({
    scheme: "dark",
    fg: "#010203",
    cursor: "#ffffff",
  });
});

test("the scheme is the one color-scheme names, else the preference", () => {
  expect(schemeOf("dark", true)).toBe("dark");
  expect(schemeOf("only light", false)).toBe("light");
  expect(schemeOf("light dark", false)).toBe("dark");
  expect(schemeOf("light dark", true)).toBe("light");
  expect(schemeOf("normal", true)).toBe("light");
  expect(schemeOf("", false)).toBe("dark");
});
