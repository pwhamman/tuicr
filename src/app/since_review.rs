//! The "changed since you reviewed it" view of the inline commit pane.

use super::*;
use crate::review_status::patch_keys;
use crate::review_store::ReviewStore;
use crate::since_review::{self as pairing, Comparison, SinceReview};

/// Pairing of the commits under review with their last reviewed versions,
/// computed once per set of commits.
#[derive(Debug, Default)]
pub struct SinceReviewState {
    key: Vec<String>,
    comparison: Option<Comparison>,
    statuses: HashMap<String, SinceReview>,
}

impl App {
    /// Commits of the review, oldest first, when the since-review view applies.
    fn since_review_commit_ids(&self) -> Option<Vec<String>> {
        let local = matches!(
            self.diff_source,
            DiffSource::CommitRange(_) | DiffSource::StagedUnstagedAndCommits(_)
        );
        if !local || self.vcs_info.vcs_type != crate::vcs::traits::VcsType::Git {
            return None;
        }
        let ids: Vec<String> = self
            .review_commits
            .iter()
            .rev()
            .filter(|commit| !Self::is_special_commit(commit))
            .map(|commit| commit.id.clone())
            .collect();
        (!ids.is_empty()).then_some(ids)
    }

    /// Recomputes the pairing when the commits under review changed.
    /// Returns true when the state changed and the screen needs a redraw.
    pub fn ensure_since_review(&mut self) -> bool {
        let ids = self.since_review_commit_ids().unwrap_or_default();
        if ids == self.since_review.key {
            return false;
        }
        let comparison = self.compare_with_previous_review(&ids);
        let statuses = match &comparison {
            Some(comparison) => self.since_review_statuses(comparison, &ids),
            None => HashMap::new(),
        };
        self.since_review = SinceReviewState {
            key: ids,
            comparison,
            statuses,
        };
        true
    }

    fn compare_with_previous_review(&self, ids: &[String]) -> Option<Comparison> {
        if ids.is_empty() {
            return None;
        }
        let root = &self.vcs_info.root_path;
        let branch = self.vcs_info.branch_name.as_deref();
        let own_range = self.session.commit_range.clone().unwrap_or(ids.to_vec());
        let previous = ReviewStore::new()
            .previous_local_session(root, branch, self.session.diff_source, &own_range)
            .ok()??;
        pairing::compare(root, &previous.commit_range?, branch, ids)
    }

    fn since_review_statuses(
        &self,
        comparison: &Comparison,
        ids: &[String],
    ) -> HashMap<String, SinceReview> {
        let vcs = self.vcs.as_ref();
        let mut patches_of = |sha: &str| patch_keys(vcs, sha);
        ids.iter()
            .map(|id| (id.clone(), comparison.status(id, &mut patches_of)))
            .collect()
    }

    /// How the commit at `index` in `review_commits` compares with its reviewed version.
    pub fn commit_since_review(&self, index: usize) -> Option<SinceReview> {
        let commit = self.review_commits.get(index)?;
        self.since_review.statuses.get(&commit.id).copied()
    }

    /// The diff pane shows range-diff entries instead of commit patches.
    pub fn is_showing_since_review(&self) -> bool {
        self.since_review_showing
            && !self.diff_files.is_empty()
            && self.diff_files.iter().all(pairing::is_view_file)
    }

    /// The commit the toggle acts on: the one under the commit-list cursor.
    fn since_review_target(&self) -> Option<usize> {
        let index = self.commit_list_cursor;
        let commit = self.review_commits.get(index)?;
        (!Self::is_special_commit(commit)).then_some(index)
    }

    /// Toggles the diff pane of the current commit between the full commit diff
    /// and what changed since the reviewed version.
    pub fn toggle_since_review_view(&mut self) {
        self.ensure_since_review();
        let cursor = self.commit_list_cursor;
        if self.since_review_view && self.commit_selection_range == Some((cursor, cursor)) {
            self.since_review_view = false;
            self.reload_since_review_selection();
            return;
        }
        let Some(index) = self.since_review_target() else {
            self.set_message("Changes since review need a local commit review");
            return;
        };
        if let Some(reason) = self.since_review_blocker(index) {
            self.set_message(reason);
            return;
        }
        self.since_review_view = true;
        self.commit_selection_range = Some((index, index));
        self.commit_list_cursor = index;
        self.reload_since_review_selection();
    }

    fn since_review_blocker(&self, index: usize) -> Option<&'static str> {
        let Some(comparison) = &self.since_review.comparison else {
            return Some("No earlier review to compare with");
        };
        let sha = &self.review_commits[index].id;
        match comparison.pairs.get(sha) {
            None => Some("New commit: nothing to compare with"),
            Some(pair) if pair.entries.is_empty() => Some("Unchanged since review"),
            Some(_) => None,
        }
    }

    fn reload_since_review_selection(&mut self) {
        if let Err(e) = self.reload_inline_selection_for_source() {
            self.set_error(format!("Failed to load diff: {e}"));
        }
    }

    /// Range-diff entries of the selected commit as diff files, when the
    /// since-review view is on and a single commit is selected.
    pub(in crate::app) fn since_review_files(
        &self,
        start: usize,
        end: usize,
    ) -> Option<Vec<DiffFile>> {
        if !self.since_review_view || start != end {
            return None;
        }
        let sha = &self.review_commits.get(start)?.id;
        let pair = self.since_review.comparison.as_ref()?.pairs.get(sha)?;
        let files: Vec<DiffFile> = pair.entries.iter().map(|e| e.to_diff_file()).collect();
        (!files.is_empty()).then_some(files)
    }

    /// Comments need a line of the repository, which range-diff lines are not.
    pub fn deny_comment_on_since_review(&mut self) -> bool {
        let showing = self.is_showing_since_review();
        if showing {
            self.set_message("Comments are not allowed on since-review lines");
        }
        showing
    }
}
