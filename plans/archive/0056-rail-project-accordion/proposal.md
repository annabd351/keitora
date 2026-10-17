# 0056 — Rail project accordion

Replace the top-bar project tab strip with collapsible project groups inside the left rail.
Any number of groups can be open at once; opening a group is a *peek* (no project switch);
clicking a worktree inside any group focuses it (switching project if needed).

## Goal

The tab strip doesn't scale: with 8+ projects it is a crowded, centred row of tiny labels whose
only signal is a session count, and it is spatially disconnected from the worktrees it governs.
The rail is already "the persistent spine and at-a-glance monitor" (build plan, Layout) — make it
the single place where projects *and* their worktrees live, so you can watch several projects at
once and jump straight to the worktree that needs you.

Success looks like:

- Every registered project is a group in the rail. Collapsed, a group still tells you what's going
  on inside it (live sessions, needs-input, busy/building, a dot per worktree).
- Expanded, a group shows that project's worktree lanes with live status lights — **including
  non-active projects**.
- The focused worktree (active project + selected worktree) is unmistakable at a glance, even with
  several groups open.
- The top bar is slimmer and calmer: brand · breadcrumb · view pills.
- It feels crafted: smooth expand/collapse, sticky group headers, no layout jank, reduced-motion
  respected, dark + light themes both polished.

## Scope

### In

1. **Rail project groups** (multi-expand accordion), persisted open/closed set.
2. **Collapsed header summary**: live-session count, needs-input alert, per-worktree status dot
   strip, busy/building hint.
3. **Cross-project lane data**: worktree info + session states for *every* registered project
   (new backend command), so non-active lanes show real status lights.
4. **Click-to-focus across projects**: clicking a lane in a non-active group switches project and
   selects that worktree in one motion (no "Switched to X" toast for this path).
5. **Stronger focus treatment** for the selected lane and the active project's header.
6. **Drag to reorder** project groups, persisted in the user config; keyboard project cycle
   (Cmd/Ctrl+←/→) follows the same order.
7. **Project context menu** on the header: New worktree…, Reveal in file explorer, Collapse
   others / Expand all, Remove project… (disabled for the active project, same rule as Settings).
8. **Slimmer top bar**: remove `ProjectTabs`; centre shows a quiet breadcrumb
   `project / worktree`; brand + view pills stay. Cycle hint (`⌘ ← →`) moves to the rail.
9. Delete `ProjectTabs.tsx` and its CSS; update stale comments and `docs/build-plan.md`.

### Out

- Rail collapse-to-icons mode (still unbuilt; separate effort).
- Running lane actions *in the background* on a non-active project. Non-active lanes keep the full
  worktree context menu, but an action first focuses that worktree (silent switch), then runs —
  see §6. No backend action gains a `project` argument.
- Task chips (`#id` + task title marquee) on non-active lanes — tasks are loaded for the active
  project only. Non-active lanes show name, provider glyph, status light, stage chip, branch and
  build bar (activity is already all-projects).
- Project rename. Cmd+1..9 still indexes the **active** project's available worktrees only.
- Changing attention/dock-badge semantics for background projects (the OS notification path in
  `bg_notifier` is untouched).
- The Settings drawer's registered-projects list and active-project `<select>` keep their current
  (alphabetical) order; only the rail, `list_projects` and the Cmd+←/→ cycle follow `project_order`.
- Switching project by clicking a group *header*: a header click only toggles open/closed. The
  only ways to change project are a lane click (or lane menu / "New worktree…"), the cycle chords,
  and Settings.

## Naming and keys (read first — the #1 source of bugs in this change)

Two different strings identify a project, and today they are conflated:

| Term | What it is | Where it comes from | Used for |
|---|---|---|---|
| **key** | registry key (`UserConfig.projects` map key) | `ProjectSummary.name` from `list_projects`; `ProjectOverview.key` | what `set_active_project` / `remove_project` / `reorder_projects` take; React keys; dnd-kit ids; `railExpanded` set; the label shown in headers and breadcrumb |
| **resolved name** | `[project].name` after config resolution (`pc.project.name`) | `ProjectSummary.resolved_name` (new); `ProjectOverview.resolved_name`; `current_project().name` | everything keyed by session/daemon namespace: `activity` map keys (`"<resolved>/<worktree>"`), `WorktreeSession.project`, `DaemonSessionHint.project`, `countsByProject`, warm-registry keys, `TerminalPane` `projectName`, `activeProjectName` in App |

They are equal for most projects but not guaranteed (a file-less project's name is the root
basename; a committed `keitora.toml` can set any name). Facts verified in the code today:

- `current_project` → `project_summary()` → `name = pc.project.name` (**resolved**).
- `list_projects` overwrites `summary.name = registry key` (**key**).
- App's `project` state is set from `currentProject()` (resolved) *and*, optimistically, from the
  `ProjectSummary` handed to `handleSwitchProject` (key, from `list_projects`). So today the warm
  registry (`warm.projects` keyed by `summary.name`) and the "already active" check silently break
  when key ≠ resolved. This change fixes it; every comparison below states which string it uses.

Rules for the implementer:

1. **Never compare a key to a resolved name.** Name variables `projectKey` / `resolvedName`.
2. Rail rows, `railExpanded`, reorder, dnd ids, menus, `remove_project`, `reorder_projects`,
   `set_active_project`: **key**.
3. Joining to `activity`, `sessions`, `countsByProject(daemonSessions)`, `warm`, `activeProjectName`,
   `useAttention`: **resolved name**.
4. "Is this group the active one?" ⇒ `summary.resolved_name === activeProjectName`.
5. App's `project` state always holds a summary whose `name` is the **resolved** name (matches what
   `current_project` returns). The optimistic flip therefore stores
   `{ ...summary, name: summary.resolved_name }` (see §6).
6. Displayed text (group header, breadcrumb project segment) is the **key** — that is what the
   tab strip displayed.

## Architecture

```
            ┌──────────────── App.tsx ─────────────────┐
            │ useProjectList()  ── order + summaries    │
            │ useProjectsOverview() ── NEW, all projects│
            │ useSessions/useAttention ── active (rich) │
            │ activity ── all projects (existing)       │
            └──────────────┬───────────────────────────┘
                           ▼
   Rail ─ RailProjects (dnd-kit sortable list)
            └─ ProjectGroup  (header + collapsible body)
                  ├─ ProjectHeader  (chevron · name · summary · grip/＋/⋯)
                  └─ Lane[]         (extracted from today's Rail lane render)
```

Data rule: **the active project's group renders from today's rich, active-only sources**
(`activeWorktrees`, `sessions`, `attention`, `activeTaskByWorktree`) — no behavioural regression for
the focused project. **Non-active groups render from the new overview** (worktrees + session
states), plus the existing all-projects `activity` map. One pure function builds a `GroupModel` per
project from those inputs so the rendering code never branches on "is this the active project" for
data. Exception (explicit): while the active project's rich sources are not yet ready (cold switch,
`warm.active !== activeProjectName`), the active group falls back to the overview path so it never
shows the previous project's lanes under the new header.

## Technical design

### 1. Backend: `projects_overview` command (src-tauri)

Lives in `src-tauri/src/host.rs`, next to `list_projects` (`host.rs:121`), because it needs the
private `worktrees_in` (`host.rs:53`) and `WorktreeView`. It calls
`crate::tasks::reconciled_sessions` (`src-tauri/src/tasks.rs:97`, `pub(crate)`).

```rust
#[derive(serde::Serialize)]
pub struct ProjectOverview {
    pub key: String,            // registry key (what set_active_project takes)
    pub resolved_name: String,  // pc.project.name (what activity/session/daemon keys use)
    pub worktrees: Vec<WorktreeView>,    // same wire shape as the `worktrees` command → TS WorktreeInfo
    pub sessions: Vec<WorktreeSession>,  // same wire shape as `sessions_list`, `.project` stamped = resolved_name
}

#[tauri::command(async)]
fn projects_overview(client: State<SharedBackend>) -> Result<Vec<ProjectOverview>, String>
```

- Registration: add `projects_overview,` to the `tauri::generate_handler![ … ]` list in
  `host.rs` (`.invoke_handler(` at `host.rs:1922`), directly after `list_projects,`. Add
  `reorder_projects,` (§5) directly after `set_active_project,`.
- Iterate projects in display order: `for key in user_cfg.ordered_project_names()` (§5), look up
  `user_cfg.projects[&key]`, `keitora_tasks::effective_config(pref)`; **skip** the project on
  error (same as `list_projects`). If `worktrees_in` returns `Err` for a project, skip it too.
- Take the shared sets **once per call**, before the loop:
  `let live = client.live_set_snapshot(); let bg = crate::backend::cached_background_set(client.inner().as_ref());`
  Both read caches (herdr: `live` is an in-memory `HashSet` clone; `cached_background_set` uses
  `cached_sessions()`) — no server query.
- Per-project body is a pure-ish helper (unit-testable without a Tauri `State`, mirrors
  `worktrees_in` taking `&dyn PtyBackend`):

  ```rust
  fn project_overview(
      client: &dyn PtyBackend,
      key: &str,
      pc: keitora_tasks::ProjectConfig,
      live: &HashSet<SessionKey>,
      bg: &HashSet<SessionKey>,
  ) -> Result<ProjectOverview, String>
  ```
  Order inside it matters because `worktrees_in` consumes `pc`: (1) `let resolved = pc.project.name.clone();`
  (2) sessions: if `pc.tasks.db_path.exists()` then
  `reconciled_sessions(live, bg, &resolved, &pc)?` with each row's `.project = resolved.clone()`,
  else `Vec::new()` — **guard against creating a database file for a never-used project**
  (`LocalStore::open` creates/initialises the DB; a background project must not get side effects
  from being listed); (3) `worktrees_in(client, Some(pc))?` (it calls `client.terminated_keys(&resolved)`).
- `reconciled_sessions` already scopes DB rows to that project's configured worktrees and applies
  the live/bg reconcile (`apply_reconcile`, `backend/mod.rs:603`) under `SessionKey{project: resolved, …}` —
  so session states for non-active projects are PTY-ownership-reconciled exactly like the active one.
- **No new events are emitted** for the overview. It is refreshed by existing events (§2).
- **Fixes the key vs resolved-name mismatch**: `ProjectSummary` (`host.rs:78`) gains
  `pub resolved_name: String`. `project_summary()` (`host.rs:96`) sets
  `resolved_name = pc.project.name.clone()` (and keeps `name = pc.project.name`, so
  `current_project` / `onboard_project` / `migrate_legacy` return `name == resolved_name`).
  `list_projects` keeps overwriting `summary.name = key` but leaves `resolved_name` untouched.
  Extend `project_summary_maps_fields` (`host.rs` tests) to assert `resolved_name`.
- Known cost (accepted for v1, see Risks): `reconciled_sessions` → `LocalStore::open` runs the full
  schema init on every call; with the 3 s poll that is one SQLite open + `effective_config` per
  project per 3 s.

### 2. Frontend data: `useProjectsOverview` + `buildGroups`

**IPC wrappers (exact names/types)**

- `src/lib/ipc.ts`, next to `setActiveProject` (`ipc.ts:792`):
  - `ProjectSummary` gains `resolved_name: string; // [project].name; the session/activity namespace`.
  - `export function reorderProjects(order: string[]): Promise<void> { return invoke("reorder_projects", { order }); }`
- `src/lib/projectsOverview.ts` (new). The wrapper lives here, not in `ipc.ts`, to avoid an
  `ipc.ts ↔ sessions.ts` import cycle (`WorktreeSession` is defined in `sessions.ts`, which imports
  from `ipc.ts`); this mirrors how `listSessions` lives in `sessions.ts`:
  ```ts
  export interface ProjectOverview {
    key: string; resolved_name: string;
    worktrees: WorktreeInfo[];        // from ./ipc
    sessions: WorktreeSession[];      // from ./sessions
  }
  export function projectsOverview(): Promise<ProjectOverview[]>   // getTransport().invoke("projects_overview")
  export function useProjectsOverview(): ProjectOverview[] | null  // null until first load
  ```
- **Events / cadence of `useProjectsOverview`** (events grepped in `host.rs`/`transfer.rs`: the
  emitted names are `project://changed`, `worktrees://changed`, `tasks://changed`, `sessions://changed`):
  - Main subscription: `useLiveResource(projectsOverview, "sessions://changed", { intervalMs: 3_000, listen: liveListen })`
    — **identical to `useSessions`** (`sessions.ts:131`): 3 s because a session running background
    work emits no event, so the interval is what surfaces idle→busy. (`liveListen` from
    `./transport`.)
  - `useLiveResource` takes a single event name, so also add a `useEffect` in the hook that
    subscribes to `onWorktreesChanged` and `onProjectChanged` (`ipc.ts:768`, `ipc.ts:822`) and calls
    the resource's `reload()` through a 150 ms trailing debounce (these two often fire in a burst,
    e.g. `set_active_project` emits four events). Cancel-safe teardown: follow the
    `cancelled` / `un` pattern in `App.tsx` (`onProjectChanged` effect).
  - Not listened to: `tasks://changed` (no task data in the overview), `daemon://status`.
- Called once, in `App.tsx`, directly under `const registryProjects = useProjectList();`.

**`src/lib/railGroups.ts` (pure, vitest)**

```ts
export type DotState = "idle" | "busy" | "waiting" | "offline" | "working" | "terminated"; // = blockStateCls output

export interface LaneModel {
  name: string; path: string; cli: CliKind; status: WorktreeStatus; terminated: boolean; // terminated already && available
  sessionState: SessionState | undefined;
  attn: AttentionKind | undefined;                 // active group: from `attention`; non-active: "waiting" iff session waiting
  task: { id: number; title: string } | undefined; // active group only
  activity: WorktreeActivity | undefined;          // activity.get(`${resolvedName}/${name}`), only when available
  building: boolean;                               // activity?.build?.state === "running"
  working: boolean;                                // activity?.working === true
  blockState: DotState;                            // blockStateCls(attn, session, available, building, terminated)
}

export interface GroupModel {
  key: string;            // registry key
  resolvedName: string;
  summary: ProjectSummary;   // for handleSwitchProject's optimistic flip + root for Reveal
  isActive: boolean;
  loading: boolean;       // non-active/fallback group whose overview hasn't loaded (overview === null)
  lanes: LaneModel[];
  summaryStats: { live: number; waiting: number; busy: number; building: number; dots: DotState[];
                  focusedDot: number | null; overflow: number }; // focusedDot: index into dots (active group only); overflow: the +N

}

export interface BuildGroupsInput {
  projects: ProjectSummary[];                 // ordered, from useProjectList (keys)
  activeResolvedName: string | null;
  activeReady: boolean;                       // warm.active === activeResolvedName
  activeWorktrees: WorktreeInfo[];            // App's activeWorktrees
  sessions: Map<string, WorktreeSession>;     // active, cache-seeded (useSessions().sessions)
  attention: Map<string, AttentionKind>;
  activeTaskByWorktree: Map<string, { id: number; title: string }>;
  overview: ProjectOverview[] | null;
  activity: Map<string, WorktreeActivity>;    // keyed `${resolvedName}/${worktree}`
  liveCounts: Map<string, number>;            // countsByProject(daemonSessions), keyed resolvedName
  selected: string | null;                    // App's selected worktree (active project)
  maxDots: number;                            // 8
}
export function buildGroups(i: BuildGroupsInput): GroupModel[]   // same order as i.projects
```

- Active group (when `activeReady`): lanes from `activeWorktrees` + `sessions` + `attention` +
  `activeTaskByWorktree`. Overview entries are matched by `overview.find(o => o.key === p.name)`
  (key) for non-active groups; a project missing from the overview (skipped by the backend) gets
  `lanes: []`, and `loading = (overview === null)`.
- Non-active group: lanes from the overview entry; `sessionState` from its `sessions`; `attn` is
  `"waiting"` iff that session's state is `waiting` (no idle-from-turn latch for background
  projects — "finished" stays the OS notification's job, consistent with the rail's rule that only
  `waiting` is surfaced).
- `blockState` always comes from `blockStateCls` (moved to `src/lib/laneState.ts`, §3), so dot
  colours and lane blocks share one mapping.
- `summaryStats` definitions (same for both paths, derived from `lanes`):
  `dots` = `blockState` of every lane whose `status === "available"`, in lane order;
  `waiting` = count of lanes with `blockState === "waiting"`;
  `building` = count of `building`; `busy` = count of `working && !building`;
  `live` = `liveCounts.get(resolvedName) ?? 0` (daemon-session count; **not** derived from lanes).
- Counts reuse `countsByProject` (`src/lib/sessionCounts.ts`), computed once in App from
  `useDaemonSessions()` (`DaemonSessionHint.project` is the resolved name). App already has this as
  `projectSessionCounts` (`App.tsx:356`); it is now passed into `buildGroups` instead of `TopBar`.

### 3. Rail component structure

Split `src/components/Rail.tsx` (770 lines) along natural seams:

- `src/lib/laneState.ts` (new, pure): move `blockStateCls`, `blockAnimates` (`Rail.tsx:62`, `:84`),
  `worktreeMenuItems` and the `WorktreeMenuItem` type (`Rail.tsx:404-419`) here so `railGroups.ts`
  does not import a `.tsx` component. `git mv src/components/Rail.test.ts src/lib/laneState.test.ts`
  and fix its import (`from "./laneState"`); all existing assertions stay green unchanged.
- `Rail.tsx` — shell: label row (Projects · cycle hint · Add project), `.rail-lanes` scroll
  container (renders `RailProjects`), `DaemonStatusBadge`/`DaemonCard`/`OfflineCard` (unchanged,
  stay in this file), footer, and the **single** instances of `WorktreeMenu` and `ProjectMenu`.
  It also owns the expanded-set state (§4) and the remove-project / reveal handlers (§8).
- `rail/RailProjects.tsx` — `DndContext` + `SortableContext` + one `ProjectGroup` per `GroupModel` (§5).
- `rail/ProjectGroup.tsx` — sortable wrapper, header, collapsible body, sentinel (§7).
- `rail/Lane.tsx` — today's lane row (`Rail.tsx:613-731`, the `worktrees.map` body) extracted
  verbatim, driven by a `LaneModel`; `Marquee` (`:283`), `StageChip` (`:343`), `LaneBuild`
  (`:357`) and the label constants (`STATUS_LABEL`, `WAITING_LABEL`, `TERMINATED_LABEL`,
  `Rail.tsx:39-52`) move with it. Props: `lane: LaneModel`, `isSelected: boolean`,
  `hintBadge?: number` (Cmd+N badge — active group only, built from the active group's available
  lanes in order, same as today's `badgeNums`), `hintVisible: boolean`, `onSelect()`, `onOpenDiff()`,
  `onContextMenu(x: number, y: number)`.
- `WorktreeMenu` (`Rail.tsx:424-546`) stays in `Rail.tsx` (single instance). Its state type gains
  `projectKey: string; inactive: boolean`.
- Order of work: extract `Lane.tsx`/`laneState.ts` **verbatim first, no visual change, own commit**.

**New `Rail` props** (replaces `worktrees`, `sessions`, `activity`, `activeProjectName`,
`attention`, `activeTaskByWorktree`, `selected`, `onSelect`, `onOpenDiff`, `onCreate`):

```ts
groups: GroupModel[];
selected: string | null;                                  // active project's selected worktree
onSelectLane: (g: GroupModel, worktree: string) => void;  // App: active → select+focus; else switch
onOpenDiff:   (g: GroupModel, worktree: string) => void;
onLaneAction: (g: GroupModel, worktree: string, action: PendingAction) => void; // §6
onNewWorktree: (g: GroupModel) => void;
onReorder:    (keys: string[]) => Promise<void>;          // App: reorderProjects(keys)
onAddProject: () => void;
// unchanged: onOpenSettings, onOpenHelp, daemonInfo, hintVisible, codexAvailable,
//            onRestartSession, onTransfer, onTerminateSession, onLaunchSession  (active-lane menu path)
```

### 4. Open/closed state

- `src/lib/railExpanded.ts` (pure reducer, vitest). State is `ReadonlySet<string>` of **project
  keys**; functions are immutable and return a new set: `toggle(s, key)`, `expandOnly(s, key)`,
  `expandAll(s, keys)`, `collapseOthers(s, key)` (→ `{key}`), `ensureExpanded(s, key)`,
  `prune(s, knownKeys)`.
- Persistence — new `useLocalStorageJson` in `src/lib/useLocalStorageState.ts` (that module has the
  boolean `useLocalStorageState` and number `useLocalStorageNumber`; no JSON variant exists):
  ```ts
  // pure + exported for tests (mirrors clampStoredNumber):
  export function parseStoredJson<T>(raw: string | null, fallback: T, validate: (v: unknown) => v is T): T;
  export function useLocalStorageJson<T>(
    key: string, initial: T, validate: (v: unknown) => v is T,
  ): [T, Dispatch<SetStateAction<T>>];
  ```
  Same contract as `useLocalStorageState`: read once on init in `try/catch` (parse failure or
  failed `validate` → `initial`), write via `JSON.stringify` on every change except the mount
  render, silent on storage errors. No cross-instance broadcast (single consumer).
- Key `keitora:railExpanded`, stored value `string[] | null` (`null` = never stored =
  first run), `validate = (v): v is string[] | null => v === null || (Array.isArray(v) && v.every(x => typeof x === "string"))`.
  The Rail shell derives `expanded = useMemo(() => new Set(stored ?? []), [stored])` and writes
  back `[...nextSet]`.
- Rules (implemented as effects in the Rail shell, keyed on `groups.find(g => g.isActive)?.key`
  = `activeKey`, and `projects` keys):
  - First run (`stored === null`): once `activeKey` is known, set stored to `[activeKey]`.
  - The active project is **auto-expanded when it changes** (keyboard cycle, Settings switch, lane
    click, add-project): keep `prevActiveKeyRef`, initialised to the *first non-null* `activeKey`
    seen (so a reload does **not** re-open a group the user collapsed — Decision 4); on a later
    change call `ensureExpanded(activeKey)`. The user can still collapse it afterwards.
  - Removed projects are pruned: `prune(known)` — only when `groups.length > 0` (`useProjectList`
    returns `[]` before its first load; pruning then would wipe the saved set).

### 5. Ordering + drag to reorder

**Backend**
- `UserConfig` (`crates/keitora-tasks/src/config.rs:234`) gains
  `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub project_order: Vec<String>`,
  declared **immediately after `active`** (before the `claude`/`codex`/… tables) so TOML emits it
  as a plain value ahead of any table. `projects` stays a `BTreeMap` (no migration).
- `impl UserConfig` (next to `set_active`, `config.rs:915`):
  - `pub fn ordered_project_names(&self) -> Vec<String>` — `project_order` entries that still exist
    in `projects` (first occurrence wins; duplicates and stale entries ignored), then every
    remaining key alphabetically. Empty `project_order` ⇒ pure alphabetical (today's behaviour).
  - `pub fn set_project_order(&mut self, order: Vec<String>) -> Result<(), String>` — pure
    validation: `order` must be a permutation of the current keys (same length, no duplicates, no
    unknown, none missing) else `Err` with a message naming the problem; on success stores it.
- `list_projects` (`host.rs:121`) and `projects_overview` iterate
  `user_cfg.ordered_project_names()` instead of `&user_cfg.projects` → the keymap's `cycleProject`
  (`src/lib/keybindings.ts:158`) automatically follows the rail order (single source preserved:
  `App.tsx` builds `projectNames` from `registryProjects`).
- New command in `host.rs`, registered after `set_active_project,`:
  `#[tauri::command(async)] fn reorder_projects(app: AppHandle, order: Vec<String>) -> Result<(), String>`
  = `UserConfig::load()` → `set_project_order(order)` → `save()` → `app.emit("project://changed", ())`.
- A project registered after a custom order exists is appended at the end of the explicit order
  (alphabetically among not-yet-ordered keys); with no custom order it stays alphabetical.
- Rust tests: see Testing.

**Frontend** (`rail/RailProjects.tsx`, `rail/ProjectGroup.tsx`)
- `@dnd-kit/sortable` (+ `@dnd-kit/core`, `@dnd-kit/utilities`; no `@dnd-kit/modifiers` is installed —
  do not add it) with `verticalListSortingStrategy`. Reuse the pattern in
  `src/components/DispatchStepsEditor.tsx:112-144` (the Board uses `@dnd-kit/core` droppables only):
  ```ts
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),  // clicks still toggle
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  <DndContext sensors={sensors} collisionDetection={closestCenter}
              measuring={{ droppable: { strategy: MeasuringStrategy.Always } }}
              onDragStart={…} onDragEnd={…} onDragCancel={…}>
  <SortableContext items={keys} strategy={verticalListSortingStrategy}>
  ```
  `items`/ids are project **keys**. `onDragEnd` → `arrayMove(keys, from, to)` → `onReorder`.
- **Pointer vs keyboard handles (resolves the Space/Enter conflict).** `useSortable` returns
  `attributes`, `listeners` (pointer *and* keyboard), `setActivatorNodeRef`. Spread `listeners` on the
  header row (`.rail-group-head`) so the **whole header is the pointer drag handle** (4 px
  threshold). Set `setActivatorNodeRef` + spread `attributes` on a dedicated `.rail-group-grip`
  button (lucide `GripVertical`, 12 px, shown on header hover/focus-within, in the header actions).
  dnd-kit's keyboard activator ignores key presses whose `event.target !== activatorNode`, so
  Enter/Space on the toggle button still just toggles, while Space on the focused grip starts a
  keyboard drag (arrows move, Space drops, Esc cancels). The grip has `aria-label="Reorder <key>"`.
- Transform style: `CSS.Transform.toString(transform && { ...transform, x: 0, scaleX: 1, scaleY: 1 })`
  (vertical only); `transition` from `useSortable({ id, transition: reduceMotion ? null : { duration: 160, easing: "cubic-bezier(.2,.8,.2,1)" } })`
  with `reduceMotion = usePrefersReducedMotion()` (`src/lib/usePrefersReducedMotion.ts`).
- While dragging, `RailProjects` adds `.dragging` to `.rail-projects`; CSS collapses every group
  body to 0 height with no transition so drop targets are header-sized. `MeasuringStrategy.Always`
  re-measures droppables after that collapse.
- Optimistic reorder: `RailProjects` holds `orderOverride: string[] | null`. On drop set it and
  call `onReorder(keys)`; on success leave it until the `groups` prop's key order equals it (effect
  clears it); on rejection clear it and `showToast(String(e))`.

### 6. Selecting across projects

**Signature changes**

```ts
// src/App.tsx (handleSwitchProject, today App.tsx:621)
type SwitchOpts = {
  select?: string | null;       // worktree to focus on arrival (implies view = "focus")
  silent?: boolean;             // skip the "Switched to X." toast
  action?: PendingAction | null;// run after the switch settles (see below)
};
handleSwitchProject(name: string /* registry KEY */, summary?: ProjectSummary, opts?: SwitchOpts): Promise<void>

// src/lib/switchProject.ts — add one optional field to the opts bag:
opts: { setProject; currentProject; showToast; summary?; silent?: boolean }
//   when `silent`, skip `showToast("Switched to …")` (the error toast is unchanged)
```
`SettingsDrawer`'s prop type `onSwitchProject: (name, summary?) => Promise<void>`
(`SettingsDrawer.tsx:257`) stays compatible (opts is a third, optional arg). `switchToProject`
(`App.tsx:682`, the keymap wrapper) is unchanged.

**handleSwitchProject changes (resolved-name aware — see "Naming and keys")**
1. `const s = summary ?? registryProjectsRef.current.find(p => p.name === name);`
   (`registryProjectsRef` = a ref mirroring `registryProjects`, like `warmRef`; Settings' degraded
   path thereby also gets the correct resolved name / optimistic flip.) `const resolved = s?.resolved_name ?? name;`
2. "Already active" check: `typeof project === "object" && project?.name === resolved`. If already
   active **and** `opts` present: apply `opts.select`/focus immediately (`setSelected`, `setView("focus")`),
   run `opts.action` immediately via `runPendingAction`, and return — no switch.
3. Warm lookup / flip uses `resolved`: `isWarm = resolved in prevWarm.projects`,
   `setWarm(w => setActive(w, resolved))`. If `opts.select` is set, **replace** the remembered
   focus restore with `setSelected(opts.select); setView("focus")` (otherwise restore as today).
4. `setSwitchingTo(resolved)` (the overlay's `switchingTo in warm.projects` check is keyed by
   resolved name). The 4 s safety-net timeout compares against the same string.
5. Optimistic flip: `switchProject(name /* key */, { setProject, currentProject: project, showToast, summary: s && { ...s, name: s.resolved_name }, silent })`.
   The backend call still uses the **key**.
6. The pending slot (below) is **cleared at the start of every `handleSwitchProject` call**, then set
   again if `opts.action`/`opts.select` need a post-settle step; on `!ok` it is cleared.
7. Cold targets (`!isWarm`): `selected` cannot be set early — until the target is warm,
   `activeWorktrees` is still the *previous* project's list and the "keep selected valid" effect
   (`App.tsx:805`) would reset it. So for cold targets the focus is applied only by the pending
   slot below.

**Pending-intent slot (where it lives, its type, and when it fires)**
- New pure module `src/lib/pendingIntent.ts` (vitest):
  ```ts
  export type PendingAction =
    | { kind: "restart"; fresh: boolean }
    | { kind: "transfer"; to: CliKind }
    | { kind: "terminate" }
    | { kind: "launch"; fresh: boolean }
    | { kind: "diff" }
    | { kind: "new-worktree" };
  export interface PendingIntent {
    project: string;            // RESOLVED name of the switch target
    worktree: string | null;    // lane to focus; null for "new-worktree"/plain project switches
    action: PendingAction | null;
  }
  export interface SettleState {
    activeProjectName: string | null; switchingTo: string | null;
    warmActive: string | null; warmHas: boolean;   // warm.projects has intent.project
  }
  export function canConsume(i: PendingIntent, s: SettleState): boolean;
    // activeProjectName === i.project && switchingTo === null && warmActive === i.project && warmHas
  export function shouldDrop(i: PendingIntent, s: SettleState): boolean;
    // switchingTo === null && activeProjectName !== i.project   (the switch ended elsewhere/failed)
  ```
- State: `const [pending, setPending] = useState<PendingIntent | null>(null);` in `AppInner`, declared
  next to `switchingTo` (`App.tsx:268`). It is state (not a ref) so the consuming effect re-runs.
- **What "switch settled" means in this codebase (verified):** `handleSwitchProject` sets
  `switchingTo = resolved`, awaits `switchProject()` (the `set_active_project` round trip), then
  `await Promise.all([loadProject(), reloadWorktrees(), reloadSessions()])`, and only then clears
  `switchingTo` (`finally`). After that the existing "settled-warm" effect (`App.tsx:735`) seeds
  `warm.projects[resolved]` and sets `warm.active`, which mounts the project's `TerminalPane`s.
  Settled ⇔ `canConsume(...)`: `activeProjectName === target` (note `project` flips *optimistically*,
  so this alone is not enough) **and** `switchingTo === null` **and** `warm.active === target` **and**
  `target in warm.projects`. This is the only safe point because the lane actions
  (`transferWorktree`, `injectHandoffSummary`, panes' spawn/kill) resolve the **backend's** active
  project, and the target pane must be mounted for a nonce bump to reach it.
- Consuming effect (declared **after** the "keep selected valid" and "persist focus" effects in
  `AppInner`, **before** the early `return`s — all hooks stay above the `if (project === "loading")`
  returns): when `pending && canConsume` → `setSelected(pending.worktree)` + `setView("focus")` when
  `worktree !== null`, then `runPendingAction(action, worktree)`, then `setPending(null)`. When
  `shouldDrop` → `setPending(null)` (the switch-failure toast already told the user).
  Declaration order matters: the later functional `setSelected` wins over the earlier effect's reset.
- `runPendingAction(action, worktree)` is a small `useCallback` in `AppInner` mapping to the existing
  handlers: `restart → onRestartSession(worktree, { fresh })`, `transfer → onTransfer(worktree, to)`,
  `terminate → onTerminateSession(worktree)`, `launch → onLaunchSession(worktree, { fresh })`,
  `diff → setDiffTarget(worktree)`, `new-worktree → setWtOpen(true)`. (The existing handlers read
  `warmRef.current.active`, which is the target by then.)
- `diffTarget` is cleared by an effect on `activeProjectName` change (`App.tsx:278`); because the
  `diff` action runs at settle (after that flip), it is not wiped.

**Click paths**
- Lane click in the **active** group → `App.onSelectLane`: `setSelected(w); setView("focus")` (unchanged).
- Lane click in a **non-active** group → `handleSwitchProject(g.key, g.summary, { select: w, silent: true })`.
  Cold target: the existing switch overlay still covers the load.
- Diff button on a non-active lane: `handleSwitchProject(g.key, g.summary, { select: w, silent: true, action: { kind: "diff" } })`.
  On an active lane: `setDiffTarget(w)` as today.
- **Right-click on a non-active lane** shows the same worktree menu as an active lane, with a
  leading **Focus** item and a divider:
  - live session: Focus · Restart session · Start fresh conversation · Transfer to … · Terminate session;
  - terminated: Focus · Launch · Launch fresh.
  `worktreeMenuItems(terminated: boolean, opts?: { inactive?: boolean }): WorktreeMenuItem[]` (in
  `laneState.ts`) returns `["focus", …existing]` when `inactive`; `WorktreeMenuItem` gains `"focus"`.
  Menu callbacks: an **active** lane's items call the existing `onRestartSession` / `onTransfer` /
  `onTerminateSession` / `onLaunchSession` props directly (unchanged); an **inactive** lane's items
  call `onLaneAction(g, w, action)` with the matching `PendingAction` (`Focus` = `onSelectLane`).
  `App.handleLaneAction` = `g.isActive ? runPendingAction(a, w) : handleSwitchProject(g.key, g.summary, { select: w, silent: true, action: a })`.
- Transfer to Codex target availability uses the same `codexAvailable` flag; the target CLI for the
  menu label comes from `LaneModel.cli` (opposite CLI), same as today.

### 7. Visual design

Design tokens only (`src/index.css` `@theme` + the `App.css` `:root` mirror — keep both in
lockstep if a token is ever added; this change adds none); no new colours except where noted. Both
themes must look right. Rail width stays `--rail-width: 272px`. The rail background is `--bg-1`,
lane cards are `--bg-2` (hover `--bg-3`).

**Rail top**

```
 PROJECTS                         ⌘ ← →   +
```
- `rail-label` "Projects" (replaces "Worktrees"); the `+` (`.rail-add-btn`) becomes **Add project**
  (`title`/`aria-label` "Add project", calls `onAddProject`; worktree creation moves to each group).
  The `⌘ ← →` cycle hint (`<span className="rail-cycle-hint">{modKeyLabel} ← →</span>`,
  `modKeyLabel` from `src/lib/platform.ts`, shown when `hintVisible`) is rendered in `.rail-label-row`
  immediately before the `+` button (moved from the tab strip; styles = today's
  `.project-cycle-hint`, renamed).

**Group header (collapsed)** — 34px tall, full rail width, radius-chip, hover `bg-2`:

```
 ›  shared-tools          ● ● ◍ ●  ·  2      ⋯
 ›  my-game           ◉     ● ● ● ● ● ○ ○ +2  7
```
- Chevron (lucide `ChevronRight`, 12px, `tx-3`) rotates 90° on open (120ms ease-out).
- Name: 12.5px, `tx-2`, weight 500, truncates with ellipsis.
- **Dot strip**: one 6px dot per available worktree using the lane status palette
  (idle/busy/waiting/offline/working/terminated), 3px gap, max 8 then a `+N` in `tx-3`.
  Hidden when the group is expanded (the lanes already show it) so the header stays quiet.
  **Focused-worktree dot:** in the active project's collapsed strip, the selected worktree's dot is
  drawn 8px with a 1.5px `accent` ring (2px offset) so "you are here" survives collapsing; it is
  never moved into the `+N` overflow (if it would overflow, it takes the last visible slot).
- **Needs-input**: when `waiting > 0`, a `waiting`-blue ring dot `◉` sits right after the name and
  pulses (same `blockPulse` keyframe as lanes; static glow under reduced motion). The header also
  gets a 2px `waiting` left edge. This is the "go here" signal and must be visible collapsed.
- **Busy/building hint**: the existing `lane-work-dot` (amber busy / teal building) to the left of
  the count when `busy + building > 0` (`.working` modifier when `building > 0`, else `.busy`).
- **Count**: live-session count in tabular figures, `tx-3`, right-aligned; omitted at 0.
- `⋯` menu button appears on hover/focus only (also reachable by right-click).

**Group header (expanded)** — same row; dot strip hidden; count stays; a hairline (`line`) under
the header separates it from the lanes. A per-group `+` (New worktree) appears on hover next to `⋯`.

**Active project header: a real "selected" state.** The project you're *in* looks selected, the way
a selected tab does, whether its group is open or collapsed. The user can collapse it; it still reads
as selected:
- the header background is a persistent `accent` tint (about 10% alpha over `bg-1`), with a 1px
  `accent` border at about 35% alpha, at radius-chip. It is filled, not just a hover state;
- a 3px `accent` pill marker at the far left of the header (inset 6px vertically), echoing the lane
  focus bar below it;
- the name is `tx-1`, weight 600, and the chevron is `accent` instead of `tx-3`;
- hover on the selected header deepens the tint slightly; it never falls back to the plain `bg-2`
  hover;
- `aria-current="true"` on the header button, for assistive tech;
- when expanded, the group body gets a very faint `accent` wash (about 4% alpha) behind its lanes, so
  the active project's region reads as one unit among several open groups.
Collapsing the active group keeps all of the header treatment. The dot strip comes back, since the
group is collapsed. The top-bar breadcrumb still names the project and worktree.

**Lanes** — today's lane design, indented 10px under the header so hierarchy reads, with one
upgrade:

**Focused lane** (active project ∧ selected worktree) — today it is only a card outline (`sel`),
which gets lost with multiple groups open. New treatment:
- 3px `accent` focus bar on the lane's left edge (inside the card, full height, radius 2),
- background `bg-3` + the existing outline in `accent` at ~55% alpha (**clarification:** today's
  lane card is already `bg-2` and hover is `bg-3`, so "lifted vs. others" means `bg-3` for the
  focused lane; hover on other lanes stays as is and has no focus bar/outline, which keeps them
  distinguishable),
- lane name in `tx-1` weight 600,
- the selected lane in a **non-active** group can't exist (selection is per-active-project), so the
  bar is a single, unique "you are here" marker in the whole rail.
- On selection change the rail scrolls the focused lane into view (`block: "nearest"`, smooth unless
  reduced motion).

**Sticky headers** — inside `.rail-lanes` (the scroll container), group headers are
`position: sticky; top: 0` with a `bg-0` background + soft bottom shadow when stuck, so with long
open groups you always know which project you're scrolling through.

**Expand/collapse motion** — CSS grid row trick (`grid-template-rows: 0fr → 1fr`, 160ms
`cubic-bezier(.2,.8,.2,1)`) on the body wrapper; lanes fade from 0→1 over the same window. No
height measuring in JS. `@media (prefers-reduced-motion: reduce)`: instant.

**Drag** — the lifted header gets `bg-3`, a 0 8px 24px shadow (a lift, no rotation); siblings animate into place via dnd-kit transforms (disabled under reduced motion).

**Top bar** — `brand` left, view pills right (unchanged), centre: breadcrumb
`my-game / alpha` in 12px `tx-3` with the worktree in `tx-2` (plain text, not interactive in
v1). **Slimmer height:** target 36px (from 44px). Scale the brand glyph down to fit (≈28px) and
drop the `muted` "logistics dispatch" tagline if it crowds; view pills shrink to match. Validate with a
screenshot in Testing; fall back to 40px only if the glyph reads poorly at 28px.

**Implementation specifics for §7 (concrete class names and mechanics — all in `src/App.css`,
in a new `/* ---- rail project groups ---- */` section placed where the old
`/* ---- project tabs (desktop) ---- */` block (`App.css:1277-1337`) was)**

Element structure and classes:

```
.rail-lanes                       (existing scroll container; gains scroll-padding-top: 34px)
  .rail-projects                  (+ .dragging while a drag is active)
    .rail-group                   (sortable wrapper; modifiers .open .active .has-waiting .is-dragging; NO overflow set — it must not break sticky)
      .rail-group-sentinel        (1px, height 1px, margin-bottom -1px; observed for "stuck")
      .rail-group-head            (div; data-stuck="true|false"; sticky; 34px; the pointer-drag handle)
        button.rail-group-toggle  (aria-expanded, aria-controls=<body id>, aria-current="true" when active; contains:)
          svg.rail-group-chev
          span.rail-group-name
          span.rail-group-wait    (the ◉ ring dot; + .pulse unless reduced motion; --block: var(--waiting))
          span.rail-dots          (collapsed only; children span.rail-dot.<DotState>; plus span.rail-dots-more "+N")
          span.lane-work-dot      (reuse existing classes: .working | .busy, .live unless reduced motion; shown when busy+building>0)
          span.rail-group-count   (tabular-nums)
        span.rail-group-actions   (opacity 0 → 1 on .rail-group-head:hover / :focus-within)
          button.rail-group-grip  (GripVertical; dnd activator for keyboard)
          button.rail-group-act.rail-group-add   (＋ New worktree)
          button.rail-group-act.rail-group-more  (⋯ opens ProjectMenu)
      .rail-group-body            (id=<body id>; role="group"; display:grid; grid-template-rows:0fr)
        .rail-group-body-inner    (min-height:0; overflow:hidden; padding: 4px 4px 0 10px = the 10px indent + room for lane glow)
          Lane rows…  or  .rail-group-empty (muted text: "Loading…" when loading, else "No worktrees")
```

- Header buttons are siblings, never nested (`<button>` inside `<button>` is invalid): the toggle
  button fills the row (`flex: 1; min-width: 0`), `.rail-group-actions` sits to its right.
- Open: `.rail-group.open .rail-group-body { grid-template-rows: 1fr }`;
  `transition: grid-template-rows 160ms cubic-bezier(.2,.8,.2,1)` on the body and
  `opacity` 0→1 on `.rail-group-body-inner` over the same 160 ms. Collapsed state also sets
  `visibility: hidden` (with `transition: visibility 0s 160ms`) so collapsed lanes are out of the
  tab order and the accessibility tree. `@media (prefers-reduced-motion: reduce)` sets all of these
  `transition: none`. `.rail-projects.dragging .rail-group-body { grid-template-rows: 0fr; transition: none }`.
- Header colours (opaque so sticky never shows lanes through it): base
  `background: var(--bg-1)` (equals the rail), hover `var(--bg-2)`.
  Active: `background: color-mix(in srgb, var(--accent) 10%, var(--bg-1))`,
  `border: 1px solid color-mix(in srgb, var(--accent) 35%, transparent)`, hover 16% mix; name
  `var(--tx-1)` weight 600; chevron `var(--accent-text)`. Pill: `.rail-group.active .rail-group-head::before`
  (`position:absolute; left:3px; top:6px; bottom:6px; width:3px; border-radius:2px; background:var(--accent)`).
  Waiting edge: `.rail-group.has-waiting .rail-group-head { box-shadow: inset 2px 0 0 var(--waiting) }`
  (at x=0..2px; the pill sits at left:3px so both are visible when a group is active *and* waiting).
  Open+active body wash: `.rail-group.active.open .rail-group-body { background: color-mix(in srgb, var(--accent) 4%, transparent) }`.
- Stuck state: `ProjectGroup` observes its `.rail-group-sentinel` with an `IntersectionObserver`
  (`root` = the `.rail-lanes` element, threshold 0); when the sentinel is out of view *above* the
  root and the group is open, set `data-stuck="true"` on the head → `background: var(--bg-0)` and
  `box-shadow: 0 6px 8px -6px rgba(0,0,0,.5)` (use `rgba(15,30,25,.18)` under
  `html[data-theme="light"]`). (This is the one place the doc's "bg-0 + soft shadow when stuck" is
  implemented; unstuck headers stay on `--bg-1` so the list doesn't have dark bands.) Sticky works
  because the only scroll container between header and viewport is `.rail-lanes`; `.rail-group`
  and `.rail-projects` must not set `overflow`, `contain`, or `transform` (dnd-kit's transform is
  applied only while that group is being dragged). Header `z-index: 2`; lane rows are `position:
  relative` with auto z-index, so the header paints over them.
- `scroll-padding-top: 34px` on `.rail-lanes` makes `scrollIntoView({ block: "nearest" })` for the
  focused lane clear the sticky header. The scroll-into-view effect lives in `Rail.tsx`: on
  `selected`/active-key change, `railRef.current?.querySelector(".lane-row.sel")?.scrollIntoView({ block: "nearest", behavior: reduceMotion ? "auto" : "smooth" })`.
  If the active group is collapsed there is no `.sel` element in view (hidden) — then do nothing.
- Dots: `.rail-dot { width:6px; height:6px; border-radius:50%; background: var(--block, var(--offline)) }`
  with `.idle/.busy/.waiting/.offline/.working/.terminated` setting `--block` exactly like the
  `.wt-block.*` rules (`App.css:321-346`; `terminated` = offline colour at `opacity:.45`, `working` = `var(--accent-2)`).
  Gap 3px; max 8 dots then `.rail-dots-more` "+N" in `--tx-3`.
- Wait dot: `.rail-group-wait { width:8px; height:8px; border-radius:50%; background:var(--waiting); --block: var(--waiting); box-shadow: 0 0 0 2px color-mix(in srgb, var(--waiting) 35%, transparent), 0 0 10px 0 var(--block) }`
  (static glow = the reduced-motion baseline); `.pulse` adds `animation: blockPulse 1.6s ease-in-out infinite`
  (existing keyframe, `App.css:~352`), class added only when `!reduceMotion` (same pattern as `.wt-block.pulse`).
- Focus treatment CSS (extends the existing `.lane` rules, `App.css:259-312`): `.lane { position: relative }`;
  `.lane-row.sel .lane { background: var(--bg-3); border-color: color-mix(in srgb, var(--accent) 55%, transparent) }`;
  `.lane-row.sel .lane::before { content:""; position:absolute; left:0; top:0; bottom:0; width:3px; border-radius:2px; background:var(--accent) }`;
  `.lane-row.sel .lane-name { color: var(--tx-1); font-weight: 600 }`. Only the active group's lanes
  ever receive `sel` (`isSelected = g.isActive && lane.name === selected`).
- Drag: `.rail-group.is-dragging .rail-group-head { background: var(--bg-3); box-shadow: 0 8px 24px rgba(0,0,0,.45); z-index: 3; position: relative }`.
- Breadcrumb (TopBar): new `.topbar-crumb { position:absolute; left:50%; transform:translateX(-50%); font-size:12px; color:var(--tx-3); max-width:36vw; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; pointer-events:none }`
  with `.crumb-sep` (margin 0 6px) and `.crumb-wt { color: var(--tx-2) }`. Delete the old `.active-project`
  rule (`App.css:121-129`; its only user was TopBar's fallback span).

### 8. Context menu (project header)

Radix DropdownMenu with the 0×0 anchor pattern already used by `WorktreeMenu`
(`src/components/Rail.tsx:424-546`; shared `menuItemCls`, `Rail.tsx:421`), opened by right-click on
the header or the `⋯` button (the button opens it at its bounding-rect bottom-left). `ProjectMenu`
is a second, sibling component in `Rail.tsx` with state `{ key: string; x: number; y: number } | null`
(single instance in the Rail shell; the group looked up from `groups` by key).

- **New worktree…** — `onNewWorktree(g)`: active → `setWtOpen(true)` (existing
  `CreateWorktreeDialog` mount in `App.tsx:952`; its `onSubmit` calls `createWorktree(name)`,
  `worktree_create` targets the **backend's active** project, `src-tauri/src/host.rs:377`); non-active →
  `handleSwitchProject(g.key, g.summary, { silent: true, action: { kind: "new-worktree" } })` so the
  dialog opens only once the switch has settled. The per-group hover `＋` is the same action.
- **Reveal in file explorer** opens the project root (`g.summary.root`). It uses
  `tauri-plugin-opener`, already a dependency (`src-tauri/Cargo.toml:23`, registered at
  `host.rs:1858`; `package.json:39` `@tauri-apps/plugin-opener` 2.5.4).
  **Decision: `revealItemInDir(root)`, no capability change.** Verified in the plugin:
  `opener:default` (already in `src-tauri/capabilities/default.json`) is the set
  `allow-open-url` + `allow-reveal-item-in-dir` + `allow-default-urls`, and
  `allow-reveal-item-in-dir` carries no path scope. By contrast `opener:allow-open-path` is *not* in
  `opener:default` and the capability scopes it to one fixed log file
  (`$HOME/.keitora/herdr/config/herdr/herdr-server.log`), so `openPath(root)` would be **denied** for
  arbitrary project roots — do not use it. Import dynamically like `SettingsDrawer.tsx:102`:
  `const { revealItemInDir } = await import("@tauri-apps/plugin-opener"); await revealItemInDir(root);`
  with `catch → showToast(String(e))`. (For a directory path the OS file manager opens the parent
  with the project folder selected — acceptable.)
- **Collapse others** (`collapseOthers(key)`) / **Expand all** (`expandAll(keys)`).
- **Remove project…** — `useConfirmDialog().confirmDialog({ title: "Remove " + key + "?", body: "It is removed from Keitora's registry only. Files on disk are not touched.", confirmLabel: "Remove", danger: true })`
  (`src/components/ConfirmDialog.tsx`), then `removeProject(key)` (`src/lib/settings.ts:108`, wraps
  `remove_project`, `host.rs:750`) and `showToast("Removed " + key + " from the registry.")`; errors →
  `showToast(String(e))`. **Disabled for the active project** (`g.isActive`) with
  `title="Switch to another project first"` — the same rule/tooltip Settings uses
  (`SettingsDrawer.tsx:802-812`; Settings' own `doRemove` is at `:439`). The list refresh and
  expanded-set prune follow from `project://changed` (emitted by `remove_project`). Rail already
  uses `useToast`; add `useConfirmDialog` there (providers wrap `AppInner`, `App.tsx:1081`).

### 9. Cleanup

- Delete `src/components/ProjectTabs.tsx` and the `.project-tab*` / `.project-tabs` /
  `.project-cycle-hint` CSS (`src/App.css:1277-1337`).
- `TopBar` props shrink to (call-site: **only** `<TopBar … />` in `App.tsx:902`; no tests render it):
  ```ts
  TopBar({ view, onView, projectLabel, worktreeLabel }: {
    view: View; onView: (v: View) => void;
    projectLabel?: string;              // registry KEY of the active project (App: activeProjectKey)
    worktreeLabel?: string | null;      // App's `selected`
  })
  ```
  Removed: `projectName`, `projects`, `onSwitchProject`, `sessionCounts`, `onAddProject`,
  `hintVisible`. Render: brand · (if `projectLabel`) `<div className="topbar-crumb">` project
  `<span className="crumb-sep">/</span>` `<span className="crumb-wt">{worktreeLabel}</span>` (the
  separator + worktree only when `worktreeLabel` is non-null) · views. Remove the `ProjectSummary` /
  `ProjectTabs` imports. `View` stays exported from `TopBar.tsx` (imported by `keybindings.ts`,
  `attention.ts`, `paneVisibility.ts`, `warmProjects.ts`, `warmProjects.test.ts`). Update the file's
  header comment.
- `App.tsx` call-site changes: `<TopBar view onView projectLabel={activeProjectKey} worktreeLabel={selected} />`
  where `activeProjectKey = registryProjects.find(p => p.resolved_name === activeProjectName)?.name ?? activeProjectName ?? ""`
  (memoised); `<Rail groups={groups} … />` per §3 with
  `groups = useMemo(() => buildGroups({ … }), [...])`; `KeyCtx.activeProjectName` is fed
  `activeProjectKey` (cycleProject compares against `projectNames`, which are **keys**; update the
  doc comment at `keybindings.ts:37`); `onAddProject` now goes to `<Rail>` instead of `<TopBar>`.
- Update stale comments that mention the tab strip / ProjectTabs / project popover:
  `src/App.tsx:149-150`, `:157-158`, `:355`, `:616`, `:950`; `src/components/SettingsDrawer.tsx:819`;
  `src/lib/switchProject.ts:1-2, 8`; `src/lib/useProjectList.ts:2-5`; `src/App.css:984-985`
  ("project popover" → drop the popover reference).
- `docs/build-plan.md`: Layout section (`:102-122`, the ASCII diagram + Rail bullet at `:119`) and
  the #0002 note (`:445-454`, "tab strip (`ProjectTabs`)" and "cycle hint on the tabs") — tabs →
  rail project groups.

## File change map

**Create**

| File | Purpose |
|---|---|
| `src/lib/projectsOverview.ts` | `ProjectOverview` type, `projectsOverview()` wrapper, `useProjectsOverview()` hook (§2) |
| `src/lib/railGroups.ts` (+ `railGroups.test.ts`) | pure `buildGroups` → `GroupModel[]` (§2) |
| `src/lib/railExpanded.ts` (+ `railExpanded.test.ts`) | pure expanded-set reducer (§4) |
| `src/lib/pendingIntent.ts` (+ `pendingIntent.test.ts`) | `PendingAction`/`PendingIntent` types, `canConsume`, `shouldDrop` (§6) |
| `src/lib/laneState.ts` | `blockStateCls`, `blockAnimates`, `worktreeMenuItems` (+ `"focus"`), moved out of `Rail.tsx` (§3) |
| `src/components/rail/Lane.tsx` | extracted lane row + `Marquee`/`StageChip`/`LaneBuild` + label constants (§3) |
| `src/components/rail/ProjectGroup.tsx` | sortable group: sentinel, header, collapsible body (§5, §7) |
| `src/components/rail/RailProjects.tsx` | `DndContext`/`SortableContext`, optimistic order, drag state (§5) |

**Modify**

| File | Change |
|---|---|
| `crates/keitora-tasks/src/config.rs` | `UserConfig.project_order`, `ordered_project_names()`, `set_project_order()` + tests (§5) |
| `src-tauri/src/host.rs` | `ProjectSummary.resolved_name`; `ProjectOverview` + `project_overview` helper + `projects_overview` + `reorder_projects` commands; ordered iteration in `list_projects`; register in `generate_handler!`; tests (§1, §5) |
| `src/lib/ipc.ts` | `ProjectSummary.resolved_name`, `reorderProjects()` (§2) |
| `src/lib/switchProject.ts` | `silent` option; header comment (§6, §9) |
| `src/lib/switchProject.test.ts` | add `resolved_name` to fixtures; test `silent` skips the success toast |
| `src/lib/warmProjects.test.ts` | add `resolved_name` to `ProjectSummary` fixtures (`makeSummary`, `summaryA`) |
| `src/lib/useLocalStorageState.ts` (+ `useLocalStorageState.test.ts`) | `parseStoredJson`, `useLocalStorageJson` (§4) |
| `src/lib/keybindings.ts` | doc comment on `KeyCtx.activeProjectName` (now the registry key) (§9) |
| `src/lib/useProjectList.ts` | header comment only (§9) |
| `src/components/Rail.tsx` | shell: label row, `RailProjects`, `ProjectMenu`, `WorktreeMenu` `inactive`/`focus`, expanded-set effects, remove/reveal handlers, scroll-into-view; lane/helpers moved out (§3, §4, §8) |
| `src/components/TopBar.tsx` | remove `ProjectTabs`; breadcrumb; shrunken props (§9) |
| `src/components/SettingsDrawer.tsx` | comment at `:819` only |
| `src/App.tsx` | `useProjectsOverview`, `groups`, `activeProjectKey`, `pending` slot + consuming effect + `runPendingAction`, `handleSwitchProject(name, summary?, opts?)`, `registryProjectsRef`, `handleLaneAction`/`onSelectLane`, new `<TopBar>`/`<Rail>` props, comment fixes (§6, §9) |
| `src/App.css` | add rail-group/breadcrumb/focus rules; remove `.project-tab*`, `.project-cycle-hint`, `.active-project`; comment fix at `:984-985` (§7, §9) |
| `docs/build-plan.md` | Layout diagram + #0002 note → rail groups (§9) |

**Move**

| From → To | Why |
|---|---|
| `src/components/Rail.test.ts` → `src/lib/laneState.test.ts` | its subjects moved to `laneState.ts`; add `inactive` cases |

**Delete**

| File | Why |
|---|---|
| `src/components/ProjectTabs.tsx` | replaced by rail groups |

No change to `src-tauri/capabilities/default.json` (`opener:default` already allows
`reveal_item_in_dir`; see §8), `Cargo.toml`, or `package.json` (all deps exist).

## Testing

- Rust (`cargo test --workspace`; run by the implementer, not during this planning pass):
  - `UserConfig::ordered_project_names` (empty = alphabetical, stale entries ignored, new projects
    appended, duplicates ignored) and `set_project_order` (accepts a permutation; rejects unknown,
    missing, duplicate keys) in `crates/keitora-tasks/src/config.rs` tests; plus a TOML round-trip
    (`save_to` / `load_from` with a non-empty `project_order` and `[projects]`) to prove the
    serialized ordering is valid.
  - `project_overview` helper in `host.rs` tests, using `crate::backend::fake::FakeBackend::new()` and
    a `tempfile::TempDir` file-less project via `effective_config(&ProjectRef::new(root))` (pattern of
    `project_summary_from_fileless_effective_config`): `key`/`resolved_name` populated and distinct
    when the root basename differs from the key; worktrees come back with `terminated` set from the
    fake; sessions empty and **no DB file created** when `db_path` does not exist; sessions scoped
    to the project's configured worktrees and stamped with `resolved_name` when it does.
    (`resolved_worktrees` reads the real user config via `UserConfig::load()` for `default_cli` —
    read-only, as in the existing tests.) Broken config skipping is the loop's `continue`; cover it
    via a small pure `fn overview_entries(user: &UserConfig, …)` if the loop is factored out, else
    leave to manual smoke.
  - `project_summary_maps_fields` asserts `resolved_name`.
- Frontend (`yarn test`, node env, pure functions only — no component rendering exists in this repo):
  - `buildGroups`: active vs non-active sourcing, `activeReady=false` fallback to overview,
    waiting/busy/building/live counts, dot order and available-only filtering, resolved-name keying
    of `activity` and `liveCounts` (key ≠ resolved fixture), project missing from overview → empty
    lanes, `loading` flag, `maxDots` overflow (`+N`), and `focusedDot` (selected worktree marked;
    kept visible by taking the last slot when it would overflow; null for non-active groups).
  - `railExpanded` reducer: toggle, expandOnly, expandAll, collapseOthers, ensureExpanded, prune.
  - `parseStoredJson` parse/fallback/validate (mirrors `useLocalStorageState.test.ts`'s
    `clampStoredNumber` cases).
  - `worktreeMenuItems` with `inactive` (leading `focus`) and unchanged results without it
    (migrated `Rail.test.ts` cases).
  - `pendingIntent`: `canConsume` true only when target is active, `switchingTo` null, warm.active
    is target and warm has it; false in each single-missing case; `shouldDrop` when the switch ended
    with a different active project.
  - `switchProject.test.ts`: `silent` suppresses the "Switched to" toast but not the error toast.
  - Existing `keybindings.test.ts` and `warmProjects.test.ts` stay green.
- Manual smoke (isolated dev instance, never the user's live herdr; no heavy runs on a loaded
  machine): 8+ projects; one project whose key ≠ `[project].name`; open several groups; click a lane
  in a non-active group (warm and cold); drag reorder then Ctrl+←/→ follows; keyboard reorder via the
  grip; context menu actions incl. non-active lane Restart/Terminate/Launch; Reveal opens the file
  manager; waiting signal on a collapsed group; sticky headers while scrolling; light theme; reduced
  motion. Screenshots reviewed.
- Before committing: `git grep -nE '#[0-9]{3,4}' -- <changed source files>` must be empty (AGENTS.md:
  no task/plan identifiers in code or commit messages; `plans/` is exempt).

## Dependencies

- None blocking. Uses existing deps: `@dnd-kit/core`, `@dnd-kit/sortable`, `@dnd-kit/utilities`,
  `@radix-ui/react-dropdown-menu`, `lucide-react` (`ChevronRight`, `GripVertical`, `Plus`,
  `MoreHorizontal`), `@tauri-apps/plugin-opener`; `motion` not needed (CSS-only animation).

## Risks

- **Overview poll cost** with many projects: one SQLite open (full schema init via
  `LocalStore::open`) + `effective_config` per project every 3 s. Mitigate: event-driven refresh +
  3 s interval (same as sessions); skip projects without a DB; measure with ~10 projects. Could cache
  `effective_config` per project later.
- **Non-active session states are less rich** than the active project's (no idle-from-turn latch, no
  task chips). Accepted; called out in Scope/Out.
- **Rail height pressure**: many open groups push the daemon card/footer. They stay pinned outside the
  scroll container (today's structure), so only lanes scroll.
- **Drag vs click** conflicts on the header — 4 px activation distance; keyboard drag only from the
  grip (dnd-kit ignores key events from other targets); verify by hand.
- **Rail.tsx refactor** touches the lane markup the user has tuned over many iterations: extract
  verbatim first (no visual change, own commit), then layer the focus treatment.
- **Name mismatch fix** changes which string the frontend keys on; covered by `buildGroups` tests and
  must keep `handleSwitchProject`'s already-active check correct (it now compares resolved names).
- **Stale activity doc comment**: `activity/mod.rs:46` says `WorktreeActivity.project` is the
  "registry project key", but the code keys by the **resolved** name (`activity/mod.rs:126-129`). The
  frontend relies on the code, not the comment; fix the comment while there.

## Decisions (refine round 1)

1. A right-click on a non-active lane shows the full worktree menu, with a leading **Focus** item.
   Every action focuses that worktree first, then runs (§6).
2. The top-bar breadcrumb is plain text in v1, not clickable.
3. Keep **Reveal in file explorer**, using the opener plugin that is already installed (§8).
4. The active project's group can be collapsed. Its header keeps a persistent "selected" treatment
   either way (§7). It auto-expands only when the active project *changes*.

## Decisions (singletonize)

Choices made while removing ambiguity; none alters a refine-round decision or the §7 design intent.

1. Two project identifiers exist (key vs resolved name); rules in "Naming and keys". `ProjectSummary`
   gains `resolved_name`; `ProjectOverview` carries `key` + `resolved_name`.
2. `projects_overview` + `reorder_projects` live in `host.rs`, registered in
   `generate_handler!`; no new events. TS wrappers: `reorderProjects` in `ipc.ts`,
   `projectsOverview` in `projectsOverview.ts` (cycle avoidance).
3. Overview refresh = `sessions://changed` + 3 s interval (as `useSessions`) + debounced
   `worktrees://changed` / `project://changed`.
4. `revealItemInDir(root)`; the capability file is unchanged (`openPath` would be denied).
5. `useLocalStorageJson<T>(key, initial, validate)` + pure `parseStoredJson`; expanded set stored as
   `string[] | null` of project keys.
6. Pending-intent slot = `useState<PendingIntent | null>` in `AppInner`; consumed when
   `activeProjectName === target && !switchingTo && warm.active === target && target in warm.projects`;
   dropped on switch failure. It also carries the focus for cold targets and the `diff` /
   `new-worktree` actions.
7. Keyboard reorder only from the dedicated grip (dnd-kit activator-node rule); the whole header is
   the pointer handle; vertical-only transform; `MeasuringStrategy.Always`.
8. Sticky "stuck" styling via an `IntersectionObserver` sentinel; unstuck header background is
   `--bg-1` (the rail colour), stuck is `--bg-0` + shadow; `.rail-lanes` gets `scroll-padding-top: 34px`.
9. Focused lane background is `bg-3` (today's lane is `bg-2`, hover `bg-3`).
10. Top bar slims to a 36px target (glyph ≈28px); breadcrumb shows the registry key + `selected` worktree.
11. `blockStateCls`/`blockAnimates`/`worktreeMenuItems` move to `src/lib/laneState.ts`;
    `Rail.test.ts` moves with them.
12. Settings' project list and `<select>` stay alphabetical (out of scope).

## Open questions

- None outstanding.
