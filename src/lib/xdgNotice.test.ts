import { describe, expect, it } from "vitest";
import { xdgLeakNoticeBody } from "./xdgNotice";

describe("xdgLeakNoticeBody", () => {
  const body = xdgLeakNoticeBody(["state/gh", "config/gh", "data/claude"]);
  const lines = body.split("\n");

  it("tells the user how to log gh back in", () => {
    expect(body).toContain("gh auth login");
  });

  it("lists one bullet per dir in input order", () => {
    const bullets = lines.filter((l) => l.startsWith("  • "));
    expect(bullets).toEqual(["  • state/gh", "  • config/gh", "  • data/claude"]);
  });

  it("says nothing was copied or deleted", () => {
    expect(lines).toContain("Nothing was copied or deleted. The old folders are still there:");
  });

  it("names the fixed default root and never a credential file", () => {
    expect(body).toContain("~/.keitora/herdr");
    expect(body).not.toContain("hosts.yml");
  });
});
