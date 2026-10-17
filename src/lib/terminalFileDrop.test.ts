import { describe, expect, it } from "vitest";
import {
  cssPointFromPhysical,
  injectionTextForPaths,
  paneAtPoint,
  paneElementAtPoint,
  quotePathForInjection,
} from "./terminalFileDrop";

// unit tests for the path-injection + pane-routing helpers.
//
// vitest runs in the default node environment here (no jsdom configured), so
// paneAtPoint is exercised through its injectable PointResolver seam rather than a
// real DOM. The end-to-end drag/paste behavior is smoke-tested by dogfooding —
// it depends on the Tauri webview event and the OS clipboard.

/** A stub element whose `.closest('.pane')` resolves to a pane with the given data attrs. */
function hitOnPane(project: string, name: string): Element {
  const pane = { dataset: { project, name } };
  return {
    closest: (sel: string) => (sel === ".pane" ? pane : null),
  } as unknown as Element;
}

/** A stub element that is not inside any pane. */
function hitOutsidePane(): Element {
  return { closest: () => null } as unknown as Element;
}

describe("quotePathForInjection", () => {
  it("leaves a path without whitespace untouched", () => {
    expect(quotePathForInjection("C:\\repos\\proj\\shot.png")).toBe("C:\\repos\\proj\\shot.png");
    expect(quotePathForInjection("/tmp/shot.png")).toBe("/tmp/shot.png");
  });

  it("wraps a path containing spaces in double quotes", () => {
    expect(quotePathForInjection("C:\\My Pictures\\shot.png")).toBe('"C:\\My Pictures\\shot.png"');
  });

  it("preserves Windows backslashes rather than rewriting separators", () => {
    expect(quotePathForInjection("C:\\a b\\c.png")).toContain("\\a b\\");
  });

  it("escapes embedded double quotes", () => {
    expect(quotePathForInjection('/tmp/od"d name.png')).toBe('"/tmp/od\\"d name.png"');
  });

  it("strips an embedded newline rather than quoting it", () => {
    // A filename may legally contain \n on macOS/Linux, and xterm's paste() turns \n
    // into \r — which would submit the prompt half-way through the path.
    expect(quotePathForInjection("/tmp/a\nb.png")).toBe("/tmp/ab.png");
    expect(quotePathForInjection("/tmp/a\rb.png")).toBe("/tmp/ab.png");
  });

  it("strips other control characters", () => {
    expect(quotePathForInjection("/tmp/a\u0007b\u0000c.png")).toBe("/tmp/abc.png");
  });

  it("still quotes a tab-bearing path only after control stripping", () => {
    // \t is a control char, so it is removed — the result needs no quoting.
    expect(quotePathForInjection("/tmp/a\tb.png")).toBe("/tmp/ab.png");
  });
});

describe("injectionTextForPaths", () => {
  it("returns an empty string for no paths", () => {
    expect(injectionTextForPaths([])).toBe("");
  });

  it("appends a trailing space after a single path", () => {
    expect(injectionTextForPaths(["/tmp/a.png"])).toBe("/tmp/a.png ");
  });

  it("space-joins multiple paths and still trails a space", () => {
    expect(injectionTextForPaths(["/tmp/a.png", "/tmp/b.png"])).toBe("/tmp/a.png /tmp/b.png ");
  });

  it("quotes only the paths that need it", () => {
    expect(injectionTextForPaths(["/tmp/a b.png", "/tmp/c.png"])).toBe(
      '"/tmp/a b.png" /tmp/c.png ',
    );
  });

  it("never emits a newline (a drop must not auto-submit)", () => {
    expect(injectionTextForPaths(["/tmp/a.png", "/tmp/b.png"])).not.toContain("\n");
  });

  it("drops empty entries", () => {
    expect(injectionTextForPaths(["", "/tmp/a.png"])).toBe("/tmp/a.png ");
    expect(injectionTextForPaths([""])).toBe("");
  });
});

describe("cssPointFromPhysical", () => {
  it("is identity at a ratio of 1", () => {
    expect(cssPointFromPhysical({ x: 100, y: 200 }, 1)).toEqual({ x: 100, y: 200 });
  });

  it("divides by the device pixel ratio on a HiDPI display", () => {
    expect(cssPointFromPhysical({ x: 300, y: 150 }, 1.5)).toEqual({ x: 200, y: 100 });
    expect(cssPointFromPhysical({ x: 400, y: 200 }, 2)).toEqual({ x: 200, y: 100 });
  });

  it("treats a non-positive ratio as 1 rather than zeroing the point", () => {
    expect(cssPointFromPhysical({ x: 100, y: 200 }, 0)).toEqual({ x: 100, y: 200 });
    expect(cssPointFromPhysical({ x: 100, y: 200 }, -2)).toEqual({ x: 100, y: 200 });
  });
});

describe("paneAtPoint", () => {
  it("returns the pane's write identity", () => {
    expect(paneAtPoint(10, 10, () => hitOnPane("proj", "epsilon"))).toEqual({
      project: "proj",
      name: "epsilon",
    });
  });

  it("returns null when the point is not inside a pane", () => {
    expect(paneAtPoint(10, 10, () => hitOutsidePane())).toBeNull();
  });

  it("returns null when no element is at the point", () => {
    expect(paneAtPoint(10, 10, () => null)).toBeNull();
  });

  it("returns null when the pane is missing its data attributes", () => {
    const bare = { closest: () => ({ dataset: {} }) } as unknown as Element;
    expect(paneAtPoint(10, 10, () => bare)).toBeNull();
  });

  it("passes the queried point through to the resolver", () => {
    const seen: Array<[number, number]> = [];
    paneAtPoint(42, 99, (x, y) => {
      seen.push([x, y]);
      return hitOnPane("proj", "epsilon");
    });
    expect(seen).toEqual([[42, 99]]);
  });
});

describe("paneElementAtPoint", () => {
  it("returns the pane element a drop should be dispatched at", () => {
    const el = paneElementAtPoint(10, 10, () => hitOnPane("proj", "epsilon"));
    expect((el as unknown as { dataset: Record<string, string> })?.dataset).toEqual({
      project: "proj",
      name: "epsilon",
    });
  });

  it("returns null outside any pane, and when nothing is at the point", () => {
    expect(paneElementAtPoint(10, 10, () => hitOutsidePane())).toBeNull();
    expect(paneElementAtPoint(10, 10, () => null)).toBeNull();
  });

  it("returns the element even when data attributes are missing", () => {
    // paneAtPoint rejects this (no identity to write to) but the element still exists;
    // keeping the two in step would silently swallow a malformed pane.
    const bare = { closest: () => ({ dataset: {} }) } as unknown as Element;
    expect(paneElementAtPoint(10, 10, () => bare)).not.toBeNull();
    expect(paneAtPoint(10, 10, () => bare)).toBeNull();
  });
});
