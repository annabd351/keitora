//! The typed `GhApi` seam between [`GitHubBackend`](super::GitHubBackend) and the
//! raw [`GhTransport`](crate::github::transport::GhTransport).
//!
//! `GitHubBackend` performs *stateful* sequences (create → read → mutate → re-read)
//! that a canned byte-returning transport double cannot model. Lifting the seam to
//! typed operations lets the contract suite run against an in-memory `FakeGitHub`
//! (see [`super::fake`]) while production wires [`GhCliApi`] over the real `gh` CLI.
//!
//! The raw `GhTransport` + [`classify`](crate::github::transport::classify) +
//! [`retry`](crate::github::retry::retry) stack remains the boundary *under*
//! [`GhCliApi`]; this module is where typed ops become `gh` argv.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::backend::{BackendError, BackendResult};
use crate::github::retry::{retry, Clock, RetryPolicy};
use crate::github::transport::{GhRequest, GhTransport};

use super::{ProjectRef, StatusMap};

/// Open/closed state of an issue, with the GitHub close-reason for closed issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueState {
    Open,
    /// `not_planned == true` → closed as *not planned* (Keitora `cancelled`);
    /// `false` → closed as *completed* (Keitora `done`).
    Closed {
        not_planned: bool,
    },
}

/// A raw GitHub issue as the backend needs it: the fields that map onto a
/// [`Task`](crate::model::Task) plus the project Status column value.
#[derive(Debug, Clone)]
pub(crate) struct RawIssue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: IssueState,
    /// The issue's value in the project "Status" single-select field, if set.
    pub status_column: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Typed GitHub operations the backend depends on.
///
/// Implementors: [`GhCliApi`] (production) and `FakeGitHub` (`#[cfg(test)]`).
#[async_trait]
pub(crate) trait GhApi: Send + Sync {
    async fn issue_get(&self, project: &ProjectRef, number: i64)
        -> BackendResult<Option<RawIssue>>;

    /// Every issue on the project board (open + closed). Reads cross-filter
    /// client-side; the backend keeps the published `ListFilter` contract.
    async fn issue_list_all(&self, project: &ProjectRef) -> BackendResult<Vec<RawIssue>>;

    async fn issue_create(
        &self,
        project: &ProjectRef,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> BackendResult<RawIssue>;

    async fn issue_edit(
        &self,
        project: &ProjectRef,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> BackendResult<()>;

    async fn add_label(&self, project: &ProjectRef, number: i64, label: &str) -> BackendResult<()>;

    /// Ensure a label exists in the repo, creating it if absent. Idempotent — a
    /// no-op when the label is already defined. `add_label` errors on an unknown
    /// label, so a brand-new label (e.g. the delete tombstone) must be ensured first.
    async fn ensure_label(&self, project: &ProjectRef, label: &str) -> BackendResult<()>;

    async fn remove_label(
        &self,
        project: &ProjectRef,
        number: i64,
        label: &str,
    ) -> BackendResult<()>;

    /// Set the project "Status" single-select column for an issue.
    async fn set_status_field(
        &self,
        project: &ProjectRef,
        number: i64,
        column: &str,
    ) -> BackendResult<()>;

    /// Close an issue. `not_planned` → *not planned* (cancel); else *completed* (done).
    async fn close_issue(
        &self,
        project: &ProjectRef,
        number: i64,
        not_planned: bool,
    ) -> BackendResult<()>;

    async fn reopen_issue(&self, project: &ProjectRef, number: i64) -> BackendResult<()>;

    /// Find an existing issue carrying the hidden idempotency marker (find-before-create).
    async fn find_by_idempotency_marker(
        &self,
        project: &ProjectRef,
        key: &str,
    ) -> BackendResult<Option<RawIssue>>;

    /// An opaque change cursor for the project (the GitHub half of `revision()`).
    async fn project_cursor(&self, project: &ProjectRef) -> BackendResult<String>;
}

// ---------------------------------------------------------------------------
// Production implementation over the raw transport.
// ---------------------------------------------------------------------------

/// Production [`GhApi`] over the `GhTransport` + `classify`.
///
/// This is a thin, **single-shot** typed wrapper: one `GhApi` method → one
/// `gh` invocation. Retry/backoff is *not* applied here — it is layered
/// uniformly by [`RetryingApi`] so the same retry policy governs every `GhApi`
/// implementation (production and the test fake). That keeps the resilience
/// behaviour testable network-free (the contract suite wraps `FakeGitHub` in
/// [`RetryingApi`] and asserts retry via fault injection).
///
/// Projects v2 writes/reads need the board's GraphQL node id, the Status
/// single-select field id, and an option id per column. These can only be read
/// from a live board, so `GhCliApi` resolves them **lazily on first use** (the
/// same lazy-I/O policy as the `gh` auth preflight) and caches the result. The
/// resolution config (`project_number` from `[tasks.github]`, the `status_map`
/// overrides) is carried alongside the transport.
pub(crate) struct GhCliApi {
    tx: Box<dyn GhTransport>,
    /// `[tasks.github] project_number`, if configured; `None` → resolve the
    /// repo's linked project deterministically at first use.
    project_number: Option<u64>,
    /// The keitora↔column map (convention + `[tasks.github.status_map]`), used to
    /// know which columns must exist and which option id each keitora status writes.
    status_map: StatusMap,
    /// Resolved Projects-v2 ids, populated once on first use.
    resolved: OnceCell<ResolvedProject>,
}

/// The board ids `GhCliApi` resolves once and reuses: the ProjectV2 node id, the
/// Status single-select field id, and a column-display-name → option-id lookup.
#[derive(Debug, Clone)]
struct ResolvedProject {
    /// The board number actually in use (configured, or the deterministically
    /// picked linked project).
    number: u64,
    /// ProjectV2 GraphQL node id (`PVT_...`).
    project_id: String,
    /// The "Status" single-select field id (`PVTSSF_...`).
    status_field_id: String,
    /// Column display name → single-select option id. Case-insensitive lookups
    /// go through [`ResolvedProject::option_id`].
    column_to_option: BTreeMap<String, String>,
}

impl ResolvedProject {
    /// The single-select option id for a column display name (case-insensitive).
    fn option_id(&self, column: &str) -> Option<&str> {
        self.column_to_option
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(column))
            .map(|(_, id)| id.as_str())
    }
}

impl GhCliApi {
    /// Construct with the resolution config from `[tasks.github]`: the optional
    /// project number and the status map (convention + overrides).
    pub(crate) fn with_config(
        tx: Box<dyn GhTransport>,
        project_number: Option<u64>,
        status_map: StatusMap,
    ) -> Self {
        Self {
            tx,
            project_number,
            status_map,
            resolved: OnceCell::new(),
        }
    }

    /// Run a read request (single shot) and return raw stdout.
    async fn run_read(&self, args: Vec<String>) -> BackendResult<Vec<u8>> {
        self.tx
            .run(GhRequest {
                args,
                stdin: None,
                mutation: false,
            })
            .await
    }

    /// Run a mutation (single shot). A non-retryable-but-maybe-applied error
    /// (`Permanent` from a mutation's `200+errors[]`, or a 409) is the caller's
    /// signal to do write-then-verify by re-reading; we surface it as-is.
    async fn run_mutation(&self, args: Vec<String>) -> BackendResult<Vec<u8>> {
        self.tx
            .run(GhRequest {
                args,
                stdin: None,
                mutation: true,
            })
            .await
    }

    /// Resolve (and cache) the board's ProjectV2 id + Status field/option ids.
    ///
    /// Steps (mirrors the `/task` skill's Step 0):
    /// 1. Determine the project number: configured, else pick the repo's linked
    ///    project deterministically (exact-title > stripped-suffix > first).
    /// 2. `gh project field-list <n> --owner <o> --format json` → the Status
    ///    single-select field id + each column's option id.
    /// 3. Resolve the ProjectV2 node id via `gh project view`.
    /// 4. Validate every required column ([`StatusMap::required_columns`]) has an
    ///    option id; a missing one → [`BackendError::Config`].
    async fn resolve<'a>(&'a self, project: &ProjectRef) -> BackendResult<&'a ResolvedProject> {
        self.resolved
            .get_or_try_init(|| self.resolve_uncached(project))
            .await
    }

    async fn resolve_uncached(&self, project: &ProjectRef) -> BackendResult<ResolvedProject> {
        let number = match self.project_number {
            Some(n) => n,
            None => self.resolve_linked_project_number(project).await?,
        };

        // ProjectV2 node id + the Status field/option ids.
        let project_id = self.resolve_project_id(project, number).await?;
        let field_bytes = self
            .run_read(vec![
                "project".into(),
                "field-list".into(),
                number.to_string(),
                "--owner".into(),
                project.owner.clone(),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        let (status_field_id, column_to_option) = parse_status_field(&field_bytes)?;

        // Validate every column we may write actually exists on the board.
        for col in self.status_map.required_columns() {
            if !column_to_option
                .keys()
                .any(|name| name.eq_ignore_ascii_case(&col))
            {
                return Err(BackendError::Config(format!(
                    "GitHub project #{number} ({}/{}) has no Status column {col:?} \
                     (required by the Keitora status map); add the column or set \
                     [tasks.github.status_map] to an existing column",
                    project.owner, project.repo
                )));
            }
        }

        Ok(ResolvedProject {
            number,
            project_id,
            status_field_id,
            column_to_option,
        })
    }

    /// Resolve the repo's linked project number deterministically when no
    /// `project_number` is configured.
    async fn resolve_linked_project_number(&self, project: &ProjectRef) -> BackendResult<u64> {
        let query = "query($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { \
               projectsV2(first: 20) { nodes { number title } } } }";
        let bytes = self
            .run_read(vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                "-f".into(),
                format!("owner={}", project.owner),
                "-f".into(),
                format!("name={}", project.repo),
            ])
            .await?;
        let projects = parse_linked_projects(&bytes)?;
        pick_linked_project(&projects, &project.repo)
            .map(|p| p.number)
            .ok_or_else(|| {
                BackendError::Config(format!(
                    "no GitHub Project linked to {}/{} — link one in the GitHub UI or set \
                     [tasks.github] project_number",
                    project.owner, project.repo
                ))
            })
    }

    /// Resolve the ProjectV2 GraphQL node id for `number` via `gh project view`.
    async fn resolve_project_id(&self, project: &ProjectRef, number: u64) -> BackendResult<String> {
        let bytes = self
            .run_read(vec![
                "project".into(),
                "view".into(),
                number.to_string(),
                "--owner".into(),
                project.owner.clone(),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        parse_project_id(&bytes)
    }

    /// Resolve the ProjectV2Item id for issue `number` *on the board `project_number`*.
    /// An issue can belong to several projects; we filter to ours.
    async fn resolve_item_id(
        &self,
        project: &ProjectRef,
        number: i64,
        project_number: u64,
    ) -> BackendResult<String> {
        let query = "query($owner: String!, $name: String!, $number: Int!) { \
             repository(owner: $owner, name: $name) { \
               issue(number: $number) { \
                 projectItems(first: 20) { nodes { id project { number } } } } } }";
        let bytes = self
            .run_read(vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                // owner/name are `String!` → `-f`: `-F` would coerce an all-digit value to an
                // int and fail `String!` validation. `number` is `Int!` → `-F`.
                "-f".into(),
                format!("owner={}", project.owner),
                "-f".into(),
                format!("name={}", project.repo),
                "-F".into(),
                format!("number={number}"),
            ])
            .await?;
        parse_project_item_id(&bytes, project_number)?.ok_or_else(|| {
            BackendError::Permanent(format!(
                "issue #{number} is not on GitHub project #{project_number} \
                 (no matching project item) — cannot set its Status"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Retry decorator — uniform retry/backoff over any GhApi.
// ---------------------------------------------------------------------------

/// Wraps an inner [`GhApi`] so every operation is retried per a [`RetryPolicy`]
/// via the injectable [`Clock`]. Retrying is gated on
/// [`BackendError::is_retryable`](crate::backend::BackendError::is_retryable):
/// `Transient`/`Timeout`/`RateLimited` are retried (honouring `Retry-After` as a
/// floor); `Conflict`/`Unauthorized`/`Config`/`Unavailable`/`NotFound` and a
/// mutation's maybe-applied `Permanent` return immediately so the backend can
/// reconcile via write-then-verify.
pub(crate) struct RetryingApi<A: GhApi> {
    inner: A,
    policy: RetryPolicy,
    clock: Arc<dyn Clock>,
}

impl<A: GhApi> RetryingApi<A> {
    pub(crate) fn new(inner: A, policy: RetryPolicy, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            policy,
            clock,
        }
    }

    /// Borrow the wrapped inner api (used by the contract suite to drive the
    /// `FakeGitHub` fault-injection knobs).
    #[cfg(test)]
    pub(crate) fn inner(&self) -> &A {
        &self.inner
    }
}

#[async_trait]
impl<A: GhApi> GhApi for RetryingApi<A> {
    async fn issue_get(
        &self,
        project: &ProjectRef,
        number: i64,
    ) -> BackendResult<Option<RawIssue>> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.issue_get(project, number)
        })
        .await
    }

    async fn issue_list_all(&self, project: &ProjectRef) -> BackendResult<Vec<RawIssue>> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.issue_list_all(project)
        })
        .await
    }

    async fn issue_create(
        &self,
        project: &ProjectRef,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> BackendResult<RawIssue> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.issue_create(project, title, body, labels)
        })
        .await
    }

    async fn issue_edit(
        &self,
        project: &ProjectRef,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.issue_edit(project, number, title, body)
        })
        .await
    }

    async fn add_label(&self, project: &ProjectRef, number: i64, label: &str) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.add_label(project, number, label)
        })
        .await
    }

    async fn ensure_label(&self, project: &ProjectRef, label: &str) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.ensure_label(project, label)
        })
        .await
    }

    async fn remove_label(
        &self,
        project: &ProjectRef,
        number: i64,
        label: &str,
    ) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.remove_label(project, number, label)
        })
        .await
    }

    async fn set_status_field(
        &self,
        project: &ProjectRef,
        number: i64,
        column: &str,
    ) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.set_status_field(project, number, column)
        })
        .await
    }

    async fn close_issue(
        &self,
        project: &ProjectRef,
        number: i64,
        not_planned: bool,
    ) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.close_issue(project, number, not_planned)
        })
        .await
    }

    async fn reopen_issue(&self, project: &ProjectRef, number: i64) -> BackendResult<()> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.reopen_issue(project, number)
        })
        .await
    }

    async fn find_by_idempotency_marker(
        &self,
        project: &ProjectRef,
        key: &str,
    ) -> BackendResult<Option<RawIssue>> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.find_by_idempotency_marker(project, key)
        })
        .await
    }

    async fn project_cursor(&self, project: &ProjectRef) -> BackendResult<String> {
        retry(&self.policy, self.clock.as_ref(), || {
            self.inner.project_cursor(project)
        })
        .await
    }
}

#[async_trait]
impl GhApi for GhCliApi {
    async fn issue_get(
        &self,
        project: &ProjectRef,
        number: i64,
    ) -> BackendResult<Option<RawIssue>> {
        // Resolve the board first so reads can filter the issue's project items to
        // *this* project's Status value.
        let resolved = self.resolve(project).await?;
        let query = issue_get_query();
        let out = self
            .run_read(vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                "-f".into(),
                format!("owner={}", project.owner),
                "-f".into(),
                format!("name={}", project.repo),
                "-F".into(),
                format!("number={number}"),
            ])
            .await;
        match out {
            Ok(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes)?;
                log_rate_limit("issue_get", &v);
                let issue = v.pointer("/data/repository/issue");
                match issue {
                    Some(serde_json::Value::Null) | None => Ok(None),
                    Some(node) => Ok(Some(graphql_value_to_issue(node, resolved.number)?)),
                }
            }
            Err(BackendError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn issue_list_all(&self, project: &ProjectRef) -> BackendResult<Vec<RawIssue>> {
        let resolved = self.resolve(project).await?;
        // `--paginate` walks `pageInfo`; the query must expose `endCursor`/`hasNextPage`
        // and accept `$endCursor` (gh feeds it back on each page).
        let query = issue_list_all_query();
        let bytes = self
            .run_read(vec![
                "api".into(),
                "graphql".into(),
                "--paginate".into(),
                "-f".into(),
                format!("query={query}"),
                "-f".into(),
                format!("owner={}", project.owner),
                "-f".into(),
                format!("name={}", project.repo),
            ])
            .await?;
        log_rate_limit_bytes("issue_list_all", &bytes);
        parse_issue_nodes(&bytes, resolved.number)
    }

    async fn issue_create(
        &self,
        project: &ProjectRef,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> BackendResult<RawIssue> {
        // Idempotency: RetryingApi wraps this whole multi-step call, so a transient
        // failure on a *later* step (item-add / re-read) re-runs it from the top.
        // Parse the marker the backend embedded in `body`; if an issue already
        // carries it (created by a prior failed attempt), reuse that issue instead
        // of running `gh issue create` again — otherwise the retry duplicates it.
        let marker = super::metadata::Metadata::parse(body).idempotency_key;
        let existing = match &marker {
            Some(key) => self.find_by_idempotency_marker(project, key).await?,
            None => None,
        };

        let (number, url) = if let Some(issue) = existing {
            // Reconstruct the issue URL for item-add from owner/repo + number.
            let url = format!(
                "https://github.com/{}/{}/issues/{}",
                project.owner, project.repo, issue.number
            );
            (issue.number, url)
        } else {
            let repo = format!("{}/{}", project.owner, project.repo);
            let mut args = vec![
                "issue".into(),
                "create".into(),
                "--repo".into(),
                repo,
                "--title".into(),
                title.to_string(),
                "--body".into(),
                body.to_string(),
            ];
            for l in labels {
                args.push("--label".into());
                args.push(l.clone());
            }
            // `gh issue create` prints the new issue URL on stdout.
            let out = self.run_mutation(args).await?;
            let stdout = String::from_utf8_lossy(&out);
            let url = parse_created_issue_url(&stdout).ok_or_else(|| {
                // A create whose URL we can't parse is non-retryable (the issue may
                // exist): surface Permanent so the backend reconciles via the
                // idempotency marker rather than blindly re-creating a duplicate.
                BackendError::Permanent(format!(
                    "could not parse the created issue URL from `gh issue create` output: {:?}",
                    stdout.trim()
                ))
            })?;
            let number = parse_issue_number_from_url(&url).ok_or_else(|| {
                BackendError::Permanent(format!(
                    "could not parse an issue number from the created issue URL {url:?}"
                ))
            })?;
            (number, url)
        };

        // Put the new issue on the project board so its Status can be set. If a
        // prior attempt already added it, `gh project item-add` is effectively a
        // no-op; treat an "already on the board" failure as success defensively so
        // a retried call doesn't error on the second add.
        let resolved = self.resolve(project).await?;
        match self
            .run_mutation(vec![
                "project".into(),
                "item-add".into(),
                resolved.number.to_string(),
                "--owner".into(),
                project.owner.clone(),
                "--url".into(),
                url.clone(),
            ])
            .await
        {
            Ok(_) => {}
            Err(e) if is_already_on_board(&e) => {}
            Err(e) => return Err(e),
        }

        // Re-read so the returned record reflects the board membership; the
        // backend sets the initial Status itself (create → set_status → re-read).
        self.issue_get(project, number)
            .await?
            .ok_or(BackendError::NotFound(number))
    }

    async fn issue_edit(
        &self,
        project: &ProjectRef,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> BackendResult<()> {
        let repo = format!("{}/{}", project.owner, project.repo);
        let mut args = vec![
            "issue".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            repo,
        ];
        if let Some(t) = title {
            args.push("--title".into());
            args.push(t.to_string());
        }
        if let Some(b) = body {
            args.push("--body".into());
            args.push(b.to_string());
        }
        self.run_mutation(args).await.map(|_| ())
    }

    async fn add_label(&self, project: &ProjectRef, number: i64, label: &str) -> BackendResult<()> {
        let repo = format!("{}/{}", project.owner, project.repo);
        self.run_mutation(vec![
            "issue".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            repo,
            "--add-label".into(),
            label.to_string(),
        ])
        .await
        .map(|_| ())
    }

    async fn ensure_label(&self, project: &ProjectRef, label: &str) -> BackendResult<()> {
        // `gh label create --force` upserts: it creates the label if missing and
        // succeeds (rather than erroring) when it already exists, so this is a safe
        // idempotent ensure.
        let repo = format!("{}/{}", project.owner, project.repo);
        self.run_mutation(vec![
            "label".into(),
            "create".into(),
            label.to_string(),
            "--repo".into(),
            repo,
            "--force".into(),
        ])
        .await
        .map(|_| ())
    }

    async fn remove_label(
        &self,
        project: &ProjectRef,
        number: i64,
        label: &str,
    ) -> BackendResult<()> {
        let repo = format!("{}/{}", project.owner, project.repo);
        self.run_mutation(vec![
            "issue".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            repo,
            "--remove-label".into(),
            label.to_string(),
        ])
        .await
        .map(|_| ())
    }

    async fn set_status_field(
        &self,
        project: &ProjectRef,
        number: i64,
        column: &str,
    ) -> BackendResult<()> {
        let resolved = self.resolve(project).await?;
        let option_id = resolved.option_id(column).ok_or_else(|| {
            // resolve() validates the StatusMap columns up front; a miss here means
            // a caller asked for a column outside the map (programmer error).
            BackendError::Config(format!(
                "no Status option id for column {column:?} on GitHub project #{}",
                resolved.number
            ))
        })?;

        // The issue's ProjectV2Item id *for this board* (an issue can sit on many).
        let item_id = self
            .resolve_item_id(project, number, resolved.number)
            .await?;

        // `updateProjectV2ItemFieldValue` — mutation:true so a 200+errors[] / 409
        // is non-retryable and the backend reconciles via write-then-verify.
        // NB: `SET_STATUS_MUTATION` deliberately carries NO rateLimit (Query-root field).
        self.run_mutation(set_status_mutation_args(
            &resolved.project_id,
            &item_id,
            &resolved.status_field_id,
            option_id,
        ))
        .await
        .map(|_| ())
    }

    async fn close_issue(
        &self,
        project: &ProjectRef,
        number: i64,
        not_planned: bool,
    ) -> BackendResult<()> {
        let repo = format!("{}/{}", project.owner, project.repo);
        let mut args = vec![
            "issue".into(),
            "close".into(),
            number.to_string(),
            "--repo".into(),
            repo,
        ];
        if not_planned {
            args.push("--reason".into());
            args.push("not planned".into());
        }
        self.run_mutation(args).await.map(|_| ())
    }

    async fn reopen_issue(&self, project: &ProjectRef, number: i64) -> BackendResult<()> {
        let repo = format!("{}/{}", project.owner, project.repo);
        self.run_mutation(vec![
            "issue".into(),
            "reopen".into(),
            number.to_string(),
            "--repo".into(),
            repo,
        ])
        .await
        .map(|_| ())
    }

    async fn find_by_idempotency_marker(
        &self,
        project: &ProjectRef,
        key: &str,
    ) -> BackendResult<Option<RawIssue>> {
        // Search the board for an issue whose metadata block carries the key.
        let all = self.issue_list_all(project).await?;
        Ok(all
            .into_iter()
            .find(|i| super::Metadata::parse(&i.body).idempotency_key.as_deref() == Some(key)))
    }

    async fn project_cursor(&self, project: &ProjectRef) -> BackendResult<String> {
        // The GitHub half of revision(): one lightweight request for the board's
        // ProjectV2 `updatedAt` (the timestamp GitHub bumps on any board change —
        // item add/remove, field-value edits). This replaces the old full
        // `issue_list_all` digest, which paginated the whole board on every poll
        //. No pagination, no per-issue work.
        let resolved = self.resolve(project).await?;
        let query = project_cursor_query();
        let bytes = self
            .run_read(vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                // `id` is `ID!` → `-f`, not `-F` (which coerces all-digit values to ints).
                "-f".into(),
                format!("id={}", resolved.project_id),
            ])
            .await?;
        log_rate_limit_bytes("project_cursor", &bytes);
        parse_project_updated_at(&bytes)
    }
}

// ---------------------------------------------------------------------------
// GraphQL parsing — issues (with their project-item Status), project/field ids.
// ---------------------------------------------------------------------------

/// The GraphQL selection set for one issue node, shared by `issue_get` and
/// `issue_list_all`. Includes `projectItems` so the read can derive the project
/// Status column in the same round-trip (read-path option (a) from the plan).
///
/// `fieldValueByName(name: "Status")` returns the single-select value; we keep
/// `project { number }` to filter to *our* board when an issue sits on several.
const ISSUE_FIELDS: &str = "number title body state stateReason createdAt updatedAt \
     labels(first: 100) { nodes { name } } \
     projectItems(first: 20) { nodes { \
       project { number } \
       fieldValueByName(name: \"Status\") { \
         ... on ProjectV2ItemFieldSingleSelectValue { name } } } }";

/// The query/mutation-**root** `rateLimit` selection. It is NOT part of
/// [`ISSUE_FIELDS`] (an issue-node selection); it must be appended to the OUTER
/// query document, as a sibling of `repository`/`node`/the mutation field. Gives
/// per-call GraphQL point cost + remaining budget before the hard limit fires.
const RATE_LIMIT_FIELDS: &str = " rateLimit { cost remaining limit resetAt }";

/// Extract `(cost, remaining)` from a `gh api graphql` response root's `rateLimit`
/// block, or `None` when it (or either field) is absent — a REST fallback or older
/// API version simply has no `rateLimit`, which must never be an error.
fn parse_rate_limit(data_root: &serde_json::Value) -> Option<(i64, i64)> {
    let rl = data_root.pointer("/data/rateLimit")?;
    let cost = rl.get("cost")?.as_i64()?;
    let remaining = rl.get("remaining")?.as_i64()?;
    Some((cost, remaining))
}

/// Emit one structured debug line for the GraphQL `rateLimit { cost remaining }`
/// carried at the response root. Additive telemetry only: silently skips when the
/// field is absent and never affects control flow. `action` names the calling
/// `GhApi` method.
fn log_rate_limit(action: &str, data_root: &serde_json::Value) {
    if let Some((cost, remaining)) = parse_rate_limit(data_root) {
        log::debug!("gh graphql {action}: cost={cost} remaining={remaining}");
    }
}

/// [`log_rate_limit`] from raw `gh api graphql` bytes: parses the FIRST top-level
/// JSON object (a `--paginate` stream concatenates one per page) and logs its
/// `rateLimit`. Best-effort — a parse failure is silently ignored.
fn log_rate_limit_bytes(action: &str, bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    if let Some(Ok(first)) = serde_json::Deserializer::from_str(text)
        .into_iter::<serde_json::Value>()
        .next()
    {
        log_rate_limit(action, &first);
    }
}

// ---- GraphQL document builders ------------------------------------------------
// The read documents carry `RATE_LIMIT_FIELDS` at the query root; the mutation MUST
// NOT (see `SET_STATUS_MUTATION`). Extracted so the invariant is unit-testable
// (a query field wrongly placed in a mutation is invalid on the live API but the
// in-memory fake can't catch it).

/// `issue_get` — single-issue read.
fn issue_get_query() -> String {
    "query($owner: String!, $name: String!, $number: Int!) { \
         repository(owner: $owner, name: $name) { \
           issue(number: $number) { "
        .to_string()
        + ISSUE_FIELDS
        + " } }"
        + RATE_LIMIT_FIELDS
        + " }"
}

/// `issue_list_all` — paginated full-board read.
fn issue_list_all_query() -> String {
    "query($owner: String!, $name: String!, $endCursor: String) { \
         repository(owner: $owner, name: $name) { \
           issues(first: 100, after: $endCursor, states: [OPEN, CLOSED], \
                  orderBy: {field: UPDATED_AT, direction: DESC}) { \
             pageInfo { hasNextPage endCursor } \
             nodes { "
        .to_string()
        + ISSUE_FIELDS
        + " } } }"
        + RATE_LIMIT_FIELDS
        + " }"
}

/// `project_cursor` — the ProjectV2 `updatedAt` read used by `revision()`.
fn project_cursor_query() -> String {
    "query($id: ID!) { node(id: $id) { \
         ... on ProjectV2 { updatedAt } }"
        .to_string()
        + RATE_LIMIT_FIELDS
        + " }"
}

/// `set_status` mutation. NB: NO `rateLimit` — it is a **Query**-root field and does
/// not exist on `Mutation`; appending it makes the whole mutation fail validation.
const SET_STATUS_MUTATION: &str =
    "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { \
         updateProjectV2ItemFieldValue(input: { \
           projectId: $project, itemId: $item, fieldId: $field, \
           value: { singleSelectOptionId: $option } }) { \
             projectV2Item { id } } }";

/// argv for the `set_status` mutation ([`SET_STATUS_MUTATION`]).
///
/// Every value is `ID!`/`String!`, so all use `-f`, never `-F`. `gh api -F` type-coerces its
/// value (anything parsing as a number/bool/null becomes that JSON type); Status option ids can
/// be all-digits (e.g. `98236657`), which `-F` would send as an int and fail the
/// `$option: String!` validation — the bug behind `task done` failing on such boards. A pure
/// argv builder so this `-f`/`-F` invariant is testable without a live transport.
fn set_status_mutation_args(
    project_id: &str,
    item_id: &str,
    field_id: &str,
    option_id: &str,
) -> Vec<String> {
    vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={SET_STATUS_MUTATION}"),
        "-f".into(),
        format!("project={project_id}"),
        "-f".into(),
        format!("item={item_id}"),
        "-f".into(),
        format!("field={field_id}"),
        "-f".into(),
        format!("option={option_id}"),
    ]
}

/// Parse the `issues.nodes` array out of a (possibly `--paginate`-concatenated)
/// `gh api graphql` body, mapping each node to a [`RawIssue`].
///
/// `gh --paginate` concatenates each page's JSON object (whitespace-separated).
/// We read every top-level value from the stream and flatten their
/// `data.repository.issues.nodes` arrays.
fn parse_issue_nodes(bytes: &[u8], project_number: u64) -> BackendResult<Vec<RawIssue>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| BackendError::Permanent(format!("non-UTF8 gh output: {e}")))?;
    let mut out = Vec::new();
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    let mut saw_any = false;
    for page in stream.by_ref() {
        let page = page?;
        saw_any = true;
        let nodes = page
            .pointer("/data/repository/issues/nodes")
            .and_then(|n| n.as_array());
        if let Some(nodes) = nodes {
            for node in nodes {
                out.push(graphql_value_to_issue(node, project_number)?);
            }
        }
    }
    if !saw_any {
        return Err(BackendError::Permanent(
            "empty `gh api graphql` issues response".to_string(),
        ));
    }
    Ok(out)
}

/// Convert one GraphQL issue node into a [`RawIssue`], deriving `status_column`
/// from the issue's project item on board `project_number` (if any).
fn graphql_value_to_issue(v: &serde_json::Value, project_number: u64) -> BackendResult<RawIssue> {
    let number = v.get("number").and_then(|n| n.as_i64()).ok_or_else(|| {
        BackendError::Permanent("issue JSON missing numeric `number`".to_string())
    })?;
    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let body = v
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let labels = v
        .pointer("/labels/nodes")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let state_str = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("OPEN")
        .to_uppercase();
    let reason = v
        .get("stateReason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_uppercase();
    let state = if state_str == "CLOSED" {
        IssueState::Closed {
            not_planned: reason == "NOT_PLANNED",
        }
    } else {
        IssueState::Open
    };
    let status_column = status_from_project_items(v.get("projectItems"), project_number);
    Ok(RawIssue {
        number,
        title,
        body,
        labels,
        state,
        status_column,
        created_at: v
            .get("createdAt")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: v
            .get("updatedAt")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Extract the Status single-select value name from an issue's `projectItems`,
/// filtering to the item on board `project_number`. `None` when the issue is not
/// on the board, or its Status is unset.
fn status_from_project_items(
    project_items: Option<&serde_json::Value>,
    project_number: u64,
) -> Option<String> {
    let nodes = project_items?.get("nodes")?.as_array()?;
    for item in nodes {
        let num = item.pointer("/project/number").and_then(|n| n.as_u64());
        if num == Some(project_number) {
            return item
                .pointer("/fieldValueByName/name")
                .and_then(|n| n.as_str())
                .map(str::to_string);
        }
    }
    None
}

/// A linked-project candidate parsed from `repository.projectsV2.nodes`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkedProject {
    number: u64,
    title: String,
}

/// Parse `repository.projectsV2.nodes` out of the linked-projects GraphQL body.
fn parse_linked_projects(bytes: &[u8]) -> BackendResult<Vec<LinkedProject>> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let nodes = v
        .pointer("/data/repository/projectsV2/nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| {
            BackendError::Permanent("unexpected linked-projects response shape".to_string())
        })?;
    Ok(nodes
        .iter()
        .filter_map(|n| {
            let number = n.get("number").and_then(|x| x.as_u64())?;
            let title = n
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(LinkedProject { number, title })
        })
        .collect())
}

/// Deterministic linked-project pick (mirrors the `/task` skill): exact-title
/// match with `repo` > stripped-suffix match (drop `-godot`/`-app`/`-web`) >
/// first. Returns `None` only for an empty list.
fn pick_linked_project<'a>(projects: &'a [LinkedProject], repo: &str) -> Option<&'a LinkedProject> {
    if projects.is_empty() {
        return None;
    }
    if let Some(p) = projects.iter().find(|p| p.title.eq_ignore_ascii_case(repo)) {
        return Some(p);
    }
    let stripped = repo
        .strip_suffix("-godot")
        .or_else(|| repo.strip_suffix("-app"))
        .or_else(|| repo.strip_suffix("-web"))
        .unwrap_or(repo);
    if let Some(p) = projects
        .iter()
        .find(|p| p.title.eq_ignore_ascii_case(stripped))
    {
        return Some(p);
    }
    projects.first()
}

/// Parse the ProjectV2 `updatedAt` timestamp from the project-cursor GraphQL
/// body (`node(id) { ... on ProjectV2 { updatedAt } }`). This single value is the
/// GitHub half of `revision()` — it advances on any board mutation.
fn parse_project_updated_at(bytes: &[u8]) -> BackendResult<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    v.pointer("/data/node/updatedAt")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            BackendError::Permanent("project-cursor response missing `node.updatedAt`".to_string())
        })
}

/// Parse the ProjectV2 node id from `gh project view --format json`.
fn parse_project_id(bytes: &[u8]) -> BackendResult<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    v.get("id")
        .and_then(|i| i.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            BackendError::Permanent("`gh project view` output missing project `id`".to_string())
        })
}

/// Parse `gh project field-list --format json` into the Status single-select
/// field id + a `{column display name → option id}` map.
///
/// Shape (gh ≥ 2.x):
/// ```json
/// { "fields": [
///     { "id": "PVTSSF_x", "name": "Status", "type": "ProjectV2SingleSelectField",
///       "options": [ { "id": "abc", "name": "Backlog" }, ... ] },
///     ... ] }
/// ```
/// A missing Status single-select field → [`BackendError::Config`].
fn parse_status_field(bytes: &[u8]) -> BackendResult<(String, BTreeMap<String, String>)> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let fields = v.get("fields").and_then(|f| f.as_array()).ok_or_else(|| {
        BackendError::Permanent("`gh project field-list` output missing `fields`".to_string())
    })?;
    let status = fields
        .iter()
        .find(|f| {
            f.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.eq_ignore_ascii_case("Status"))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            BackendError::Config(
                "GitHub project has no \"Status\" field — add a single-select Status \
                 field to the board"
                    .to_string(),
            )
        })?;
    let field_id = status
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| BackendError::Permanent("Status field missing `id`".to_string()))?
        .to_string();
    let options = status
        .get("options")
        .and_then(|o| o.as_array())
        .ok_or_else(|| {
            BackendError::Config(
                "the project \"Status\" field is not a single-select (no options) — \
                 Keitora requires a single-select Status field"
                    .to_string(),
            )
        })?;
    let mut map = BTreeMap::new();
    for opt in options {
        let name = opt.get("name").and_then(|n| n.as_str());
        let id = opt.get("id").and_then(|i| i.as_str());
        if let (Some(name), Some(id)) = (name, id) {
            map.insert(name.to_string(), id.to_string());
        }
    }
    Ok((field_id, map))
}

/// Find the ProjectV2Item id for the project-items GraphQL body, filtered to the
/// item on board `project_number`. `Ok(None)` when the issue isn't on the board.
fn parse_project_item_id(bytes: &[u8], project_number: u64) -> BackendResult<Option<String>> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let nodes = v
        .pointer("/data/repository/issue/projectItems/nodes")
        .and_then(|n| n.as_array());
    let Some(nodes) = nodes else {
        return Ok(None);
    };
    for item in nodes {
        let num = item.pointer("/project/number").and_then(|n| n.as_u64());
        if num == Some(project_number) {
            return Ok(item.get("id").and_then(|i| i.as_str()).map(str::to_string));
        }
    }
    Ok(None)
}

/// `true` if a `gh project item-add` error indicates the issue is already on the
/// board (so a retried `issue_create` can treat it as success rather than error).
/// Matched defensively on the error text — gh has no stable code for this.
fn is_already_on_board(e: &BackendError) -> bool {
    let text = e.to_string().to_lowercase();
    text.contains("already") && (text.contains("project") || text.contains("item"))
}

/// Parse the created-issue URL line from `gh issue create` stdout. `gh` prints
/// the issue URL (possibly among other lines / with a trailing newline).
fn parse_created_issue_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.contains("github.com/") && l.contains("/issues/"))
        .map(str::to_string)
}

/// Parse the trailing issue number from an issue URL,
/// e.g. `https://github.com/o/r/issues/42` → `42`.
fn parse_issue_number_from_url(url: &str) -> Option<i64> {
    let url = url.trim().trim_end_matches('/');
    let (_, tail) = url.rsplit_once("/issues/")?;
    // Tolerate a trailing fragment/query (#…, ?…) after the number.
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

// ---------------------------------------------------------------------------
// git remote parsing helper (owner/repo resolution).
// ---------------------------------------------------------------------------

/// Parse `owner/repo` from a git remote URL (https and `git@` forms).
///
/// Examples:
/// - `https://github.com/acme/rpg-game.git` → `("acme", "rpg-game")`
/// - `git@github.com:acme/rpg-game.git` → `("acme", "rpg-game")`
pub(crate) fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    // Strip the scheme/host prefix.
    let path = if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:owner/repo(.git)
        rest.split_once(':').map(|(_, p)| p)?
    } else if let Some(idx) = url.find("://") {
        // scheme://host/owner/repo(.git)
        let after = &url[idx + 3..];
        after.split_once('/').map(|(_, p)| p)?
    } else {
        // host:owner/repo or owner/repo
        url.split_once(':').map(|(_, p)| p).unwrap_or(url)
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut it = path.split('/').filter(|s| !s.is_empty());
    let owner = it.next()?.to_string();
    let repo = it.next()?.to_string();
    Some((owner, repo))
}

/// Resolve `owner/repo` from `git remote get-url origin` in `cwd`.
pub(crate) fn owner_repo_from_git_remote(cwd: &std::path::Path) -> BackendResult<(String, String)> {
    let out = crate::config::git_command()
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .map_err(|e| BackendError::Config(format!("failed to run `git remote get-url`: {e}")))?;
    if !out.status.success() {
        return Err(BackendError::Config(
            "could not read `origin` remote — set [tasks.github] owner/repo explicitly".to_string(),
        ));
    }
    let url = String::from_utf8_lossy(&out.stdout);
    parse_owner_repo(&url).ok_or_else(|| {
        BackendError::Config(format!(
            "could not parse owner/repo from origin URL {:?}",
            url.trim()
        ))
    })
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_remote() {
        assert_eq!(
            parse_owner_repo("https://github.com/acme/rpg-game.git"),
            Some(("acme".into(), "rpg-game".into()))
        );
    }

    #[test]
    fn parse_ssh_remote() {
        assert_eq!(
            parse_owner_repo("git@github.com:acme/rpg-game.git"),
            Some(("acme".into(), "rpg-game".into()))
        );
    }

    #[test]
    fn parse_remote_without_dot_git() {
        assert_eq!(
            parse_owner_repo("https://github.com/o/r"),
            Some(("o".into(), "r".into()))
        );
    }

    // ---- created-issue URL → number ------------------------------------

    #[test]
    fn created_url_parse_valid() {
        let stdout = "https://github.com/acme/rpg-game/issues/42";
        let url = parse_created_issue_url(stdout).unwrap();
        assert_eq!(url, "https://github.com/acme/rpg-game/issues/42");
        assert_eq!(parse_issue_number_from_url(&url), Some(42));
    }

    #[test]
    fn created_url_parse_trailing_newline_and_noise() {
        // `gh` may print a leading status line before the URL, plus a trailing \n.
        let stdout = "Creating issue in acme/rpg-game\n\
                      https://github.com/acme/rpg-game/issues/7\n";
        let url = parse_created_issue_url(stdout).expect("url line found");
        assert_eq!(parse_issue_number_from_url(&url), Some(7));
    }

    #[test]
    fn created_url_parse_malformed_errors() {
        // No issue URL at all → no line matched.
        assert!(parse_created_issue_url("some unexpected gh chatter\n").is_none());
        // A URL-ish line but no numeric tail → number parse fails.
        assert_eq!(
            parse_issue_number_from_url("https://github.com/o/r/issues/"),
            None
        );
        assert_eq!(
            parse_issue_number_from_url("https://github.com/o/r/pull/9"),
            None
        );
    }

    #[test]
    fn created_url_number_tolerates_fragment() {
        assert_eq!(
            parse_issue_number_from_url("https://github.com/o/r/issues/123#issuecomment-1"),
            Some(123)
        );
    }

    // ---- gh project field-list → (field id, {column → option id}) -------

    #[test]
    fn parse_status_field_maps_options() {
        let json = br#"{
          "fields": [
            { "id": "PVTF_title", "name": "Title", "type": "ProjectV2Field" },
            { "id": "PVTSSF_status", "name": "Status",
              "type": "ProjectV2SingleSelectField",
              "options": [
                { "id": "opt-backlog", "name": "Backlog" },
                { "id": "opt-ready",   "name": "Ready" },
                { "id": "opt-prog",    "name": "In progress" },
                { "id": "opt-done",    "name": "Done" }
              ] }
          ]
        }"#;
        let (field_id, map) = parse_status_field(json).unwrap();
        assert_eq!(field_id, "PVTSSF_status");
        assert_eq!(map.get("In progress").map(String::as_str), Some("opt-prog"));
        assert_eq!(map.get("Backlog").map(String::as_str), Some("opt-backlog"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn parse_status_field_missing_is_config() {
        let json = br#"{ "fields": [ { "id": "x", "name": "Title" } ] }"#;
        let err = parse_status_field(json).unwrap_err();
        assert!(matches!(err, BackendError::Config(_)), "got {err:?}");
    }

    #[test]
    fn parse_status_field_not_single_select_is_config() {
        // A "Status" field that isn't a single-select (no options array).
        let json =
            br#"{ "fields": [ { "id": "x", "name": "Status", "type": "ProjectV2Field" } ] }"#;
        let err = parse_status_field(json).unwrap_err();
        assert!(matches!(err, BackendError::Config(_)), "got {err:?}");
    }

    // ---- issue node → RawIssue (+ status_column from project items) -----

    fn issue_node(status: Option<&str>, project_number: u64) -> String {
        let items = match status {
            Some(s) => format!(
                r#"[{{ "project": {{ "number": {project_number} }},
                       "fieldValueByName": {{ "name": "{s}" }} }}]"#
            ),
            None => "[]".to_string(),
        };
        format!(
            r#"{{
              "number": 5, "title": "t", "body": "b",
              "state": "OPEN", "stateReason": null,
              "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-02T00:00:00Z",
              "labels": {{ "nodes": [ {{ "name": "priority:high" }}, {{ "name": "keitora:feng" }} ] }},
              "projectItems": {{ "nodes": {items} }}
            }}"#
        )
    }

    #[test]
    fn issue_node_status_from_matching_project() {
        let node: serde_json::Value =
            serde_json::from_str(&issue_node(Some("In progress"), 3)).unwrap();
        let issue = graphql_value_to_issue(&node, 3).unwrap();
        assert_eq!(issue.number, 5);
        assert_eq!(issue.labels, vec!["priority:high", "keitora:feng"]);
        assert_eq!(issue.status_column.as_deref(), Some("In progress"));
    }

    #[test]
    fn issue_node_status_none_when_not_on_board() {
        // The issue's only project item is on a *different* board (#9), not #3.
        let node: serde_json::Value =
            serde_json::from_str(&issue_node(Some("In progress"), 9)).unwrap();
        let issue = graphql_value_to_issue(&node, 3).unwrap();
        assert_eq!(issue.status_column, None);
    }

    #[test]
    fn issue_node_status_none_when_no_project_item() {
        let node: serde_json::Value = serde_json::from_str(&issue_node(None, 3)).unwrap();
        let issue = graphql_value_to_issue(&node, 3).unwrap();
        assert_eq!(issue.status_column, None);
    }

    #[test]
    fn issue_node_closed_not_planned() {
        let json = r#"{
          "number": 8, "title": "u", "body": "",
          "state": "CLOSED", "stateReason": "NOT_PLANNED",
          "createdAt": "", "updatedAt": "",
          "labels": { "nodes": [] },
          "projectItems": { "nodes": [] }
        }"#;
        let node: serde_json::Value = serde_json::from_str(json).unwrap();
        let issue = graphql_value_to_issue(&node, 3).unwrap();
        assert_eq!(issue.state, IssueState::Closed { not_planned: true });
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_issue_nodes_paginated_concatenation() {
        // `gh --paginate` concatenates per-page JSON objects.
        let page1 = format!(
            r#"{{ "data": {{ "repository": {{ "issues": {{
                "pageInfo": {{ "hasNextPage": true, "endCursor": "c1" }},
                "nodes": [ {} ] }} }} }} }}"#,
            issue_node(Some("Ready"), 3)
        );
        let page2 = r#"{ "data": { "repository": { "issues": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [ { "number": 6, "title": "x", "body": "",
                  "state": "OPEN", "labels": { "nodes": [] },
                  "projectItems": { "nodes": [] } } ] } } } }"#;
        let bytes = format!("{page1}\n{page2}");
        let issues = parse_issue_nodes(bytes.as_bytes(), 3).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 5);
        assert_eq!(issues[0].status_column.as_deref(), Some("Ready"));
        assert_eq!(issues[1].number, 6);
    }

    // ---- project-item id resolution ------------------------------------

    #[test]
    fn parse_project_item_id_filters_to_board() {
        let json = br#"{ "data": { "repository": { "issue": { "projectItems": { "nodes": [
            { "id": "ITEM_other", "project": { "number": 9 } },
            { "id": "ITEM_ours",  "project": { "number": 3 } }
        ] } } } } }"#;
        assert_eq!(
            parse_project_item_id(json, 3).unwrap().as_deref(),
            Some("ITEM_ours")
        );
        assert_eq!(parse_project_item_id(json, 99).unwrap(), None);
    }

    #[test]
    fn parse_project_item_id_no_items_is_none() {
        let json =
            br#"{ "data": { "repository": { "issue": { "projectItems": { "nodes": [] } } } } }"#;
        assert_eq!(parse_project_item_id(json, 3).unwrap(), None);
    }

    // ---- project id + linked-project pick ------------------------------

    #[test]
    fn parse_project_updated_at_ok() {
        let json = br#"{ "data": { "node": { "updatedAt": "2026-06-24T12:00:00Z" } } }"#;
        assert_eq!(
            parse_project_updated_at(json).unwrap(),
            "2026-06-24T12:00:00Z"
        );
    }

    #[test]
    fn parse_project_updated_at_missing_is_permanent() {
        let json = br#"{ "data": { "node": {} } }"#;
        let err = parse_project_updated_at(json).unwrap_err();
        assert!(matches!(err, BackendError::Permanent(_)), "got {err:?}");
    }

    #[test]
    fn parse_project_id_ok() {
        let json = br#"{ "number": 3, "id": "PVT_abc", "title": "rpg-game" }"#;
        assert_eq!(parse_project_id(json).unwrap(), "PVT_abc");
    }

    #[test]
    fn parse_rate_limit_extracts_cost_and_remaining() {
        // A response carrying the query-root rateLimit alongside the data.
        let v: serde_json::Value = serde_json::from_str(
            r#"{ "data": { "repository": { "issue": null },
                 "rateLimit": { "cost": 1, "remaining": 4998, "limit": 5000,
                                "resetAt": "2026-06-24T13:00:00Z" } } }"#,
        )
        .unwrap();
        assert_eq!(parse_rate_limit(&v), Some((1, 4998)));
    }

    #[test]
    fn parse_rate_limit_absent_is_none() {
        // No rateLimit block (REST fallback / older API) → None, never an error.
        let v: serde_json::Value =
            serde_json::from_str(r#"{ "data": { "repository": { "issue": null } } }"#).unwrap();
        assert_eq!(parse_rate_limit(&v), None);
    }

    /// `{`/`}` are balanced in a GraphQL document — a cheap guard that the
    /// concatenated read queries + the mutation are structurally well-formed.
    fn braces_balanced(doc: &str) -> bool {
        let mut depth: i32 = 0;
        for c in doc.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return false;
            }
        }
        depth == 0
    }

    #[test]
    fn rate_limit_only_on_query_documents_never_on_mutation() {
        // `rateLimit` is a GitHub GraphQL Query-root field; putting it in a mutation
        // document makes the whole mutation fail validation on the live API (the
        // in-memory fake can't catch this). Guard the placement invariant here.
        for q in [
            issue_get_query(),
            issue_list_all_query(),
            project_cursor_query(),
        ] {
            assert!(
                q.contains("rateLimit"),
                "read query must carry rateLimit: {q}"
            );
            assert!(braces_balanced(&q), "unbalanced braces in query: {q}");
        }
        assert!(
            !SET_STATUS_MUTATION.contains("rateLimit"),
            "rateLimit must NOT appear in a mutation document"
        );
        assert!(
            braces_balanced(SET_STATUS_MUTATION),
            "unbalanced braces in mutation"
        );
    }

    #[test]
    fn set_status_argv_uses_f_never_capital_f() {
        // Regression: all-digit Status option ids (e.g. `98236657`) must not go through `-F`,
        // which coerces them to an int and fails `$option: String!`. Every arg is `ID!`/`String!`,
        // so all use `-f`.
        let argv = set_status_mutation_args("PVT_project", "PVTI_item", "PVTSSF_field", "98236657");
        for key in ["query", "project", "item", "field", "option"] {
            let prefix = format!("{key}=");
            let pos = argv
                .iter()
                .position(|a| a.starts_with(&prefix))
                .unwrap_or_else(|| panic!("missing `{key}=` arg in {argv:?}"));
            assert!(pos > 0, "`{key}=` has no preceding flag in {argv:?}");
            assert_eq!(
                argv[pos - 1],
                "-f",
                "`{key}` must be passed with `-f` (not `-F`, which coerces numeric ids): {argv:?}"
            );
        }
        // Belt and braces: `-F` must not appear anywhere in this mutation's argv.
        assert!(
            !argv.iter().any(|a| a == "-F"),
            "set_status argv must not use `-F`: {argv:?}"
        );
    }

    #[test]
    fn linked_project_pick_exact_title() {
        let projects = vec![
            LinkedProject {
                number: 1,
                title: "Random".into(),
            },
            LinkedProject {
                number: 2,
                title: "rpg-game".into(),
            },
            LinkedProject {
                number: 3,
                title: "rpg".into(),
            },
        ];
        assert_eq!(
            pick_linked_project(&projects, "rpg-game").unwrap().number,
            2
        );
    }

    #[test]
    fn linked_project_pick_stripped_suffix() {
        // No exact "rpg-game-godot" title; "rpg-game" matches after dropping "-godot".
        let projects = vec![
            LinkedProject {
                number: 1,
                title: "Misc".into(),
            },
            LinkedProject {
                number: 7,
                title: "rpg-game".into(),
            },
        ];
        assert_eq!(
            pick_linked_project(&projects, "rpg-game-godot")
                .unwrap()
                .number,
            7
        );
    }

    #[test]
    fn linked_project_pick_first_fallback() {
        let projects = vec![
            LinkedProject {
                number: 4,
                title: "Alpha".into(),
            },
            LinkedProject {
                number: 5,
                title: "Beta".into(),
            },
        ];
        assert_eq!(
            pick_linked_project(&projects, "rpg-game").unwrap().number,
            4
        );
    }

    #[test]
    fn linked_project_pick_empty_is_none() {
        assert!(pick_linked_project(&[], "rpg-game").is_none());
    }

    #[test]
    fn parse_linked_projects_shape() {
        let json = br#"{ "data": { "repository": { "projectsV2": { "nodes": [
            { "number": 2, "title": "rpg-game", "id": "PVT_x" },
            { "number": 5, "title": "Other" }
        ] } } } }"#;
        let projects = parse_linked_projects(json).unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(
            projects[0],
            LinkedProject {
                number: 2,
                title: "rpg-game".into()
            }
        );
    }
}
