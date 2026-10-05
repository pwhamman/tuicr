//! `tuicr review status`: which files of a commit carry a review mark.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::app::App;
use crate::error::{Result, TuicrError};
use crate::model::DiffFile;
use crate::model::SessionDiffSource;
use crate::model::review::ReviewedPatch;
use crate::review_store::ReviewStore;
use crate::since_review::{self, Comparison, SinceReview};
use crate::syntax::SyntaxHighlighter;
use crate::vcs::{DiffWhitespaceMode, GitBackend, GitBackendPreference};
use crate::vcs::{ResolvedRevisionRange, RevisionDiffTarget, VcsBackend};

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FileStatusOutput {
    pub path: String,
    pub content_hash: u64,
    pub reviewed: bool,
    pub added: usize,
    pub deleted: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CommitStatusOutput {
    pub sha: String,
    pub reviewed: bool,
    /// How the commit's patch compares with the version last reviewed.
    /// `None` when there is no earlier review to compare with.
    pub since_review: Option<SinceReview>,
    pub files: Vec<FileStatusOutput>,
}

/// Report the review state of each commit's diff against its first parent.
///
/// The diff is loaded the way the inline commit selector loads a single
/// commit (`CommitList` target plus `.tuicrignore`), so each `content_hash`
/// equals the one the TUI computes for that commit viewed alone.
pub fn commit_statuses(
    store: &ReviewStore,
    repo: &Path,
    commits: &[String],
    preference: GitBackendPreference,
    whitespace: DiffWhitespaceMode,
) -> Result<Vec<CommitStatusOutput>> {
    let vcs = GitBackend::discover_from(repo, preference, whitespace)?;
    let marks = collect_marks(store, repo)?;
    let mut statuses = commits
        .iter()
        .map(|rev| commit_status(&vcs, rev, &marks))
        .collect::<Result<Vec<_>>>()?;
    let shas: Vec<String> = statuses.iter().map(|status| status.sha.clone()).collect();
    if let Some(comparison) = compare_with_previous_review(store, &vcs, &shas) {
        for status in &mut statuses {
            let mut patches_of = |sha: &str| patch_keys(&vcs, sha);
            status.since_review = Some(comparison.status(&status.sha, &mut patches_of));
        }
    }
    Ok(statuses)
}

fn compare_with_previous_review(
    store: &ReviewStore,
    vcs: &dyn VcsBackend,
    shas: &[String],
) -> Option<Comparison> {
    let info = vcs.info();
    let ordered = since_review::oldest_first(&info.root_path, shas);
    let previous = store
        .previous_local_session(
            &info.root_path,
            info.branch_name.as_deref(),
            SessionDiffSource::CommitRange,
            &ordered,
        )
        .ok()??;
    since_review::compare(
        &info.root_path,
        &previous.commit_range?,
        info.branch_name.as_deref(),
        &ordered,
    )
}

/// The review-mark keys of a commit's patch: what inheritance compares.
pub(crate) fn patch_keys(vcs: &dyn VcsBackend, sha: &str) -> BTreeSet<ReviewedPatch> {
    load_commit_files(vcs, sha)
        .unwrap_or_default()
        .iter()
        .map(|file| ReviewedPatch {
            path: file.display_path().clone(),
            content_hash: file.content_hash,
        })
        .collect()
}

fn collect_marks(store: &ReviewStore, repo: &Path) -> Result<BTreeSet<ReviewedPatch>> {
    let mut marks = BTreeSet::new();
    for summary in store.list_sessions_for_repo(repo)? {
        marks.extend(store.get_review(&summary.session_ref)?.reviewed_patches);
    }
    Ok(marks)
}

fn commit_status(
    vcs: &dyn VcsBackend,
    rev: &str,
    marks: &BTreeSet<ReviewedPatch>,
) -> Result<CommitStatusOutput> {
    let sha = resolve_commit(vcs, rev)?;
    let files: Vec<FileStatusOutput> = load_commit_files(vcs, &sha)?
        .iter()
        .map(|file| file_status(file, marks))
        .collect();
    Ok(CommitStatusOutput {
        sha,
        reviewed: files.iter().all(|file| file.reviewed),
        since_review: None,
        files,
    })
}

fn resolve_commit(vcs: &dyn VcsBackend, rev: &str) -> Result<String> {
    let range = vcs
        .resolve_revision_range(rev)
        .map_err(|_| TuicrError::InvalidInput(format!("unknown commit: {rev}")))?;
    match range.commit_ids.as_ref() {
        [sha] => Ok(sha.clone()),
        _ => Err(TuicrError::InvalidInput(format!(
            "not a single commit: {rev}"
        ))),
    }
}

fn load_commit_files(vcs: &dyn VcsBackend, sha: &str) -> Result<Vec<DiffFile>> {
    let ids = [sha.to_string()];
    let range = ResolvedRevisionRange::from_commit_ids(&ids, RevisionDiffTarget::CommitList);
    match App::get_commit_range_diff_with_ignore(
        vcs,
        &vcs.info().root_path,
        &range,
        &SyntaxHighlighter::default(),
        None,
    ) {
        Err(TuicrError::NoChanges) => Ok(Vec::new()),
        other => other,
    }
}

fn file_status(file: &DiffFile, marks: &BTreeSet<ReviewedPatch>) -> FileStatusOutput {
    let path = file.display_path().clone();
    let (added, deleted) = file.stat();
    let reviewed = marks.contains(&ReviewedPatch {
        path: path.clone(),
        content_hash: file.content_hash,
    });
    FileStatusOutput {
        path: path.display().to_string(),
        content_hash: file.content_hash,
        reviewed,
        added,
        deleted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReviewSession, SessionDiffSource};
    use crate::since_review::test_repo::ReviewRepo;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::{TempDir, tempdir};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Repo with two commits that both touch `a.txt`.
    fn two_commit_repo() -> (TempDir, String, String) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "T"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
        let first = git(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(root, &["commit", "-q", "-am", "second"]);
        let second = git(root, &["rev-parse", "HEAD"]);
        (dir, first, second)
    }

    fn statuses(store: &ReviewStore, repo: &Path, shas: &[&str]) -> Vec<CommitStatusOutput> {
        let shas: Vec<String> = shas.iter().map(|s| s.to_string()).collect();
        commit_statuses(
            store,
            repo,
            &shas,
            GitBackendPreference::Libgit2,
            DiffWhitespaceMode::Normal,
        )
        .unwrap()
    }

    fn session_marking(repo: &Path, branch: &str, status: &CommitStatusOutput) -> ReviewSession {
        let mut session = ReviewSession::new(
            repo.to_path_buf(),
            status.sha.clone(),
            Some(branch.to_string()),
            SessionDiffSource::CommitRange,
        );
        session.commit_range = Some(vec![status.sha.clone()]);
        session
            .reviewed_patches
            .extend(status.files.iter().map(|f| ReviewedPatch {
                path: PathBuf::from(&f.path),
                content_hash: f.content_hash,
            }));
        session
    }

    #[test]
    fn should_report_unreviewed_commits_without_marks() {
        let (repo, first, second) = two_commit_repo();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());

        let out = statuses(&store, repo.path(), &[&first, &second]);

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sha, first);
        assert!(!out[0].reviewed);
        assert_eq!(out[0].files.len(), 1);
        assert_eq!(out[0].files[0].path, "a.txt");
        assert_eq!((out[0].files[0].added, out[0].files[0].deleted), (1, 0));
        assert!(!out[1].reviewed);
        assert_eq!((out[1].files[0].added, out[1].files[0].deleted), (2, 0));
    }

    #[test]
    fn should_mark_only_the_commit_whose_patch_was_reviewed() {
        let (repo, first, second) = two_commit_repo();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());
        let before = statuses(&store, repo.path(), &[&first]);
        store
            .save_review(&session_marking(repo.path(), "main", &before[0]))
            .unwrap();

        let out = statuses(&store, repo.path(), &[&first[..7], &second]);

        assert_eq!(out[0].sha, first);
        assert!(out[0].reviewed);
        assert!(out[0].files[0].reviewed);
        assert!(!out[1].reviewed);
        assert!(!out[1].files[0].reviewed);
    }

    #[test]
    fn should_find_marks_saved_by_a_session_on_another_branch() {
        let (repo, first, second) = two_commit_repo();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());
        let before = statuses(&store, repo.path(), &[&second]);
        git(repo.path(), &["checkout", "-q", "-b", "feature/x"]);
        store
            .save_review(&session_marking(repo.path(), "feature/x", &before[0]))
            .unwrap();
        git(repo.path(), &["checkout", "-q", "main"]);

        let out = statuses(&store, repo.path(), &[&first, &second]);

        assert!(!out[0].reviewed);
        assert!(out[1].reviewed);
    }

    #[test]
    fn should_error_for_an_unknown_sha() {
        let (repo, _, _) = two_commit_repo();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());

        let err = commit_statuses(
            &store,
            repo.path(),
            &["deadbeef".to_string()],
            GitBackendPreference::Libgit2,
            DiffWhitespaceMode::Normal,
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "Invalid input: unknown commit: deadbeef");
    }

    #[test]
    fn should_leave_out_files_matched_by_tuicrignore() {
        let (repo, _, _) = two_commit_repo();
        std::fs::write(repo.path().join("b.log"), "x\n").unwrap();
        std::fs::write(repo.path().join(".tuicrignore"), "*.log\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-q", "-m", "third"]);
        let head = git(repo.path(), &["rev-parse", "HEAD"]);
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());

        let out = statuses(&store, repo.path(), &[&head]);

        assert_eq!(out[0].files.len(), 1);
        assert_eq!(out[0].files[0].path, ".tuicrignore");
    }

    fn feat_session(repo: &Path, range: &[&String]) -> ReviewSession {
        let mut session = ReviewSession::new(
            repo.to_path_buf(),
            range.last().unwrap().to_string(),
            Some("feat".to_string()),
            SessionDiffSource::CommitRange,
        );
        session.commit_range = Some(range.iter().map(|sha| sha.to_string()).collect());
        session
    }

    #[test]
    fn should_report_unchanged_changed_and_new_commits_since_the_reviewed_range() {
        let repo = ReviewRepo::new();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());
        store
            .save_review(&feat_session(repo.path(), &[&repo.a, &repo.b]))
            .unwrap();
        let (b2, c) = repo.amend_b_and_add_c();

        let out = statuses(&store, repo.path(), &[&repo.a, &b2, &c]);

        let since: Vec<Option<SinceReview>> = out.iter().map(|s| s.since_review).collect();
        assert_eq!(
            since,
            [
                Some(SinceReview::Unchanged),
                Some(SinceReview::Changed),
                Some(SinceReview::New)
            ]
        );
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json[0]["since_review"], "unchanged");
        assert_eq!(json[1]["since_review"], "changed");
        assert_eq!(json[2]["since_review"], "new");
    }

    #[test]
    fn should_report_null_since_review_without_an_earlier_session() {
        let repo = ReviewRepo::new();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());

        let out = statuses(&store, repo.path(), &[&repo.a, &repo.b]);

        assert_eq!(out[0].since_review, None);
        let json = serde_json::to_value(&out).unwrap();
        assert!(json[1]["since_review"].is_null());
    }

    #[test]
    fn should_compare_with_the_pushed_branch_when_the_reviewed_commits_are_gone() {
        let repo = ReviewRepo::new();
        let _origin = repo.push_to_origin();
        let reviews = tempdir().unwrap();
        let store = ReviewStore::with_reviews_dir(reviews.path());
        let gone = "1".repeat(40);
        store
            .save_review(&feat_session(repo.path(), &[&gone]))
            .unwrap();
        let (b2, _) = repo.amend_b_and_add_c();

        let out = statuses(&store, repo.path(), &[&repo.a, &b2]);

        assert_eq!(out[0].since_review, Some(SinceReview::Unchanged));
        assert_eq!(out[1].since_review, Some(SinceReview::Changed));
    }
}
