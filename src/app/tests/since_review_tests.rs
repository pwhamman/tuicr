use crate::app::*;
use crate::since_review::SinceReview;
use crate::since_review::test_repo::ReviewRepo;
use crate::vcs::{DiffWhitespaceMode, GitBackend, GitBackendPreference};

fn app_over(repo: &Path, ids: &[String]) -> App {
    let vcs = GitBackend::discover_from(
        repo,
        GitBackendPreference::Libgit2,
        DiffWhitespaceMode::default(),
    )
    .unwrap();
    let info = vcs.info().clone();
    let commits = vcs.get_commits_info(ids).unwrap();
    let session = App::load_or_create_commit_range_session(&info, ids);
    let mut app = App::build(
        Box::new(vcs),
        info,
        Theme::dark(),
        None,
        false,
        Vec::new(),
        session,
        DiffSource::CommitRange(ids.to_vec()),
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .unwrap();
    app.review_commits = commits.into_iter().rev().collect();
    app.commit_list = app.review_commits.clone();
    app.commit_selection_range = Some((0, ids.len() - 1));
    app.show_commit_selector = true;
    app.reload_inline_selection().unwrap();
    app
}

fn paths(app: &App) -> Vec<String> {
    app.diff_files
        .iter()
        .map(|f| f.display_path().display().to_string())
        .collect()
}

fn message(app: &App) -> String {
    app.message.as_ref().unwrap().content.clone()
}

/// A reviewed app over A..B, then a second app after amending B and adding C.
/// Commit rows are newest first: C, B', A.
fn amended_review() -> (ReviewRepo, App) {
    let repo = ReviewRepo::new();
    let first = app_over(repo.path(), &[repo.a.clone(), repo.b.clone()]);
    crate::persistence::save_session(&first.session).unwrap();
    let (b2, c) = repo.amend_b_and_add_c();
    let mut app = app_over(repo.path(), &[repo.a.clone(), b2, c]);
    assert!(app.ensure_since_review());
    (repo, app)
}

#[test]
fn should_mark_each_commit_row_as_unchanged_changed_or_new() {
    let (_repo, app) = amended_review();

    let marks: Vec<Option<SinceReview>> = (0..3).map(|i| app.commit_since_review(i)).collect();

    assert_eq!(
        marks,
        [
            Some(SinceReview::New),
            Some(SinceReview::Changed),
            Some(SinceReview::Unchanged)
        ]
    );
}

#[test]
fn should_toggle_one_commit_between_its_full_diff_and_the_since_review_diff() {
    let (_repo, mut app) = amended_review();
    app.commit_list_cursor = 1;

    app.toggle_since_review_view();

    assert_eq!(paths(&app), ["b.txt (since review)"]);
    assert!(app.is_showing_since_review());
    assert_eq!(app.commit_selection_range, Some((1, 1)));
    assert!(
        !app.session
            .files
            .contains_key(Path::new("b.txt (since review)"))
    );

    app.toggle_since_review_view();

    assert!(!app.is_showing_since_review());
    assert_eq!(paths(&app)[1], "b.txt");
}

#[test]
fn should_say_why_an_unchanged_or_new_commit_has_no_since_review_diff() {
    let (_repo, mut app) = amended_review();

    app.commit_list_cursor = 2;
    app.toggle_since_review_view();
    assert_eq!(message(&app), "Unchanged since review");

    app.commit_list_cursor = 0;
    app.toggle_since_review_view();
    assert_eq!(message(&app), "New commit: nothing to compare with");
    assert!(!app.since_review_view);
}

#[test]
fn should_refuse_comments_on_since_review_lines() {
    let (_repo, mut app) = amended_review();
    app.commit_list_cursor = 1;
    app.toggle_since_review_view();

    app.enter_comment_mode(false, Some((1, LineSide::New)));

    assert_eq!(app.input_mode, InputMode::Normal);
    assert_eq!(
        message(&app),
        "Comments are not allowed on since-review lines"
    );
}

#[test]
fn should_say_there_is_no_earlier_review_without_a_previous_session() {
    let repo = ReviewRepo::new();
    let mut app = app_over(repo.path(), &[repo.a.clone(), repo.b.clone()]);
    app.ensure_since_review();
    app.commit_list_cursor = 0;

    app.toggle_since_review_view();

    assert_eq!(message(&app), "No earlier review to compare with");
    assert_eq!(app.commit_since_review(0), None);
}

#[test]
fn should_retarget_the_view_to_the_cursor_commit_instead_of_switching_off() {
    let (_repo, mut app) = amended_review();
    app.commit_list_cursor = 1;
    app.toggle_since_review_view();

    app.commit_list_cursor = 2;
    app.toggle_since_review_view();

    assert_eq!(message(&app), "Unchanged since review");
    assert!(app.since_review_view);
    assert_eq!(paths(&app), ["b.txt (since review)"]);
}
