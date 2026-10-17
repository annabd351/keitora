//! `exe_missing` — the "herdr was not found" state and its message.
//!
//! The backend holds one [`ExeMissing`]. It is set when spawning `herdr` fails
//! because the executable does not exist, and cleared when a search finds it or
//! a herdr call runs again. While set, its message is the `reason` of the
//! `daemon://status` payload, and [`ExeMissing::overlay`] puts the same text
//! (with [`NOT_FOUND_CODE`]) on every `daemon://recovery` payload on its way to
//! the UI. The recovery controller's own payload is never changed, so a later
//! publish neither keeps a stale message nor drops one that is still true.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::env::ExeDiscovery;
use super::ops::HerdrError;
use super::recovery::RecoveryPayload;

/// `reason_code` of a recovery payload while herdr is not found.
pub const NOT_FOUND_CODE: &str = "herdr_not_found";

/// How many searched locations the message names before counting the rest.
const MAX_NAMED: usize = 5;
/// How many it names when none of them is a notable one.
const MAX_NAMED_FALLBACK: usize = 3;

/// `path` for display, with the user's home shortened to `~`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if !rest.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        _ => path.display().to_string(),
    }
}

/// `["a"]` → `a`; `["a", "b", "c"]` with `last_and` → `a, b and c`, without →
/// `a, b, c`.
fn join_list(items: &[String], last_and: bool) -> String {
    match items {
        [head @ .., last] if last_and && !head.is_empty() => {
            format!("{} and {last}", head.join(", "))
        }
        _ => items.join(", "),
    }
}

/// The user-facing text for a herdr executable that was not found: the short
/// statement first (the rail shows one truncated line), then where Keitora
/// looked, then what to do.
///
/// `searched` can hold every directory of the login `PATH`, so only a few
/// locations are named: the `notable` ones (what the user configured, then the
/// usual install locations) that were searched, else the first few searched.
/// The rest are counted.
pub fn not_found_message(searched: &[PathBuf], notable: &[PathBuf], home: Option<&Path>) -> String {
    let mut all: Vec<&PathBuf> = Vec::new();
    for p in searched {
        if !all.contains(&p) {
            all.push(p);
        }
    }
    let mut named: Vec<&PathBuf> = Vec::new();
    for p in notable {
        if named.len() < MAX_NAMED && all.contains(&p) && !named.contains(&p) {
            named.push(p);
        }
    }
    if named.is_empty() {
        named = all.iter().copied().take(MAX_NAMED_FALLBACK).collect();
    }
    let rest = all.len() - named.len();
    let names: Vec<String> = named.iter().map(|p| display_path(p, home)).collect();

    let mut text = String::from("herdr was not found.");
    if names.is_empty() {
        text.push_str(" It is not on the PATH.");
    } else if rest == 0 {
        text.push_str(&format!(" Looked in {}.", join_list(&names, true)));
    } else {
        text.push_str(&format!(
            " Looked in {} and {rest} other {}.",
            join_list(&names, false),
            if rest == 1 { "location" } else { "locations" }
        ));
    }
    text.push_str(
        " Install herdr, or set [pty] herdr_bin in keitora.toml to its full path, \
         then press Restart.",
    );
    text
}

struct Inner {
    /// The latest search for the executable.
    discovery: ExeDiscovery,
    /// The message while herdr is not found.
    message: Option<String>,
}

/// Whether the herdr executable is currently known to be missing, and the text
/// that says so. Shared by everything that reports status to the UI.
pub struct ExeMissing {
    inner: Mutex<Inner>,
    home: Option<PathBuf>,
}

impl ExeMissing {
    /// Not missing; `discovery` is the search the backend was built from.
    pub fn new(discovery: ExeDiscovery) -> Self {
        ExeMissing {
            inner: Mutex::new(Inner {
                discovery,
                message: None,
            }),
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .filter(|h| h.is_absolute()),
        }
    }

    /// The not-found message, while herdr is missing.
    pub fn message(&self) -> Option<String> {
        self.inner.lock().unwrap().message.clone()
    }

    /// Whether the latest search left the login-shell `PATH` for later (see
    /// [`ExeDiscovery::login_deferred`]).
    pub fn login_deferred(&self) -> bool {
        self.inner.lock().unwrap().discovery.login_deferred
    }

    /// Every location the latest search tried (for the log).
    pub fn searched(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().discovery.searched.clone()
    }

    /// herdr is missing. `discovery` replaces the remembered search when there
    /// is a newer one. Returns whether the reported state changed.
    pub fn set(&self, discovery: Option<ExeDiscovery>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(d) = discovery {
            inner.discovery = d;
        }
        let message = not_found_message(
            &inner.discovery.searched,
            &inner.discovery.notable,
            self.home.as_deref(),
        );
        let changed = inner.message.as_deref() != Some(message.as_str());
        inner.message = Some(message);
        changed
    }

    /// herdr is there. `discovery` replaces the remembered search when there is
    /// a newer one. Returns whether the reported state changed.
    pub fn clear(&self, discovery: Option<ExeDiscovery>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(d) = discovery {
            inner.discovery = d;
        }
        inner.message.take().is_some()
    }

    /// A newer search, with no herdr call made yet. One that found herdr
    /// clears the state. One that did not says nothing by itself (the bare
    /// name may still run): it replaces the remembered search, and a message
    /// already showing is rebuilt from it. Returns whether the reported state
    /// changed.
    pub fn searched_again(&self, discovery: ExeDiscovery) -> bool {
        if discovery.found {
            return self.clear(Some(discovery));
        }
        if self.message().is_some() {
            return self.set(Some(discovery));
        }
        self.inner.lock().unwrap().discovery = discovery;
        false
    }

    /// Learn from the result of one herdr call: a spawn that failed because the
    /// executable does not exist sets the state, a call that ran clears it, and
    /// any other failure says nothing either way. Returns whether the reported
    /// state changed.
    pub fn observe<T>(&self, result: &Result<T, HerdrError>) -> bool {
        match result {
            Ok(_) => self.clear(None),
            Err(e) if e.is_exe_missing() => self.set(None),
            // herdr ran and failed, or could not be run for another reason.
            Err(HerdrError::Exit { .. } | HerdrError::Api { .. } | HerdrError::Parse { .. }) => {
                self.clear(None)
            }
            Err(_) => false,
        }
    }

    /// `payload` as the UI should see it: unchanged while herdr is there,
    /// otherwise with the not-found message and [`NOT_FOUND_CODE`].
    pub fn overlay(&self, mut payload: RecoveryPayload) -> RecoveryPayload {
        if let Some(message) = self.message() {
            payload.reason = Some(message);
            payload.reason_code = Some(NOT_FOUND_CODE);
        }
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::recovery::RecoveryPhase;

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn missing_error() -> HerdrError {
        HerdrError::SpawnFailed {
            os_error: Some(2),
            message: "No such file or directory".into(),
        }
    }

    fn not_found(searched: &[&str], notable: &[&str]) -> ExeDiscovery {
        ExeDiscovery {
            exe: PathBuf::from("herdr"),
            searched: paths(searched),
            notable: paths(notable),
            found: false,
            explicit: false,
            login_deferred: false,
        }
    }

    const HINT: &str = " Install herdr, or set [pty] herdr_bin in keitora.toml to its full path, \
                        then press Restart.";

    #[test]
    fn message_names_the_notable_locations_and_counts_the_rest() {
        let searched = paths(&[
            "/Users/u/.cargo/bin/herdr",
            "/opt/homebrew/bin/herdr",
            "/opt/homebrew/sbin/herdr",
            "/usr/local/bin/herdr",
            "/usr/bin/herdr",
            "/bin/herdr",
            "/Users/u/.local/bin/herdr",
        ]);
        let notable = paths(&[
            "/opt/homebrew/bin/herdr",
            "/usr/local/bin/herdr",
            "/Users/u/.local/bin/herdr",
        ]);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            not_found_message(&searched, &notable, Some(Path::new("/Users/u"))),
            format!(
                "herdr was not found. Looked in /opt/homebrew/bin/herdr, /usr/local/bin/herdr, \
                 ~{sep}.local/bin/herdr and 4 other locations.{HINT}"
            )
        );
    }

    #[test]
    fn message_puts_a_configured_path_first() {
        let searched = paths(&["/cfg/herdr", "/a/herdr", "/opt/homebrew/bin/herdr"]);
        let notable = paths(&["/cfg/herdr", "/opt/homebrew/bin/herdr"]);
        assert_eq!(
            not_found_message(&searched, &notable, None),
            format!(
                "herdr was not found. Looked in /cfg/herdr, /opt/homebrew/bin/herdr and 1 other \
                 location.{HINT}"
            )
        );
    }

    #[test]
    fn message_lists_everything_when_it_is_short() {
        let both = paths(&["/opt/homebrew/bin/herdr", "/usr/local/bin/herdr"]);
        assert_eq!(
            not_found_message(&both, &both, None),
            format!(
                "herdr was not found. Looked in /opt/homebrew/bin/herdr and \
                 /usr/local/bin/herdr.{HINT}"
            )
        );
        let one = paths(&["/cfg/herdr"]);
        assert_eq!(
            not_found_message(&one, &one, None),
            format!("herdr was not found. Looked in /cfg/herdr.{HINT}")
        );
    }

    #[test]
    fn message_falls_back_to_the_first_searched_and_ignores_duplicates() {
        let searched = paths(&["/a/herdr", "/b/herdr", "/a/herdr", "/c/herdr", "/d/herdr"]);
        // A notable path that was never searched is not named.
        let notable = paths(&["/elsewhere/herdr"]);
        assert_eq!(
            not_found_message(&searched, &notable, None),
            format!(
                "herdr was not found. Looked in /a/herdr, /b/herdr, /c/herdr and 1 other \
                 location.{HINT}"
            )
        );
    }

    #[test]
    fn message_stays_short_however_long_the_search_was() {
        let searched: Vec<PathBuf> = (0..40)
            .map(|i| PathBuf::from(format!("/path/dir{i}/herdr")))
            .collect();
        let text = not_found_message(&searched, &searched, None);
        assert!(text.starts_with("herdr was not found. Looked in /path/dir0/herdr, "));
        assert!(text.contains("/path/dir4/herdr and 35 other locations."));
        assert!(!text.contains("/path/dir5/herdr"));
        assert!(text.len() < 300, "{} chars: {text}", text.len());
    }

    #[test]
    fn message_without_any_searched_location_says_so() {
        assert_eq!(
            not_found_message(&[], &[], None),
            format!("herdr was not found. It is not on the PATH.{HINT}")
        );
    }

    #[test]
    fn display_path_shortens_only_paths_under_home() {
        let home = Some(Path::new("/Users/u"));
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            display_path(Path::new("/Users/u/.local/bin/herdr"), home),
            format!("~{sep}.local/bin/herdr")
        );
        assert_eq!(display_path(Path::new("/Users/u"), home), "/Users/u");
        assert_eq!(
            display_path(Path::new("/Users/uu/bin/herdr"), home),
            "/Users/uu/bin/herdr"
        );
        assert_eq!(display_path(Path::new("/opt/herdr"), None), "/opt/herdr");
    }

    #[test]
    fn set_and_clear_report_whether_the_state_changed() {
        let state = ExeMissing::new(ExeDiscovery::explicit("/x/herdr".into()));
        assert_eq!(state.message(), None);
        assert!(!state.clear(None), "already clear");

        assert!(state.set(None));
        assert!(!state.set(None), "the same message again");
        assert!(state.message().unwrap().contains("/x/herdr"));

        // A newer search changes the text.
        assert!(state.set(Some(not_found(&["/y/herdr"], &["/y/herdr"]))));
        assert!(state.message().unwrap().contains("/y/herdr"));
        assert_eq!(state.searched(), paths(&["/y/herdr"]));

        assert!(state.clear(Some(ExeDiscovery::explicit("/z/herdr".into()))));
        assert_eq!(state.message(), None);
        assert_eq!(state.searched(), paths(&["/z/herdr"]));
    }

    #[test]
    fn searched_again_keeps_the_new_search_and_only_a_hit_clears() {
        let mut launch = not_found(&["/a/herdr"], &[]);
        launch.login_deferred = true;
        let state = ExeMissing::new(launch);
        assert!(state.login_deferred());

        // Still nothing, nothing showing: remembered, nothing to report.
        assert!(!state.searched_again(not_found(&["/a/herdr", "/login/herdr"], &[])));
        assert!(!state.login_deferred());
        assert_eq!(state.message(), None);
        assert_eq!(state.searched(), paths(&["/a/herdr", "/login/herdr"]));

        // A message already showing is rebuilt from the newer search.
        state.set(None);
        assert!(state.searched_again(not_found(&["/b/herdr"], &[])));
        assert!(state.message().unwrap().contains("/b/herdr"));
        assert!(!state.searched_again(not_found(&["/b/herdr"], &[])));

        // Found: the message goes.
        assert!(state.searched_again(ExeDiscovery::explicit("/z/herdr".into())));
        assert_eq!(state.message(), None);
        assert!(!state.searched_again(ExeDiscovery::explicit("/z/herdr".into())));
    }

    #[test]
    fn observe_follows_the_evidence_of_each_call() {
        let state = ExeMissing::new(not_found(&["/a/herdr"], &[]));
        // Failures that say nothing about the executable.
        assert!(!state.observe::<()>(&Err(HerdrError::Timeout)));
        assert!(!state.observe::<()>(&Err(HerdrError::SpawnFailed {
            os_error: Some(12),
            message: "out of memory".into(),
        })));
        assert_eq!(state.message(), None);

        assert!(state.observe::<()>(&Err(missing_error())));
        assert!(state.message().is_some());
        // Still unknown: a still-true message is kept.
        assert!(!state.observe::<()>(&Err(HerdrError::Timeout)));
        assert!(!state.observe::<()>(&Err(missing_error())));
        assert!(state.message().is_some());

        // herdr ran (and failed): the executable is there.
        assert!(state.observe::<()>(&Err(HerdrError::Exit {
            code: Some(1),
            stderr: "server not running".into(),
        })));
        assert_eq!(state.message(), None);

        state.set(None);
        assert!(state.observe(&Ok(())));
        assert_eq!(state.message(), None);
    }

    #[test]
    fn overlay_marks_the_payload_only_while_missing() {
        let state = ExeMissing::new(not_found(&["/a/herdr"], &[]));
        let mut payload = RecoveryPayload::healthy();
        payload.phase = RecoveryPhase::Paused;
        payload.reason = Some("herdr could not be started.".into());
        payload.reason_code = Some("start_failed");
        payload.seq = 7;

        assert_eq!(state.overlay(payload.clone()), payload);

        state.set(None);
        let shown = state.overlay(payload.clone());
        assert_eq!(shown.reason, state.message());
        assert_eq!(shown.reason_code, Some(NOT_FOUND_CODE));
        // Nothing else about the controller's state is touched.
        assert_eq!(
            (shown.phase, shown.seq),
            (RecoveryPhase::Paused, payload.seq)
        );

        state.clear(None);
        assert_eq!(state.overlay(payload.clone()), payload);
    }
}
