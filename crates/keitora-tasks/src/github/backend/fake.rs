//! `FakeGitHub` — a stateful in-memory [`GhApi`] for the contract suite.
//!
//! The universal contract tier ([`crate::backend_tests::run_universal`]) performs
//! real stateful sequences (create → get → claim → set_done → assert state). A
//! canned byte-returning transport double cannot satisfy that, so this models the
//! pieces of GitHub the backend round-trips: an auto-incrementing issue number,
//! per-issue `{title, body, labels, state, status column}`, and the project Status
//! options. It also exposes **fault-injection knobs** for the Eventual tier:
//!
//! - `fail_next_mutation` — make the next mutating op return a scripted error
//!   (e.g. `Timeout`/`Transient` to drive retry, or a maybe-applied `Permanent`).
//! - a `mutation_count` so tests can assert how many times a mutation was issued.
//!
//! Gated on `#[cfg(test)]`; never compiled into a release build.

use async_trait::async_trait;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use crate::backend::{BackendError, BackendResult};
use crate::github::backend::api::{GhApi, IssueState, RawIssue};
use crate::github::backend::ProjectRef;

#[derive(Clone)]
struct StoredIssue {
    title: String,
    body: String,
    labels: Vec<String>,
    state: IssueState,
    status_column: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Default)]
struct State {
    next_number: i64,
    issues: BTreeMap<i64, StoredIssue>,
    /// A monotonic clock for created/updated timestamps (deterministic).
    tick: i64,
    /// Scripted errors for the next N mutations (FIFO).
    fail_next: VecDeque<BackendError>,
    /// Count of mutating calls (create/edit/label/status/close/reopen).
    mutation_count: u64,
    /// Count of *attempts* including ones consumed by fault injection.
    attempt_count: u64,
    /// Count of `issue_list_all` reads — round-trip-count assertions.
    list_all_count: u64,
    /// Count of `issue_get` reads — round-trip-count assertions.
    get_count: u64,
    /// When set, `issue_create` records the new issue, THEN fails once with this
    /// error on the post-create step (modeling a transient item-add failure that
    /// leaves the issue created). The retried call must dedupe via the marker
    /// rather than create a second issue (FIX #1).
    fail_after_create: Option<BackendError>,
}

/// In-memory [`GhApi`] for the contract suite.
pub(crate) struct FakeGitHub {
    state: Mutex<State>,
}

impl FakeGitHub {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                next_number: 1,
                ..Default::default()
            }),
        }
    }

    /// Script the next mutation to fail with `err` (consumed once). Multiple calls
    /// queue multiple failures in order.
    pub(crate) fn fail_next_mutation(&self, err: BackendError) {
        self.state.lock().unwrap().fail_next.push_back(err);
    }

    /// Total mutating *attempts* (including ones the fault injector failed).
    pub(crate) fn attempt_count(&self) -> u64 {
        self.state.lock().unwrap().attempt_count
    }

    /// Number of `issue_list_all` (full-board) reads issued so far. Lets a test
    /// assert a combined read makes exactly one board fetch.
    pub(crate) fn list_all_count(&self) -> u64 {
        self.state.lock().unwrap().list_all_count
    }

    /// Number of single-issue `issue_get` reads issued so far. Lets a test assert a
    /// combined `board_snapshot` makes ZERO per-candidate reads.
    pub(crate) fn get_count(&self) -> u64 {
        self.state.lock().unwrap().get_count
    }

    /// Script `issue_create` to record the new issue and THEN fail once with `err`
    /// on the post-create step (consumed once). Drives the FIX #1 dedupe path: the
    /// RetryingApi retry must reuse the recorded issue, not create a duplicate.
    pub(crate) fn fail_after_create_once(&self, err: BackendError) {
        self.state.lock().unwrap().fail_after_create = Some(err);
    }

    /// Count of issues whose body carries the given idempotency marker.
    pub(crate) fn count_with_marker(&self, key: &str) -> usize {
        let state = self.state.lock().unwrap();
        state
            .issues
            .values()
            .filter(|s| {
                crate::github::backend::Metadata::parse(&s.body)
                    .idempotency_key
                    .as_deref()
                    == Some(key)
            })
            .count()
    }

    fn now(state: &mut State) -> String {
        state.tick += 1;
        format!("2026-01-01T00:00:{:02}Z", state.tick % 60)
    }

    /// Consume a scripted fault if one is queued. Returns `Err` to short-circuit
    /// the mutation before it applies (modeling a failed write).
    fn check_fault(state: &mut State) -> BackendResult<()> {
        state.attempt_count += 1;
        if let Some(err) = state.fail_next.pop_front() {
            return Err(err);
        }
        Ok(())
    }

    fn to_raw(number: i64, s: &StoredIssue) -> RawIssue {
        RawIssue {
            number,
            title: s.title.clone(),
            body: s.body.clone(),
            labels: s.labels.clone(),
            state: s.state,
            status_column: s.status_column.clone(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
        }
    }
}

#[async_trait]
impl GhApi for FakeGitHub {
    async fn issue_get(
        &self,
        _project: &ProjectRef,
        number: i64,
    ) -> BackendResult<Option<RawIssue>> {
        let mut state = self.state.lock().unwrap();
        state.get_count += 1;
        Ok(state.issues.get(&number).map(|s| Self::to_raw(number, s)))
    }

    async fn issue_list_all(&self, _project: &ProjectRef) -> BackendResult<Vec<RawIssue>> {
        let mut state = self.state.lock().unwrap();
        state.list_all_count += 1;
        Ok(state
            .issues
            .iter()
            .map(|(n, s)| Self::to_raw(*n, s))
            .collect())
    }

    async fn issue_create(
        &self,
        _project: &ProjectRef,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> BackendResult<RawIssue> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;

        // Idempotency dedupe (mirrors GhCliApi::issue_create, FIX #1): if an issue
        // already carries the body's marker (from a prior failed attempt), reuse it
        // instead of creating a second issue.
        let marker = crate::github::backend::Metadata::parse(body).idempotency_key;
        if let Some(key) = &marker {
            if let Some((n, s)) = state.issues.iter().find(|(_, s)| {
                crate::github::backend::Metadata::parse(&s.body)
                    .idempotency_key
                    .as_deref()
                    == Some(key.as_str())
            }) {
                let (n, s) = (*n, s.clone());
                // Apply the post-create fault on this retried path too, so a fault
                // queued for the *first* attempt's post-create step still fires.
                if let Some(err) = state.fail_after_create.take() {
                    return Err(err);
                }
                return Ok(Self::to_raw(n, &s));
            }
        }

        let number = state.next_number;
        state.next_number += 1;
        let now = Self::now(&mut state);
        let issue = StoredIssue {
            title: title.to_string(),
            body: body.to_string(),
            labels: labels.to_vec(),
            state: IssueState::Open,
            status_column: None,
            created_at: now.clone(),
            updated_at: now,
        };
        state.mutation_count += 1;
        state.issues.insert(number, issue.clone());
        // Post-create fault: the issue IS recorded, then the step after create
        // fails once. The RetryingApi retry re-enters issue_create and must dedupe
        // via the marker above (exactly one issue ends up with the marker).
        if let Some(err) = state.fail_after_create.take() {
            return Err(err);
        }
        Ok(Self::to_raw(number, &issue))
    }

    async fn issue_edit(
        &self,
        _project: &ProjectRef,
        number: i64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        if let Some(t) = title {
            issue.title = t.to_string();
        }
        if let Some(b) = body {
            issue.body = b.to_string();
        }
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn add_label(
        &self,
        _project: &ProjectRef,
        number: i64,
        label: &str,
    ) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        if !issue.labels.iter().any(|l| l == label) {
            issue.labels.push(label.to_string());
        }
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn ensure_label(&self, _project: &ProjectRef, _label: &str) -> BackendResult<()> {
        // The fake has no repo-level label registry; add_label accepts any string,
        // so ensuring a label is a no-op (it honours the injected fault for parity).
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        Ok(())
    }

    async fn remove_label(
        &self,
        _project: &ProjectRef,
        number: i64,
        label: &str,
    ) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        issue.labels.retain(|l| l != label);
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn set_status_field(
        &self,
        _project: &ProjectRef,
        number: i64,
        column: &str,
    ) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        issue.status_column = Some(column.to_string());
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn close_issue(
        &self,
        _project: &ProjectRef,
        number: i64,
        not_planned: bool,
    ) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        issue.state = IssueState::Closed { not_planned };
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn reopen_issue(&self, _project: &ProjectRef, number: i64) -> BackendResult<()> {
        let mut state = self.state.lock().unwrap();
        Self::check_fault(&mut state)?;
        let now = Self::now(&mut state);
        let issue = state
            .issues
            .get_mut(&number)
            .ok_or(BackendError::NotFound(number))?;
        issue.state = IssueState::Open;
        issue.updated_at = now;
        state.mutation_count += 1;
        Ok(())
    }

    async fn find_by_idempotency_marker(
        &self,
        _project: &ProjectRef,
        key: &str,
    ) -> BackendResult<Option<RawIssue>> {
        let state = self.state.lock().unwrap();
        Ok(state.issues.iter().find_map(|(n, s)| {
            let meta = crate::github::backend::Metadata::parse(&s.body);
            if meta.idempotency_key.as_deref() == Some(key) {
                Some(Self::to_raw(*n, s))
            } else {
                None
            }
        }))
    }

    async fn project_cursor(&self, _project: &ProjectRef) -> BackendResult<String> {
        let state = self.state.lock().unwrap();
        // The composed GitHub half: total mutations + newest updatedAt. Advances
        // on every applied GitHub mutation; stable between mutations.
        let newest = state
            .issues
            .values()
            .map(|s| s.updated_at.clone())
            .max()
            .unwrap_or_default();
        Ok(format!("{}:{}", state.mutation_count, newest))
    }
}
