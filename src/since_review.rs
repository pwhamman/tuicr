//! "Changed since you reviewed it": pairs the commits under review with the
//! versions the reviewer last saw, the way `git range-diff` does.
//!
//! The baseline is the previous session's commit range (the session review
//! marks are inherited from). When those commits are gone after a force-push
//! they are fetched from `origin`, and if `origin` no longer has them either,
//! the pushed branch `origin/<branch>` stands in.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;

use crate::model::review::ReviewedPatch;
use crate::model::{DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin};
use crate::process::run_command_output;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MESSAGE_LABEL: &str = "Commit message";
const VIEW_SUFFIX: &str = " (since review)";

/// How a commit's patch compares with the version that was reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SinceReview {
    Unchanged,
    Changed,
    New,
}

impl SinceReview {
    pub fn label(self) -> &'static str {
        match self {
            SinceReview::Unchanged => "unchanged",
            SinceReview::Changed => "changed since review",
            SinceReview::New => "new",
        }
    }
}

/// One section of a `git range-diff` body: a file, or the commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeEntry {
    pub label: String,
    pub is_message: bool,
    /// Lines as range-diff prints them minus the 4-space indent: a column for
    /// the diff-of-diff (`+`, `-` or space), then the original line.
    pub lines: Vec<String>,
}

/// The reviewed version of a commit and what changed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPair {
    pub previous: String,
    pub entries: Vec<RangeEntry>,
}

/// The old range the new one is compared against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub base: String,
    pub head: String,
    /// True when the pushed branch stands in for the reviewed commits.
    pub pushed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub baseline: Baseline,
    /// Keyed by the full sha of the commit in the new range.
    pub pairs: HashMap<String, CommitPair>,
}

impl Comparison {
    pub fn status(
        &self,
        sha: &str,
        patches_of: &mut dyn FnMut(&str) -> BTreeSet<ReviewedPatch>,
    ) -> SinceReview {
        let Some(pair) = self.pairs.get(sha) else {
            return SinceReview::New;
        };
        if pair.previous == sha || patches_of(sha) == patches_of(&pair.previous) {
            SinceReview::Unchanged
        } else {
            SinceReview::Changed
        }
    }
}

impl RangeEntry {
    pub fn to_diff_file(&self) -> DiffFile {
        let lines: Vec<DiffLine> = self.lines.iter().map(|line| diff_line(line)).collect();
        let count = lines.len() as u32;
        let hunks = vec![DiffHunk {
            header: String::new(),
            lines,
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: count,
        }];
        let content_hash = DiffFile::compute_content_hash(&hunks);
        DiffFile {
            old_path: None,
            new_path: Some(PathBuf::from(format!("{}{VIEW_SUFFIX}", self.label))),
            status: FileStatus::Modified,
            hunks,
            is_binary: false,
            is_too_large: false,
            is_commit_message: false,
            content_hash,
        }
    }
}

/// Whether `file` is a range-diff entry rather than a patch of the repository.
pub fn is_view_file(file: &DiffFile) -> bool {
    file.display_path().to_string_lossy().ends_with(VIEW_SUFFIX)
}

fn diff_line(raw: &str) -> DiffLine {
    let mut chars = raw.chars();
    let origin = match chars.next() {
        Some('+') => LineOrigin::Addition,
        Some('-') => LineOrigin::Deletion,
        _ => LineOrigin::Context,
    };
    DiffLine {
        origin,
        content: chars.as_str().to_string(),
        old_lineno: None,
        new_lineno: None,
        highlighted_spans: None,
    }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = run_command_output("git", Some(repo), args.iter()).ok()?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

fn commit_exists(repo: &Path, sha: &str) -> bool {
    let spec = format!("{sha}^{{commit}}");
    git(repo, &["rev-parse", "--verify", "--quiet", &spec]).is_some()
}

fn full_sha(repo: &Path, short: &str) -> Option<String> {
    let spec = format!("{short}^{{commit}}");
    git(repo, &["rev-parse", "--verify", "--quiet", &spec])
}

/// Sorts commits oldest first by how much history each one has.
pub fn oldest_first(repo: &Path, commits: &[String]) -> Vec<String> {
    let mut keyed: Vec<(u64, &String)> = commits
        .iter()
        .map(|sha| {
            let count = git(repo, &["rev-list", "--count", sha]);
            (count.and_then(|n| n.parse().ok()).unwrap_or(0), sha)
        })
        .collect();
    keyed.sort_by_key(|(count, _)| *count);
    keyed.into_iter().map(|(_, sha)| sha.clone()).collect()
}

/// Best-effort `git fetch origin <shas>`. Never prompts, gives up after a timeout.
fn fetch_commits(repo: &Path, shas: &[&String]) {
    let child = Command::new("git")
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["fetch", "--quiet", "--no-tags", "origin"])
        .args(shas)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return };
    let started = Instant::now();
    while matches!(child.try_wait(), Ok(None)) {
        if started.elapsed() > FETCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn session_baseline(repo: &Path, old_commits: &[String]) -> Option<Baseline> {
    if old_commits.is_empty() {
        return None;
    }
    let missing: Vec<&String> = old_commits
        .iter()
        .filter(|sha| !commit_exists(repo, sha))
        .collect();
    if !missing.is_empty() {
        fetch_commits(repo, &missing);
    }
    if !old_commits.iter().all(|sha| commit_exists(repo, sha)) {
        return None;
    }
    let ordered = oldest_first(repo, old_commits);
    let first = ordered.first()?;
    let base = full_sha(repo, &format!("{first}^"))?;
    let head = full_sha(repo, ordered.last()?)?;
    Some(Baseline {
        base,
        head,
        pushed: false,
    })
}

fn pushed_baseline(repo: &Path, branch: &str, new_base: &str) -> Option<Baseline> {
    let remote = format!("origin/{branch}");
    let head = full_sha(repo, &remote)?;
    let base = git(repo, &["merge-base", &head, new_base])?;
    Some(Baseline {
        base,
        head,
        pushed: true,
    })
}

/// Finds the old range: the previous session's commits, else `origin/<branch>`.
pub fn resolve_baseline(
    repo: &Path,
    old_commits: &[String],
    branch: Option<&str>,
    new_base: &str,
) -> Option<Baseline> {
    session_baseline(repo, old_commits).or_else(|| pushed_baseline(repo, branch?, new_base))
}

/// Compares `new_commits` (oldest first) with the reviewed range.
pub fn compare(
    repo: &Path,
    old_commits: &[String],
    branch: Option<&str>,
    new_commits: &[String],
) -> Option<Comparison> {
    let new_base = full_sha(repo, &format!("{}^", new_commits.first()?))?;
    let new_head = full_sha(repo, new_commits.last()?)?;
    let baseline = resolve_baseline(repo, old_commits, branch, &new_base)?;
    let old_range = format!("{}..{}", baseline.base, baseline.head);
    let new_range = format!("{new_base}..{new_head}");
    let out = run_command_output(
        "git",
        Some(repo),
        ["range-diff", "--no-color", &old_range, &new_range].iter(),
    )
    .ok()?;
    let pairs = parse_range_diff(&out, &|short| full_sha(repo, short));
    Some(Comparison { baseline, pairs })
}

fn pair_header() -> &'static Regex {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    HEADER.get_or_init(|| {
        Regex::new(r"^\s*(?:\d+|-+):\s+([0-9a-f]+|-+)\s+([<>=!])\s+(?:\d+|-+):\s+([0-9a-f]+|-+)\s")
            .expect("range-diff header pattern")
    })
}

struct OpenPair<'a> {
    new: String,
    previous: String,
    body: Vec<&'a str>,
}

/// Parses `git range-diff` output into pairs keyed by the new commit's full sha.
/// Commits that exist on one side only produce no pair.
pub fn parse_range_diff(
    out: &str,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> HashMap<String, CommitPair> {
    let mut pairs = HashMap::new();
    let mut open: Option<OpenPair> = None;
    for line in out.lines() {
        if let Some(header) = pair_header().captures(line) {
            close_pair(&mut pairs, open.take());
            open = pair_start(&header, resolve);
        } else if let Some(pair) = open.as_mut() {
            pair.body.push(line);
        }
    }
    close_pair(&mut pairs, open.take());
    pairs
}

fn pair_start<'a>(
    header: &regex::Captures<'_>,
    resolve: &dyn Fn(&str) -> Option<String>,
) -> Option<OpenPair<'a>> {
    if !matches!(&header[2], "=" | "!") {
        return None;
    }
    Some(OpenPair {
        new: resolve(&header[3])?,
        previous: resolve(&header[1])?,
        body: Vec::new(),
    })
}

fn close_pair(pairs: &mut HashMap<String, CommitPair>, open: Option<OpenPair<'_>>) {
    let Some(open) = open else {
        return;
    };
    pairs.insert(
        open.new,
        CommitPair {
            previous: open.previous,
            entries: split_entries(&open.body),
        },
    );
}

/// Splits one commit's range-diff body into a message entry and one entry per file.
pub fn split_entries(body: &[&str]) -> Vec<RangeEntry> {
    let mut entries: Vec<RangeEntry> = Vec::new();
    let mut current = entry_index(&mut entries, MESSAGE_LABEL);
    for line in body {
        let rest = line.strip_prefix("    ").unwrap_or(line.trim_start());
        if let Some(label) = rest.strip_prefix("@@") {
            if let Some(label) = hunk_label(label) {
                current = entry_index(&mut entries, &label);
            }
        } else if let Some(label) = section_label(rest.get(1..).unwrap_or("")) {
            current = entry_index(&mut entries, &label);
        } else {
            entries[current].lines.push(rest.to_string());
        }
    }
    entries.retain(|entry| !entry.lines.is_empty());
    entries
}

fn entry_index(entries: &mut Vec<RangeEntry>, label: &str) -> usize {
    let label = if label == "Metadata" {
        MESSAGE_LABEL
    } else {
        label
    };
    if let Some(index) = entries.iter().position(|e| e.label == label) {
        return index;
    }
    entries.push(RangeEntry {
        label: label.to_string(),
        is_message: label == MESSAGE_LABEL,
        lines: Vec::new(),
    });
    entries.len() - 1
}

/// `@@ src/a.ts (new)` or `@@ src/a.ts: function foo` name the section a hunk starts in.
fn hunk_label(after_at: &str) -> Option<String> {
    let label = after_at.trim();
    let label = label.split_once(": ").map_or(label, |(path, _)| path);
    let label = strip_status(label);
    (!label.is_empty()).then(|| label.to_string())
}

/// ` ## src/a.ts (new) ##` heads each file's section inside a hunk.
fn section_label(content: &str) -> Option<String> {
    let inner = content.trim().strip_prefix("## ")?.strip_suffix(" ##")?;
    Some(strip_status(inner).to_string())
}

fn strip_status(label: &str) -> &str {
    match label.rfind(" (") {
        Some(at) if label.ends_with(')') => &label[..at],
        _ => label,
    }
}

#[cfg(test)]
pub(crate) mod test_repo {
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    pub fn git(dir: &Path, args: &[&str]) -> String {
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

    fn numbered(prefix: &str) -> String {
        (1..=12).map(|n| format!("{prefix}{n}\n")).collect()
    }

    /// `main` with a base commit, and `feat` with commits A (a.txt) and B (b.txt).
    pub struct ReviewRepo {
        pub dir: TempDir,
        pub a: String,
        pub b: String,
    }

    impl ReviewRepo {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            git(root, &["init", "-q", "-b", "main"]);
            git(root, &["config", "user.email", "t@example.com"]);
            git(root, &["config", "user.name", "T"]);
            std::fs::write(root.join("base.txt"), "base\n").unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "base"]);
            git(root, &["checkout", "-q", "-b", "feat"]);
            std::fs::write(root.join("a.txt"), numbered("a")).unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "add a"]);
            let a = git(root, &["rev-parse", "HEAD"]);
            std::fs::write(root.join("b.txt"), numbered("b")).unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "add b"]);
            let b = git(root, &["rev-parse", "HEAD"]);
            Self { dir, a, b }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Amends B (one line of b.txt) and adds C. Returns the new B and C.
        pub fn amend_b_and_add_c(&self) -> (String, String) {
            let root = self.path();
            let edited = numbered("b").replace("b3\n", "b3 fixed\n");
            std::fs::write(root.join("b.txt"), edited).unwrap();
            git(root, &["commit", "-q", "-a", "--amend", "--no-edit"]);
            let b = git(root, &["rev-parse", "HEAD"]);
            std::fs::write(root.join("c.txt"), "c\n").unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "add c"]);
            (b, git(root, &["rev-parse", "HEAD"]))
        }

        /// Adds a bare `origin` and pushes `feat` to it.
        pub fn push_to_origin(&self) -> TempDir {
            let origin = tempfile::tempdir().unwrap();
            git(origin.path(), &["init", "-q", "--bare"]);
            let url = origin.path().to_str().unwrap();
            git(self.path(), &["remote", "add", "origin", url]);
            git(self.path(), &["push", "-q", "origin", "feat"]);
            origin
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE_DIFF: &str = "\
1:  456f136 = 1:  456f136 add four
2:  102a21d ! 2:  5fca649 add b
    @@ Metadata
     Author: t <t@e>
     
      ## Commit message ##
    -    add b
    +    add b v2
    +
    +    body
     
      ## b.txt (new) ##
     @@
     +x
    -+y
    ++Y
     +z
3:  1111111 < -:  ------- gone
-:  ------- > 4:  9999999 brand new
";

    fn identity(short: &str) -> Option<String> {
        Some(short.to_string())
    }

    #[test]
    fn should_split_a_range_diff_into_a_message_entry_and_one_entry_per_file() {
        let pairs = parse_range_diff(RANGE_DIFF, &identity);

        let pair = &pairs["5fca649"];
        assert_eq!(pair.previous, "102a21d");
        let labels: Vec<&str> = pair.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Commit message", "b.txt"]);
        assert!(pair.entries[0].is_message);
        assert!(!pair.entries[1].is_message);
        assert_eq!(
            pair.entries[0].lines,
            [
                " Author: t <t@e>",
                " ",
                "-    add b",
                "+    add b v2",
                "+",
                "+    body",
                " ",
            ]
        );
        assert_eq!(pair.entries[1].lines, [" @@", " +x", "-+y", "++Y", " +z"]);
    }

    #[test]
    fn should_pair_unchanged_commits_without_entries_and_skip_one_sided_commits() {
        let pairs = parse_range_diff(RANGE_DIFF, &identity);

        assert_eq!(pairs["456f136"].previous, "456f136");
        assert!(pairs["456f136"].entries.is_empty());
        assert_eq!(pairs.len(), 2);
        assert!(!pairs.contains_key("9999999"));
        assert!(!pairs.contains_key("1111111"));
    }

    #[test]
    fn should_read_sections_named_by_hunk_headers() {
        let body = [
            "    @@ Metadata",
            "     Author: t <t@e>",
            "    @@ src/a.ts: function foo",
            "     +keep",
            "    -+old",
            "    @@ src/b.ts (new)",
            "    ++new",
        ];

        let entries = split_entries(&body);

        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Commit message", "src/a.ts", "src/b.ts"]);
        assert_eq!(entries[1].lines, [" +keep", "-+old"]);
        assert_eq!(entries[2].lines, ["++new"]);
    }

    #[test]
    fn should_turn_an_entry_into_a_diff_file_with_a_distinct_path() {
        let pairs = parse_range_diff(RANGE_DIFF, &identity);

        let file = pairs["5fca649"].entries[1].to_diff_file();

        assert_eq!(
            file.display_path().display().to_string(),
            "b.txt (since review)"
        );
        let origins: Vec<LineOrigin> = file.hunks[0].lines.iter().map(|l| l.origin).collect();
        assert_eq!(
            origins,
            [
                LineOrigin::Context,
                LineOrigin::Context,
                LineOrigin::Deletion,
                LineOrigin::Addition,
                LineOrigin::Context
            ]
        );
        assert_eq!(file.hunks[0].lines[2].content, "+y");
    }

    use super::test_repo::{ReviewRepo, git};

    #[test]
    fn should_pair_amended_commits_and_leave_new_ones_unpaired() {
        let repo = ReviewRepo::new();
        let (b2, c) = repo.amend_b_and_add_c();
        let new = [repo.a.clone(), b2.clone(), c.clone()];

        let comparison = compare(
            repo.path(),
            &[repo.a.clone(), repo.b.clone()],
            Some("feat"),
            &new,
        )
        .unwrap();

        assert!(!comparison.baseline.pushed);
        assert_eq!(comparison.baseline.head, repo.b);
        assert_eq!(comparison.pairs[&repo.a].previous, repo.a);
        assert!(comparison.pairs[&repo.a].entries.is_empty());
        let pair = &comparison.pairs[&b2];
        assert_eq!(pair.previous, repo.b);
        assert_eq!(pair.entries.len(), 1);
        assert_eq!(pair.entries[0].label, "b.txt");
        assert!(pair.entries[0].lines.contains(&"-+b3".to_string()));
        assert!(pair.entries[0].lines.contains(&"++b3 fixed".to_string()));
        assert!(!comparison.pairs.contains_key(&c));
    }

    #[test]
    fn should_classify_each_commit_as_unchanged_changed_or_new() {
        let repo = ReviewRepo::new();
        let (b2, c) = repo.amend_b_and_add_c();
        let new = [repo.a.clone(), b2.clone(), c.clone()];
        let comparison = compare(
            repo.path(),
            &[repo.a.clone(), repo.b.clone()],
            Some("feat"),
            &new,
        )
        .unwrap();
        let mut patches_of = |sha: &str| {
            let text = git(repo.path(), &["show", "--format=", sha]);
            BTreeSet::from([ReviewedPatch {
                path: PathBuf::from("patch"),
                content_hash: text.len() as u64,
            }])
        };

        assert_eq!(
            comparison.status(&repo.a, &mut patches_of),
            SinceReview::Unchanged
        );
        assert_eq!(
            comparison.status(&b2, &mut patches_of),
            SinceReview::Changed
        );
        assert_eq!(comparison.status(&c, &mut patches_of), SinceReview::New);
    }

    #[test]
    fn should_fall_back_to_the_pushed_branch_when_the_old_commits_are_gone() {
        let repo = ReviewRepo::new();
        let _origin = repo.push_to_origin();
        let (b2, c) = repo.amend_b_and_add_c();
        let gone = ["1".repeat(40), "2".repeat(40)];
        let new = [repo.a.clone(), b2.clone(), c.clone()];

        let comparison = compare(repo.path(), &gone, Some("feat"), &new).unwrap();

        assert!(comparison.baseline.pushed);
        assert_eq!(comparison.baseline.head, repo.b);
        assert_eq!(comparison.pairs[&b2].previous, repo.b);
        assert_eq!(comparison.pairs[&repo.a].previous, repo.a);
        assert!(!comparison.pairs.contains_key(&c));
    }

    #[test]
    fn should_have_no_baseline_when_the_old_commits_and_the_pushed_branch_are_gone() {
        let repo = ReviewRepo::new();
        let gone = ["1".repeat(40)];
        let new = [repo.a.clone(), repo.b.clone()];

        assert_eq!(compare(repo.path(), &gone, Some("feat"), &new), None);
    }
}
