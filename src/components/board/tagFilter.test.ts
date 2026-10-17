import { describe, expect, it } from "vitest";
import type { TaskWithDeps } from "../../lib/tasks";
import { activeSelection, availableTags, filterTags, filterTasksByTags } from "./tagFilter";

// Minimal factory: only `tags` matters for these pure helpers, so the rest is
// cast away. Keeps the tests focused on filtering behavior.
function task(id: number, tags: string[]): TaskWithDeps {
  return { id, tags } as unknown as TaskWithDeps;
}

describe("availableTags", () => {
  it("returns the sorted, de-duplicated union of tags", () => {
    const tasks = [task(1, ["ui", "bug"]), task(2, ["bug", "infra"]), task(3, ["ui"])];
    expect(availableTags(tasks)).toEqual(["bug", "infra", "ui"]);
  });

  it("sorts case-insensitively", () => {
    const tasks = [task(1, ["Zebra"]), task(2, ["apple"]), task(3, ["Mango"])];
    expect(availableTags(tasks)).toEqual(["apple", "Mango", "Zebra"]);
  });

  it("returns [] when no task has tags", () => {
    expect(availableTags([task(1, []), task(2, [])])).toEqual([]);
    expect(availableTags([])).toEqual([]);
  });

  it("keeps case-only-distinct tags in a deterministic order", () => {
    // "Bug" and "bug" are distinct entries (tags are case-sensitive). The exact
    // which-case-first is locale-dependent, but the tiebreak must make it stable:
    // the output is identical regardless of input order, and both are present.
    const a = availableTags([task(1, ["bug"]), task(2, ["Bug"])]);
    const b = availableTags([task(1, ["Bug"]), task(2, ["bug"])]);
    expect(a).toEqual(b);
    expect([...a].sort()).toEqual(["Bug", "bug"]);
  });
});

describe("filterTags", () => {
  const tags = ["bug", "infra", "ui", "UX"];

  it("returns all tags when the query is blank", () => {
    expect(filterTags(tags, "")).toBe(tags); // passthrough, same reference
  });

  it("returns all tags when the query is whitespace only", () => {
    expect(filterTags(tags, "   ")).toBe(tags);
  });

  it("matches case-insensitively", () => {
    expect(filterTags(tags, "UI")).toEqual(["ui"]);
    expect(filterTags(tags, "ux")).toEqual(["UX"]);
  });

  it("matches on a substring, not just a prefix", () => {
    expect(filterTags(tags, "fra")).toEqual(["infra"]);
  });

  it("returns [] when nothing matches", () => {
    expect(filterTags(tags, "zzz")).toEqual([]);
  });

  it("preserves input order among matches", () => {
    expect(filterTags(["ui", "bug", "guiding", "ubiquity"], "u")).toEqual([
      "ui",
      "bug",
      "guiding",
      "ubiquity",
    ]);
  });
});

describe("activeSelection", () => {
  it("returns the same (empty) reference when nothing is selected", () => {
    const empty: string[] = [];
    expect(activeSelection(empty, ["ui", "bug"])).toBe(empty);
  });

  it("keeps only selected tags that still exist, preserving selection order", () => {
    expect(activeSelection(["ui", "bug"], ["bug", "infra", "ui"])).toEqual(["ui", "bug"]);
  });

  it("drops selected tags that no longer exist", () => {
    expect(activeSelection(["ui", "gone"], ["ui", "bug"])).toEqual(["ui"]);
  });

  it("returns [] when no available tags remain (total vanish)", () => {
    expect(activeSelection(["ui"], [])).toEqual([]);
  });
});

describe("filterTasksByTags", () => {
  const tasks = [task(1, ["ui"]), task(2, ["bug", "infra"]), task(3, ["ui", "bug"]), task(4, [])];

  it("returns all tasks unchanged when no tag is selected", () => {
    const result = filterTasksByTags(tasks, []);
    expect(result).toBe(tasks); // passthrough, same reference
  });

  it("keeps only tasks carrying the single selected tag", () => {
    expect(filterTasksByTags(tasks, ["ui"]).map((t) => t.id)).toEqual([1, 3]);
  });

  it("treats multiple selected tags as an inclusive OR", () => {
    // infra (task 2) OR ui (tasks 1, 3) → 1, 2, 3
    expect(filterTasksByTags(tasks, ["ui", "infra"]).map((t) => t.id)).toEqual([1, 2, 3]);
  });

  it("excludes tasks with no tags when a filter is active", () => {
    expect(filterTasksByTags(tasks, ["ui"]).some((t) => t.id === 4)).toBe(false);
  });

  it("returns [] when the selected tag matches nothing", () => {
    expect(filterTasksByTags(tasks, ["nonexistent"])).toEqual([]);
  });
});
