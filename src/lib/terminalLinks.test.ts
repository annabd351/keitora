import type { ILink, ILinkProvider, Terminal } from "@xterm/xterm";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  activateTerminalLink,
  classifyTerminalLink,
  createFileLinkProvider,
  findFileReferences,
  installTerminalLinks,
} from "./terminalLinks";

vi.hoisted(() => {
  Object.defineProperty(globalThis, "self", { value: globalThis, configurable: true });
});

afterEach(() => vi.unstubAllGlobals());

describe("terminal destination classification", () => {
  it.each([
    ["D:/repo/file.ts:42:3", "D:/repo/file.ts", 42, 3],
    ["/D:/repo/file.ts:42", "D:/repo/file.ts", 42, undefined],
    ["D:\\repo\\file.ts:2", "D:\\repo\\file.ts", 2, undefined],
    ["src/file.ts:9", "src/file.ts", 9, undefined],
    ["file.ts:7", "file.ts", 7, undefined],
    ["/repo/file.ts", "/repo/file.ts", undefined, undefined],
    ["file:///D:/My%20Project/a%2520b.ts#L42C3", "D:/My Project/a%20b.ts", 42, 3],
    ["file://localhost/repo/file.ts:5", "/repo/file.ts", 5, undefined],
    ["src/a%20b.ts", "src/a%20b.ts", undefined, undefined],
    ["README.md", "README.md", undefined, undefined],
  ])("parses %s", (input, path, line, column) => {
    expect(classifyTerminalLink(input)).toEqual({
      kind: "file",
      path,
      ...(line ? { line } : {}),
      ...(column ? { column } : {}),
    });
  });

  it.each([
    "javascript:alert(1)",
    "data:text/html,test",
    "mailto:a@b.com",
    "https:example.com",
    "https://",
    "https://exa mple.com",
    "https://example.com\\evil",
    "file://server/share/a.ts",
    "file:///x/%ZZ.ts",
    "file:///x/a.ts?query",
    "file:///x/a.ts#unexpected",
    "file:////server/a.ts",
    "\\\\server\\a.ts",
    "\\\\?\\D:\\a.ts",
    "D:relative.ts",
    "D:/x/a.ts:stream",
    "src/a.ts:0",
    "src/a.ts\n",
    "file:///x/a%00.ts",
  ])("rejects %s", (input) => {
    expect(classifyTerminalLink(input)).toBeNull();
  });

  it("accepts only explicit web URLs", () => {
    expect(classifyTerminalLink("HTTPS://Example.com/a?q=1#part")).toEqual({
      kind: "web",
      url: "https://example.com/a?q=1#part",
    });
  });

  it("requires delimiter context for root-level filenames with spaces", () => {
    expect(classifyTerminalLink("hello world.txt")).toBeNull();
    expect(classifyTerminalLink("hello world.txt", true)).toEqual({
      kind: "file",
      path: "hello world.txt",
    });
    expect(classifyTerminalLink("ordinary prose", true)).toBeNull();
    expect(classifyTerminalLink("javascript:hello world.txt", true)).toBeNull();
  });
});

describe("printed file references", () => {
  it("recognizes delimited spaces and excludes web destinations", () => {
    const text =
      'See `src/My File.ts:4`, "D:/My Project/a.ts:5" and [file](</D:/My Project/b.ts:6>). https://example.com/src/no.ts src/yes.ts:7.';
    const matches = findFileReferences(text);
    expect(matches.map((match) => match.text)).toEqual([
      "src/My File.ts:4",
      "D:/My Project/a.ts:5",
      "/D:/My Project/b.ts:6",
      "src/yes.ts:7",
    ]);
    for (const match of matches)
      expect(text.slice(match.index, match.index + match.text.length)).toBe(match.text);
  });
  it("does not turn URL substrings into file links", () => {
    expect(findFileReferences("https://example.com/src/a.ts javascript:src/a.ts")).toEqual([]);
  });
  it("accepts filenames in Markdown and backticks", () => {
    expect(
      findFileReferences("[README](README.md) `README.md`").map((match) => match.text),
    ).toEqual(["README.md", "README.md"]);
  });
});

function actions() {
  return {
    context: () => ({ project: "project", worktree: "epsilon", active: true }),
    openUrl: vi.fn(async () => {}),
    openFile: vi.fn(async () => {}),
    showError: vi.fn(),
  };
}

describe("native activation", () => {
  it("uses current pane identity and strips locations", async () => {
    const deps = actions();
    deps.context = () => ({
      project: "new-project",
      worktree: "echo",
      active: true,
    });
    await activateTerminalLink("src/a.ts:42:3", deps);
    expect(deps.openFile).toHaveBeenCalledWith("new-project", "echo", "src/a.ts");
    expect(deps.openUrl).not.toHaveBeenCalled();
  });
  it("opens web links locally", async () => {
    const deps = actions();
    await activateTerminalLink("https://example.com", deps);
    expect(deps.openUrl).toHaveBeenCalledWith("https://example.com/");
  });
  it("reports unsupported links and native errors", async () => {
    const deps = actions();
    await activateTerminalLink("javascript:alert(1)", deps);
    expect(deps.openUrl).not.toHaveBeenCalled();
    expect(deps.openFile).not.toHaveBeenCalled();
    deps.openUrl.mockRejectedValueOnce(new Error("browser unavailable"));
    await activateTerminalLink("https://example.com", deps);
    expect(deps.showError).toHaveBeenLastCalledWith("Could not open link: browser unavailable");
    deps.openFile.mockRejectedValueOnce("Missing file");
    await activateTerminalLink("src/missing.ts", deps);
    expect(deps.showError).toHaveBeenLastCalledWith("Could not open link: Missing file");
  });
  it("ignores hidden panes", async () => {
    const deps = actions();
    deps.context = () => ({ project: "p", worktree: "w", active: false });
    await activateTerminalLink("src/a.ts", deps);
    expect(deps.openFile).not.toHaveBeenCalled();
  });
});

function terminalFixture(rows: string[], cols: number) {
  const providers: ILinkProvider[] = [];
  const disposals: ReturnType<typeof vi.fn>[] = [];
  const disposable = () => {
    const dispose = vi.fn();
    disposals.push(dispose);
    return { dispose };
  };
  const lines = rows.map((row, index) => {
    const cells: { getChars(): string; getWidth(): number }[] = [];
    for (const segment of row.matchAll(/.[\u0300-\u036f]*/gu)) {
      const width = segment[0] === "界" ? 2 : 1;
      cells.push({ getChars: () => segment[0], getWidth: () => width });
      if (width === 2) cells.push({ getChars: () => "", getWidth: () => 0 });
    }
    while (cells.length < cols) cells.push({ getChars: () => "", getWidth: () => 1 });
    return {
      length: cols,
      isWrapped: index > 0,
      getCell: (col: number, target?: object) => {
        if (target) Object.assign(target, cells[col]);
        return cells[col];
      },
      translateToString: (trim: boolean) => (trim ? row.trimEnd() : row.padEnd(cols)),
    };
  });
  const terminal = {
    cols,
    hasSelection: () => false,
    options: { linkHandler: undefined },
    buffer: { active: { getLine: (row: number) => lines[row], getNullCell: () => ({}) } },
    loadAddon: (addon: { activate(term: Terminal): void }) =>
      addon.activate(terminal as unknown as Terminal),
    registerLinkProvider: (provider: ILinkProvider) => {
      providers.push(provider);
      return disposable();
    },
    onScroll: disposable,
    onResize: disposable,
  } as unknown as Terminal;
  return { terminal, providers, disposals };
}

function linksAt(provider: ILinkProvider, row: number) {
  let links: ILink[] = [];
  provider.provideLinks(row, (result) => {
    links = result ?? [];
  });
  return links;
}

it("maps wrapped file references after wide and combining cells", () => {
  const { terminal } = terminalFixture(["界e\u0301 src/long", "file.ts:42"], 12);
  const provider = createFileLinkProvider(terminal, { activate: vi.fn() });
  for (const row of [1, 2]) {
    expect(linksAt(provider, row)[0]).toMatchObject({
      text: "src/longfile.ts:42",
      range: { start: { x: 5, y: 1 }, end: { x: 10, y: 2 } },
    });
  }
});

it.each([
  '"hello world.txt"',
  "'hello world.txt'",
  "`hello world.txt`",
  "[file](<hello world.txt>)",
  "[file](hello world.txt)",
])("opens the full delimited filename in %s", async (reference) => {
  const tooltip = { style: {}, setAttribute: vi.fn(), remove: vi.fn() };
  vi.stubGlobal("document", { createElement: () => tooltip, body: { appendChild: vi.fn() } });
  vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener: vi.fn() });
  const { terminal, providers } = terminalFixture([reference], reference.length);
  const deps = actions();
  const installed = installTerminalLinks(terminal, deps);
  const links = linksAt(providers[1], 1);
  expect(links).toHaveLength(1);
  const link = links[0];
  expect(link.text).toBe("hello world.txt");
  expect(link.range).toEqual({
    start: { x: reference.indexOf("hello world.txt") + 1, y: 1 },
    end: { x: reference.indexOf("hello world.txt") + "hello world.txt".length, y: 1 },
  });
  link.activate({ button: 0 } as MouseEvent, link.text);
  await Promise.resolve();
  expect(deps.openFile).toHaveBeenCalledOnce();
  expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "hello world.txt");
  expect(deps.openUrl).not.toHaveBeenCalled();
  expect(deps.showError).not.toHaveBeenCalled();
  installed.dispose();
});

it("wires web addon and OSC handlers and disposes hover and registrations", async () => {
  const tooltip = { style: {}, setAttribute: vi.fn(), remove: vi.fn(), textContent: "" };
  const appendChild = vi.fn();
  vi.stubGlobal("document", { createElement: () => tooltip, body: { appendChild } });
  const removeEventListener = vi.fn();
  vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener });
  const { terminal, providers, disposals } = terminalFixture(["https://exam", "ple.com/a."], 12);
  const deps = actions();
  const installed = installTerminalLinks(terminal, deps);
  const web = linksAt(providers[0], 1)[0];
  expect(web.text).toBe("https://example.com/a");
  const event = { button: 0 } as MouseEvent;
  web.activate(event, web.text);
  await Promise.resolve();
  expect(deps.openUrl).toHaveBeenCalledWith("https://example.com/a");
  const handler = terminal.options.linkHandler;
  expect(handler?.allowNonHttpProtocols).toBe(true);
  handler?.hover?.(event, "file:///D:/repo/a.ts#L7", web.range);
  expect(tooltip.textContent).toBe("file:///D:/repo/a.ts#L7");
  expect(appendChild).toHaveBeenCalledWith(tooltip);
  handler?.activate(event, "file:///D:/repo/a.ts#L7", web.range);
  await Promise.resolve();
  expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "D:/repo/a.ts");
  deps.openFile.mockClear();
  handler?.activate({ button: 2 } as MouseEvent, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  terminal.hasSelection = () => true;
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  terminal.hasSelection = () => false;
  const listeners = vi.mocked(window.addEventListener).mock.calls;
  const down = listeners.find(([name]) => name === "mousedown")?.[1] as (event: MouseEvent) => void;
  const move = listeners.find(([name]) => name === "mousemove")?.[1] as (event: MouseEvent) => void;
  down({ clientX: 10, clientY: 10 } as MouseEvent);
  move({ clientX: 30, clientY: 10, buttons: 1 } as MouseEvent);
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  down({ clientX: 10, clientY: 10 } as MouseEvent);
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).toHaveBeenCalledOnce();
  installed.hide();
  installed.dispose();
  expect(tooltip.remove).toHaveBeenCalled();
  expect(removeEventListener).toHaveBeenCalledWith("blur", installed.hide);
  expect(disposals.every((dispose) => dispose.mock.calls.length === 1)).toBe(true);
  expect(terminal.options.linkHandler).toBeUndefined();
});
