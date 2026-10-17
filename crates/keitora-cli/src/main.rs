//! Keitora CLI (`keitora`) — a thin command layer over the `keitora-tasks` library.
//! Task tracking (`keitora task …`), session state (`keitora session …`), and project
//! configuration (`keitora config …`) from the terminal.

use std::error::Error;

use clap::{Parser, Subcommand};
use keitora_tasks::{
    current_worktree, find_legacy_config, migrate_legacy, resolve, BackendError, ClaimOutcome,
    Config, ListFilter, LocalStore, ProjectConfig, ResolveError, Task, TaskBackend, TaskUpdate,
    UserConfig, PRIORITIES, STATUSES,
};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "keitora",
    about = "Keitora — task tracking, sessions, and config",
    version
)]
struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cross-worktree task tracking (list, claim, done, …)
    #[command(subcommand)]
    Task(TaskCmd),
    /// Record this worktree's Claude session state (used by Claude Code hooks)
    Session {
        /// One of: idle | busy | waiting | offline
        state: String,
    },
    /// Manage project configuration
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
enum TaskCmd {
    /// List tasks (open + active by default)
    List {
        /// Include archived (old done/cancelled) tasks
        #[arg(long)]
        all: bool,
        /// Only tasks assigned to this worktree
        #[arg(long)]
        mine: bool,
        /// Include backlog-status tasks
        #[arg(long)]
        backlog: bool,
    },
    /// Create a task
    New {
        title: String,
        #[arg(short = 'd', long = "desc")]
        desc: Option<String>,
        #[arg(short = 'p', long = "priority", default_value = "normal")]
        priority: String,
        /// Comma-separated tags
        #[arg(short = 't', long = "tags")]
        tags: Option<String>,
    },
    /// Show task details + activity log
    Show { id: i64 },
    /// Claim a task for this worktree
    Claim { id: i64 },
    /// Release a claim
    Unclaim { id: i64 },
    /// Mark a task done
    Done { id: i64 },
    /// Mark a task cancelled
    Cancel { id: i64 },
    /// Punt a task to the backlog
    Backlog { id: i64 },
    /// Update task fields
    Update {
        id: i64,
        #[arg(short = 't', long = "title")]
        title: Option<String>,
        #[arg(short = 'd', long = "desc")]
        desc: Option<String>,
        #[arg(short = 'p', long = "priority")]
        priority: Option<String>,
        #[arg(short = 's', long = "status")]
        status: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long = "pr")]
        pr: Option<String>,
    },
    /// Search tasks by title/description
    Find { term: String },
    /// Kanban board grouped by worktree
    Board {
        #[arg(long)]
        backlog: bool,
    },
    /// Add a dependency: <task_id> is blocked by <depends_on>
    Block { task_id: i64, depends_on: i64 },
    /// Remove a dependency
    Unblock { task_id: i64, depends_on: i64 },
    /// Show the dependency tree
    Deps { id: i64 },
    /// Permanently delete a task.
    #[command(visible_alias = "rm")]
    Delete { id: i64 },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Scaffold a new keitora.toml in the current project root
    Init,
    /// Migrate a legacy task-config.json to keitora.toml
    Migrate,
    /// List registered projects and show which is active
    Projects,
    /// Set the active project by name
    Use {
        /// The name of a registered project (as shown by `config projects`)
        name: String,
    },
}

// ---- ANSI helpers ----
fn pri_marker(p: &str) -> &'static str {
    match p {
        "urgent" => "\x1b[31m■\x1b[0m",
        "high" => "\x1b[33m■\x1b[0m",
        "normal" => "\x1b[36m■\x1b[0m",
        "low" => "\x1b[90m■\x1b[0m",
        _ => "■",
    }
}
fn status_label(s: &str) -> String {
    let c = match s {
        "open" => "\x1b[37mopen\x1b[0m",
        "active" => "\x1b[32mactive\x1b[0m",
        "backlog" => "\x1b[35mbacklog\x1b[0m",
        "done" => "\x1b[90mdone\x1b[0m",
        "cancelled" => "\x1b[90mcancelled\x1b[0m",
        other => other,
    };
    c.to_string()
}

async fn print_task_row(backend: &dyn TaskBackend, t: &Task) -> Result<(), Box<dyn Error>> {
    let blocked = backend.blocked_by(t.id).await?;
    let blocked_str = if blocked.is_empty() {
        String::new()
    } else {
        let ids = blocked
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(" \x1b[31m[blocked by {ids}]\x1b[0m")
    };
    let assignee = t
        .assigned_to
        .as_deref()
        .map(|a| format!("\x1b[35m@{a}\x1b[0m"))
        .unwrap_or_default();
    let tags = if t.tags.is_empty() {
        String::new()
    } else {
        format!(" \x1b[90m[{}]\x1b[0m", t.tags.join(","))
    };
    println!(
        "  {} #{}  {}  {}  {}{}{}",
        pri_marker(&t.priority),
        t.id,
        status_label(&t.status),
        t.title,
        assignee,
        tags,
        blocked_str
    );
    Ok(())
}

fn validate(value: &str, allowed: &[&str], label: &str) -> Result<(), Box<dyn Error>> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "invalid {label} \"{value}\" — must be one of: {}",
            allowed.join(", ")
        )
        .into())
    }
}

/// The actor identity used for CLI-originated writes: the current git worktree name.
/// Matches the behaviour of `Store::open` which baked in `current_worktree()` at
/// construction time.
fn cli_actor() -> String {
    current_worktree()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    // Config subcommands do not need the store; short-circuit before resolve().
    if let Cmd::Config(ref sub) = cli.cmd {
        return run_config(sub, cli.json);
    }

    let config = resolve()
        .map(Config::from_project)
        .map_err(|e| -> Box<dyn Error> {
            match e {
                ResolveError::NotFound | ResolveError::NeedsOnboarding => {
                    "no keitora.toml found — run `keitora config init` to create one, \
                     or `keitora config migrate` if you have a legacy task-config.json"
                        .into()
                }
                ResolveError::Io(msg) => msg.into(),
            }
        })?;

    // Task/dispatch commands use the async TaskBackend.
    // Session commands use the synchronous LocalStore.
    let backend = keitora_tasks::open_backend(config.project_config())?;
    let local = LocalStore::open(&config.db_path)?;

    match cli.cmd {
        Cmd::Config(_) => unreachable!("handled above"),
        Cmd::Session { state } => {
            // The hook entry point — runs on every turn, so keep it cheap: one
            // targeted query for the active task id, no full list scan.
            let me = cli_actor();
            let current = local.active_task_id(&me).ok().flatten();
            local.session_upsert(&me, &state, current, None)?;
            emit_ok(cli.json, &format!("  {me} session → {state}"))?;
        }
        Cmd::Task(task) => run_task(task, backend.as_ref(), &config, cli.json).await?,
    }

    Ok(())
}

/// Dispatch the `keitora task …` subcommands. Split from `run` so the top-level
/// `Cmd` stays a flat task/session/config tree.
async fn run_task(
    task: TaskCmd,
    backend: &dyn TaskBackend,
    config: &Config,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let me = cli_actor();

    match task {
        TaskCmd::List { all, mine, backlog } => {
            let filter = ListFilter {
                include_archived: all,
                include_backlog: backlog,
                assigned_to: if mine { Some(me.to_string()) } else { None },
                archive_days: config.archive_days,
            };
            let tasks = backend.list(&filter).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("  No tasks found.");
            } else {
                for t in &tasks {
                    print_task_row(backend, &t.task).await?;
                }
            }
        }

        TaskCmd::New {
            title,
            desc,
            priority,
            tags,
        } => {
            validate(&priority, &PRIORITIES, "priority")?;
            let tag_vec: Vec<String> = tags
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let task = backend
                .create(
                    &me,
                    &title,
                    desc.as_deref().unwrap_or(""),
                    &priority,
                    &tag_vec,
                    None,
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("  Created task #{}: {}", task.id, task.title);
            }
        }

        TaskCmd::Show { id } => {
            let twd = backend
                .get(id)
                .await?
                .ok_or_else(|| format!("task #{id} not found"))?;
            if json {
                let logs = backend.logs(id, 20).await?;
                let mut v = serde_json::to_value(&twd)?;
                v.as_object_mut()
                    .unwrap()
                    .insert("logs".into(), serde_json::to_value(logs)?);
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let t = &twd.task;
                println!("\n  #{}  {}", t.id, t.title);
                println!(
                    "  Status: {}  Priority: {}",
                    status_label(&t.status),
                    t.priority
                );
                if let Some(a) = &t.assigned_to {
                    println!("  Assigned: @{a}");
                }
                if !t.description.is_empty() {
                    println!("  Description: {}", t.description);
                }
                if !t.tags.is_empty() {
                    println!("  Tags: {}", t.tags.join(", "));
                }
                if let Some(b) = &t.branch {
                    println!("  Branch: {b}");
                }
                if let Some(p) = &t.pr_url {
                    println!("  PR: {p}");
                }
                if !twd.blocked_by.is_empty() {
                    let ids = twd
                        .blocked_by
                        .iter()
                        .map(|b| format!("#{b}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  \x1b[31mBlocked by: {ids}\x1b[0m");
                }
                if !twd.blocks.is_empty() {
                    let ids = twd
                        .blocks
                        .iter()
                        .map(|b| format!("#{b}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  Blocks: {ids}");
                }
                println!("  Created: {}  Updated: {}", t.created_at, t.updated_at);
                let logs = backend.logs(id, 20).await?;
                if !logs.is_empty() {
                    println!("\n  Activity:");
                    for l in logs {
                        println!(
                            "    {}  @{}  {}  {}",
                            l.created_at, l.worktree, l.action, l.detail
                        );
                    }
                }
                println!();
            }
        }

        TaskCmd::Claim { id } => {
            let (task, msg) = match backend.claim(id, &me).await? {
                ClaimOutcome::Claimed(t) => (t, format!("  Claimed task #{id} for @{me}")),
                ClaimOutcome::AlreadyYours(t) => {
                    (t, format!("  Task #{id} is already yours (@{me})"))
                }
                ClaimOutcome::ClaimedUnverified(t) => (
                    t,
                    format!(
                        "  Claim submitted for #{id} — verify it stuck (eventually-consistent backend)"
                    ),
                ),
            };
            emit_task_msg(json, &task, &msg)?;
        }
        TaskCmd::Unclaim { id } => {
            backend.unclaim(id, &me).await?;
            emit_ok(json, &format!("  Released task #{id}"))?;
        }
        TaskCmd::Done { id } => {
            let outcome = backend.set_done(id, &me).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "task": outcome.task, "unblocked": outcome.unblocked })
                    )?
                );
            } else {
                println!("  Completed task #{id}");
                if !outcome.unblocked.is_empty() {
                    let ids = outcome
                        .unblocked
                        .iter()
                        .map(|u| format!("#{u}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("  Unblocked: {ids}");
                }
            }
        }
        TaskCmd::Cancel { id } => {
            backend.cancel(id, &me).await?;
            emit_ok(json, &format!("  Cancelled task #{id}"))?;
        }
        TaskCmd::Backlog { id } => {
            backend.backlog(id, &me).await?;
            emit_ok(json, &format!("  Moved task #{id} to backlog"))?;
        }

        TaskCmd::Update {
            id,
            title,
            desc,
            priority,
            status,
            branch,
            pr,
        } => {
            if let Some(p) = &priority {
                validate(p, &PRIORITIES, "priority")?;
            }
            if let Some(s) = &status {
                validate(s, &STATUSES, "status")?;
            }
            let upd = TaskUpdate {
                title,
                description: desc,
                priority,
                status,
                branch,
                pr_url: pr,
            };
            backend.update(id, &me, &upd).await?;
            emit_ok(json, &format!("  Updated task #{id}"))?;
        }

        TaskCmd::Find { term } => {
            let tasks = backend.find(&term, config.archive_days).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("  No tasks matching \"{term}\"");
            } else {
                for t in &tasks {
                    print_task_row(backend, t).await?;
                }
            }
        }

        TaskCmd::Board { backlog } => {
            let filter = ListFilter {
                include_archived: false,
                include_backlog: backlog,
                assigned_to: None,
                archive_days: config.archive_days,
            };
            let tasks = backend.list(&filter).await?;

            // Columns: open, each worktree, [backlog], done.
            let mut order: Vec<String> = vec!["open".into()];
            order.extend(config.worktrees.iter().cloned());
            if backlog {
                order.push("backlog".into());
            }
            order.push("done".into());

            let mut columns: std::collections::BTreeMap<String, Vec<Task>> =
                std::collections::BTreeMap::new();
            for col in &order {
                columns.insert(col.clone(), Vec::new());
            }
            for twd in tasks {
                let t = twd.task;
                let col = if t.status == "done" || t.status == "cancelled" {
                    "done".to_string()
                } else if t.status == "backlog" {
                    if backlog {
                        "backlog".to_string()
                    } else {
                        continue;
                    }
                } else if let Some(a) = &t.assigned_to {
                    if columns.contains_key(a) {
                        a.clone()
                    } else {
                        "open".to_string()
                    }
                } else {
                    "open".to_string()
                };
                columns.get_mut(&col).unwrap().push(t);
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&columns)?);
            } else {
                for col in &order {
                    let list = &columns[col];
                    if col == "done" && list.is_empty() {
                        continue;
                    }
                    println!("\n  \x1b[1m{}\x1b[0m ({})", col.to_uppercase(), list.len());
                    for t in list {
                        print_task_row(backend, t).await?;
                    }
                }
                println!();
            }
        }

        TaskCmd::Block {
            task_id,
            depends_on,
        } => {
            // The backend returns `Conflict("dependency already exists")` for a
            // duplicate edge (the universal contract — see backend_tests). Absorb that
            // one as idempotent success ("already blocked"); other conflicts (self-dep,
            // cycle) stay errors.
            match backend.add_dep(task_id, depends_on, &me).await {
                Ok(()) => emit_ok(
                    json,
                    &format!("  Task #{task_id} now blocked by #{depends_on}"),
                )?,
                Err(BackendError::Conflict(msg)) if msg.contains("already exists") => emit_ok(
                    json,
                    &format!("  Task #{task_id} already blocked by #{depends_on}"),
                )?,
                Err(e) => return Err(e.into()),
            }
        }
        TaskCmd::Unblock {
            task_id,
            depends_on,
        } => {
            backend.remove_dep(task_id, depends_on, &me).await?;
            emit_ok(
                json,
                &format!("  Removed dependency: #{task_id} no longer blocked by #{depends_on}"),
            )?;
        }
        TaskCmd::Deps { id } => {
            let nodes = backend.deps_tree(id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                let t = backend
                    .get(id)
                    .await?
                    .ok_or_else(|| format!("task #{id} not found"))?;
                println!("\n  Dependencies for #{id}: {}", t.task.title);
                if nodes.is_empty() {
                    println!("  (none)");
                } else {
                    for n in nodes {
                        let indent = "  ".repeat(n.depth as usize);
                        let mark = if n.status == "done" {
                            "\x1b[32m✓\x1b[0m"
                        } else {
                            "\x1b[31m○\x1b[0m"
                        };
                        println!("  {indent}{mark} #{} {} ({})", n.id, n.title, n.status);
                    }
                }
                println!();
            }
        }
        TaskCmd::Delete { id } => {
            backend.delete(id).await?;
            emit_ok(json, &format!("  Deleted task #{id}"))?;
        }
    }

    Ok(())
}

// ---- config subcommand dispatch ----

fn run_config(sub: &ConfigCmd, json: bool) -> Result<(), Box<dyn Error>> {
    match sub {
        ConfigCmd::Init => cmd_config_init(json),
        ConfigCmd::Migrate => cmd_config_migrate(json),
        ConfigCmd::Projects => cmd_config_projects(json),
        ConfigCmd::Use { name } => cmd_config_use(name, json),
    }
}

fn cmd_config_init(json: bool) -> Result<(), Box<dyn Error>> {
    // 1. Locate project root via git toplevel.
    let project_root = keitora_tasks::git_toplevel().ok_or("not inside a git repository")?;

    // 2. Candidate keitora.toml path.
    let toml_path = project_root.join("keitora.toml");

    // 3. Error if it already exists.
    if toml_path.exists() {
        return Err(format!(
            "keitora.toml already exists at {} \
             — run `keitora config migrate` if you have a legacy task-config.json",
            toml_path.display()
        )
        .into());
    }

    // 4–5. Scaffold via the shared helper so the CLI and the app's onboarding
    //      produce identical configs (correct retention defaults + shared DB
    //      path under <name>-worktrees/shared/, not a divergent flat tasks.db).
    let pc = ProjectConfig::scaffold(&project_root);
    let name = pc.project.name.clone();
    pc.write_initial().map_err(|e| e.to_string())?;

    // 6–9. Register in user config; set active if not yet set.
    let mut user_cfg = UserConfig::load().map_err(|e| e.to_string())?;
    user_cfg.register(&name, project_root.clone());
    let set_active = user_cfg.active.is_none();
    if set_active {
        user_cfg.set_active(&name);
    }
    user_cfg.save().map_err(|e| e.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": toml_path.to_string_lossy(),
                "name": name,
                "set_active": set_active,
            }))?
        );
    } else {
        println!("  Initialized keitora.toml at {}", toml_path.display());
        println!("  Registered project \"{name}\" in user config");
        if set_active {
            println!("  Active project set to \"{name}\"");
        }
    }

    Ok(())
}

fn cmd_config_migrate(json: bool) -> Result<(), Box<dyn Error>> {
    // 1. Locate legacy task-config.json.
    let legacy_path = find_legacy_config().ok_or("no task-config.json found")?;
    if !legacy_path.exists() {
        return Err("no task-config.json found".into());
    }

    // 2. Migrate (idempotent). `wrote` is the accurate signal — true when a new
    //    keitora.toml was written, false on the idempotent no-op — so we don't have
    //    to infer it from registry state (which misfires on name collisions or a
    //    stale registry after a manual delete).
    let (pc, wrote) = migrate_legacy(&legacy_path).map_err(|e| format!("migration failed: {e}"))?;
    let toml_path = pc.root.join("keitora.toml");
    let already_existed = !wrote;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": toml_path.to_string_lossy(),
                "already_existed": already_existed,
            }))?
        );
    } else if already_existed {
        println!(
            "  keitora.toml already exists at {} — nothing to do",
            toml_path.display()
        );
    } else {
        println!("  Migrated task-config.json → {}", toml_path.display());
    }

    Ok(())
}

fn cmd_config_projects(json: bool) -> Result<(), Box<dyn Error>> {
    let user_cfg = UserConfig::load().map_err(|e| e.to_string())?;

    if json {
        let projects: serde_json::Map<String, serde_json::Value> = user_cfg
            .projects
            .iter()
            .map(|(k, v)| (k.clone(), json!({ "path": v.path.to_string_lossy() })))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "active": user_cfg.active,
                "projects": projects,
            }))?
        );
    } else if user_cfg.projects.is_empty() {
        println!("  No projects registered. Run `keitora config init` to register this project.");
    } else {
        for (name, pref) in &user_cfg.projects {
            let is_active = user_cfg.active.as_deref() == Some(name.as_str());
            let marker = if is_active { "*" } else { " " };
            // Path column tab-aligned 16 chars from start of name.
            println!("  {marker} {name:<16}{}", pref.path.display());
        }
    }

    Ok(())
}

fn cmd_config_use(name: &str, json: bool) -> Result<(), Box<dyn Error>> {
    let mut user_cfg = UserConfig::load().map_err(|e| e.to_string())?;

    if !user_cfg.projects.contains_key(name) {
        return Err(format!(
            "unknown project \"{name}\" — run `keitora config projects` to see registered names"
        )
        .into());
    }

    user_cfg.set_active(name);
    user_cfg.save().map_err(|e| e.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "active": name }))?
        );
    } else {
        println!("  Active project set to \"{name}\"");
    }

    Ok(())
}

fn emit_ok(json: bool, msg: &str) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string(&json!({ "ok": true }))?);
    } else {
        println!("{msg}");
    }
    Ok(())
}

fn emit_task_msg(json: bool, task: &Task, msg: &str) -> Result<(), Box<dyn Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(task)?);
    } else {
        println!("{msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `config` subcommand wiring is the new surface in this binary; the
    // heavy logic (scaffold defaults, migration) lives in (and is tested by)
    // `keitora-tasks`. Guard the clap parsing so the subcommands can't silently
    // change shape.
    #[test]
    fn parses_config_subcommands() {
        match Cli::try_parse_from(["keitora", "config", "use", "myproj"])
            .unwrap()
            .cmd
        {
            Cmd::Config(ConfigCmd::Use { name }) => assert_eq!(name, "myproj"),
            _ => panic!("expected `config use`"),
        }
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "init"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Init)
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "migrate"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Migrate)
        ));
        assert!(matches!(
            Cli::try_parse_from(["keitora", "config", "projects"])
                .unwrap()
                .cmd,
            Cmd::Config(ConfigCmd::Projects)
        ));
        // `config use` requires a project name.
        assert!(Cli::try_parse_from(["keitora", "config", "use"]).is_err());
    }

    // Task verbs now live under the `task` subcommand; `session` and `config`
    // are siblings, not task verbs.
    #[test]
    fn parses_task_and_sibling_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["keitora", "task", "list"])
                .unwrap()
                .cmd,
            Cmd::Task(TaskCmd::List { .. })
        ));
        match Cli::try_parse_from(["keitora", "task", "claim", "7"])
            .unwrap()
            .cmd
        {
            Cmd::Task(TaskCmd::Claim { id }) => assert_eq!(id, 7),
            _ => panic!("expected `task claim`"),
        }
        match Cli::try_parse_from(["keitora", "session", "idle"])
            .unwrap()
            .cmd
        {
            Cmd::Session { state } => assert_eq!(state, "idle"),
            _ => panic!("expected `session`"),
        }
        // `--json` is global: valid before or after the subcommand path.
        assert!(Cli::try_parse_from(["keitora", "--json", "task", "list"]).is_ok());
        assert!(Cli::try_parse_from(["keitora", "task", "list", "--json"]).is_ok());
        // Old flat form must NOT parse anymore (clean break — no `task` alias).
        assert!(Cli::try_parse_from(["keitora", "list"]).is_err());
    }
}
