#![cfg(unix)]

use gix::objs::Exists;
use gix::{ObjectId, Repository};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// Real gix history exercises revision selection. Only Nix is replaced, so
// these tests can observe requests and force failures without building flakes.
struct Fixture {
    root: TempDir,
    repo: PathBuf,
    repository: Repository,
    commits: Vec<ObjectId>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_repo_name("repo")
    }

    fn with_repo_name(repo_name: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("validate-commits.")
            .tempdir()
            .unwrap();
        let repo = root.path().join(repo_name);
        let repository = gix::init(&repo).unwrap();
        let mut fixture = Self {
            root,
            repo,
            repository,
            commits: Vec::new(),
        };

        for message in ["Base", "First", "Second", "Third"] {
            fixture.commit(message);
        }

        let bin = fixture.root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|directory| directory.join("sh"))
            .find(|candidate| candidate.is_file())
            .expect("sh must be available while running tests")
            .canonicalize()
            .unwrap();
        let script = format!(
            "#!{}\n{}",
            shell.display(),
            r#"
[ "$#" -eq 5 ] || exit 90
[ "$1" = flake ] && [ "$2" = check ] || exit 91
[ "$3" = --no-update-lock-file ] || exit 92
[ "$4" = --print-build-logs ] || exit 93
printf '%s\n' "$5" >> "$TEST_NIX_LOG"
case "$5" in
  *"rev=$TEST_FAIL_COMMIT&"*) exit 7 ;;
esac
"#
        );
        fs::write(bin.join("nix"), script).unwrap();
        fs::set_permissions(bin.join("nix"), fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn commit(&mut self, message: &str) {
        fs::write(self.repo.join("tracked"), message).unwrap();
        let blob = self.repository.write_blob(message).unwrap().detach();
        let tree = gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: "tracked".into(),
                oid: blob,
            }],
        };
        let tree = self.repository.write_object(tree).unwrap().detach();
        let parents = self.commits.last().copied();
        let commit = commit_as(
            &self.repository,
            Some("HEAD"),
            message,
            tree,
            parents,
            self.commits.len() as i64,
        );
        self.commits.push(commit);
    }

    fn run(&self, range: &str, fail: Option<ObjectId>) -> Output {
        let paths = std::iter::once(self.root.path().join("bin"))
            .chain(env::split_paths(&env::var_os("PATH").unwrap()))
            .collect::<Vec<_>>();
        Command::new(env!("CARGO_BIN_EXE_validate-commits"))
            .arg(range)
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", env::join_paths(paths).unwrap())
            .env("TEST_NIX_LOG", self.root.path().join("calls"))
            .env(
                "TEST_FAIL_COMMIT",
                fail.map(|oid| oid.to_string()).unwrap_or_default(),
            )
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<String> {
        let path = self.root.path().join("calls");
        if !path.exists() {
            return Vec::new();
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn dirty_checkout(&self) -> (ObjectId, Vec<u8>, Option<Vec<u8>>) {
        fs::write(self.repo.join("tracked"), "unstaged edit").unwrap();
        fs::write(self.repo.join("untracked"), "keep me").unwrap();
        (
            self.repository.head_id().unwrap().detach(),
            fs::read(self.repository.path().join("HEAD")).unwrap(),
            fs::read(self.repository.path().join("index")).ok(),
        )
    }

    fn assert_checkout(&self, before: &(ObjectId, Vec<u8>, Option<Vec<u8>>)) {
        assert_eq!(self.repository.head_id().unwrap().detach(), before.0);
        assert_eq!(
            fs::read(self.repository.path().join("HEAD")).unwrap(),
            before.1
        );
        assert_eq!(
            fs::read(self.repository.path().join("index")).ok(),
            before.2
        );
        assert_eq!(
            fs::read_to_string(self.repo.join("tracked")).unwrap(),
            "unstaged edit"
        );
        assert_eq!(
            fs::read_to_string(self.repo.join("untracked")).unwrap(),
            "keep me"
        );
    }
}

// The CLI hands Nix a relative reference resolved against the repository root
// it sets as working directory, so Nix percent-encodes the repository path
// itself; absolute `git+file:` URLs built here could not express `[`, `]`,
// `^`, or `|` in a path.
fn expected_calls(commits: &[ObjectId]) -> Vec<String> {
    commits
        .iter()
        .map(|commit| format!("git+file:./?rev={commit}&shallow=1"))
        .collect()
}

fn signature(timestamp: i64) -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Test".into(),
        email: "test@example.com".into(),
        time: gix::date::Time::new(timestamp, 0),
    }
}

fn commit_as(
    repository: &Repository,
    reference: Option<&str>,
    message: &str,
    tree: ObjectId,
    parents: impl IntoIterator<Item = ObjectId>,
    timestamp: i64,
) -> ObjectId {
    let signature = signature(timestamp);
    let mut author_time = gix::date::parse::TimeBuf::default();
    let mut committer_time = gix::date::parse::TimeBuf::default();
    match reference {
        Some(reference) => repository
            .commit_as(
                signature.to_ref(&mut committer_time),
                signature.to_ref(&mut author_time),
                reference,
                message,
                tree,
                parents,
            )
            .unwrap()
            .detach(),
        None => {
            repository
                .new_commit_as(
                    signature.to_ref(&mut committer_time),
                    signature.to_ref(&mut author_time),
                    message,
                    tree,
                    parents,
                )
                .unwrap()
                .id
        }
    }
}

#[test]
fn validates_only_selected_commits_in_order_without_changing_checkout() {
    let fixture = Fixture::with_repo_name("repo[a]^b|c");
    let before = fixture.dirty_checkout();
    let output = fixture.run(&format!("{}..HEAD", fixture.commits[0]), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&fixture.commits[1..]));
    fixture.assert_checkout(&before);
}

#[test]
fn orders_merge_parents_with_skewed_dates() {
    let fixture = Fixture::new();
    let base = fixture.commits[3];
    let tree = fixture
        .repository
        .find_commit(base)
        .unwrap()
        .tree_id()
        .unwrap()
        .detach();
    let commit = |message, parents: &[ObjectId], timestamp| {
        commit_as(
            &fixture.repository,
            None,
            message,
            tree,
            parents.iter().copied(),
            timestamp,
        )
    };
    let fork = commit("Fork", &[base], 900);
    let left = commit("Left", &[fork], 10);
    let a = commit("A", &[left], 100);
    let b = commit("B", &[a], 100);
    let head = commit("Merge", &[b, fork], 100);

    let output = fixture.run(&format!("{b}...{fork}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[left, a, b]));
    fs::remove_file(fixture.root.path().join("calls")).unwrap();

    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[fork, left, a, b, head]));
}

fn history_commit(fixture: &Fixture, message: &str, parents: &[ObjectId]) -> ObjectId {
    let tree = fixture
        .repository
        .find_commit(fixture.commits[0])
        .unwrap()
        .tree_id()
        .unwrap()
        .detach();
    commit_as(
        &fixture.repository,
        None,
        message,
        tree,
        parents.iter().copied(),
        100,
    )
}

fn git_at(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn clone_shallow(fixture: &mut Fixture, depth: usize, branch: &str) {
    let remote = fixture.root.path().join("remote");
    fs::rename(&fixture.repo, &remote).unwrap();
    git_at(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            &format!("--depth={depth}"),
            &format!("--branch={branch}"),
            &format!("file://{}", remote.display()),
            "repo",
        ],
    );
    fixture.repository = gix::open(&fixture.repo).unwrap();
}

fn shallow_fixture(depth: usize) -> (Fixture, ObjectId, Vec<ObjectId>) {
    let mut fixture = Fixture::new();
    for i in 0..100 {
        fixture.commit(&format!("Main {i}"));
    }
    let base = *fixture.commits.last().unwrap();
    let mut side = base;
    let mut selected = Vec::new();
    for i in 0..40 {
        side = history_commit(&fixture, &format!("Side {i}"), &[side]);
        selected.push(side);
    }
    let head = history_commit(&fixture, "Merge", &[base, side]);
    selected.push(head);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, depth, "queue");
    (fixture, base, selected)
}

#[test]
fn deepens_missing_base_and_truncated_merge_parents() {
    for depth in [1, 2] {
        let (fixture, base, selected) = shallow_fixture(depth);
        let before = fixture.dirty_checkout();
        let refs = git_at(&fixture.repo, &["show-ref"]);
        let fetch_head = fixture.repo.join(".git/FETCH_HEAD");
        fs::write(&fetch_head, "preserve fetch head\n").unwrap();
        let output = fixture.run(&format!("{base}..HEAD"), None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut actual = fixture.calls();
        let mut expected = expected_calls(&selected);
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
            "true"
        );
        fixture.assert_checkout(&before);
        assert_eq!(git_at(&fixture.repo, &["show-ref"]), refs);
        assert_eq!(
            fs::read_to_string(&fetch_head).unwrap(),
            "preserve fetch head\n"
        );
    }
}

#[test]
fn complete_and_empty_shallow_ranges_do_not_fetch() {
    let (fixture, base, selected) = shallow_fixture(42);
    git_at(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/validate-commits",
        ],
    );
    assert!(fixture.run("HEAD..HEAD", None).status.success());
    assert!(fixture.calls().is_empty());
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&selected));
}

#[test]
fn shallow_origin_without_progress_stops_before_nix() {
    let (fixture, _, _) = shallow_fixture(1);
    let source = fixture.root.path().join("remote");
    let shallow_origin = fixture.root.path().join("shallow-origin");
    git_at(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            "--depth=1",
            "--branch=queue",
            &format!("file://{}", source.display()),
            "shallow-origin",
        ],
    );
    git_at(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("file://{}", shallow_origin.display()),
        ],
    );
    let output = fixture.run("HEAD", None);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fetch did not deepen history"), "{stderr}");
    assert!(fixture.calls().is_empty());
}

#[test]
fn fetch_failures_and_invalid_ranges_stop_before_nix() {
    // Depth 1 requires fetching the base; depth 2 requires deepening ancestry.
    for depth in [1, 2] {
        let (fixture, base, _) = shallow_fixture(depth);
        git_at(
            &fixture.repo,
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/validate-commits",
            ],
        );
        let output = fixture.run(&format!("{base}..HEAD"), None);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("git fetch failed"));
        assert!(fixture.calls().is_empty());
        assert!(!fixture.run("missing..HEAD", None).status.success());
        assert!(fixture.calls().is_empty());
    }
}

#[test]
fn deepens_base_history_to_exclude_shared_fork() {
    let mut fixture = Fixture::new();
    for i in 0..100 {
        fixture.commit(&format!("Shared {i}"));
    }
    let shared = *fixture.commits.last().unwrap();
    let fork = history_commit(&fixture, "Fork", &[shared]);
    let branch = history_commit(&fixture, "Branch", &[shared]);
    let mut main = fork;
    for i in 0..20 {
        main = history_commit(&fixture, &format!("Main {i}"), &[main]);
    }
    let base = history_commit(&fixture, "Merge branch", &[main, branch]);
    let pr = history_commit(&fixture, "PR", &[fork]);
    let head = history_commit(&fixture, "Queue", &[base, pr]);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, 1, "queue");
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut actual = fixture.calls();
    actual.sort();
    let mut expected = expected_calls(&[pr, head]);
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
}

#[test]
fn deepening_can_reach_full_history() {
    let mut fixture = Fixture::new();
    let base = fixture.commits[0];
    let branch = fixture.repository.head_name().unwrap().unwrap();
    clone_shallow(&mut fixture, 1, branch.shorten().to_string().as_str());
    let output = fixture.run(&format!("{base}..HEAD"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&fixture.commits[1..]));
    assert_eq!(
        git_at(&fixture.repo, &["rev-parse", "--is-shallow-repository"]),
        "false"
    );
}

#[test]
fn real_roots_in_complete_ranges_do_not_fetch_unrelated_shallow_history() {
    let fixture = Fixture::new();
    let base = fixture.commits[0];
    let independent = history_commit(&fixture, "Independent root", &[]);
    let head = history_commit(&fixture, "Merge independent history", &[base, independent]);
    let boundary = fixture.commits[3];
    fs::write(fixture.repo.join(".git/shallow"), format!("{boundary}\n")).unwrap();
    // The shallow boundary belongs to another branch; origin is unnecessary.
    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[independent, head]));
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".git/shallow")).unwrap(),
        format!("{boundary}\n")
    );
}

#[test]
fn real_roots_are_not_selected_without_proving_base_history_complete() {
    let fixture = Fixture::new();
    let base = fixture.commits[3];
    fs::write(fixture.repo.join(".git/shallow"), format!("{base}\n")).unwrap();
    let output = fixture.run(&format!("{base}..{}", fixture.commits[0]), None);
    assert!(!output.status.success());
    assert!(fixture.calls().is_empty());
}

#[test]
fn orders_merge_parents_with_skewed_dates_in_a_shallow_clone() {
    let mut fixture = Fixture::new();
    for i in 0..20 {
        fixture.commit(&format!("Main {i}"));
    }
    let base = *fixture.commits.last().unwrap();
    let tree = fixture
        .repository
        .find_commit(base)
        .unwrap()
        .tree_id()
        .unwrap()
        .detach();
    let fork = commit_as(&fixture.repository, None, "Fork", tree, [base], 900);
    let left = commit_as(&fixture.repository, None, "Left", tree, [fork], 10);
    let a = history_commit(&fixture, "A", &[left]);
    let b = history_commit(&fixture, "B", &[a]);
    let head = history_commit(&fixture, "Merge", &[b, fork]);
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &head.to_string()],
    );
    clone_shallow(&mut fixture, 6, "queue");
    let output = fixture.run(&format!("{base}..{head}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.calls(), expected_calls(&[fork, left, a, b, head]));
}

#[test]
fn single_commit_and_empty_shallow_selections_do_not_fetch() {
    let (fixture, _, selected) = shallow_fixture(1);
    git_at(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/validate-commits",
        ],
    );
    for revision in ["HEAD...HEAD", "^HEAD", "HEAD.."] {
        let output = fixture.run(revision, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.calls().is_empty());
    }
    let output = fixture.run("HEAD^!", None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.calls(),
        expected_calls(&[*selected.last().unwrap()])
    );
}

#[test]
fn single_revision_and_parent_selections_deepen_shallow_history() {
    for revision in ["HEAD", "HEAD^@"] {
        let (fixture, _, _) = shallow_fixture(1);
        let remote = fixture.root.path().join("remote");
        let remote_revision = revision.replace("HEAD", "queue");
        let expected: Vec<_> = git_at(&remote, &["rev-list", &remote_revision])
            .lines()
            .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
            .collect();
        let output = fixture.run(revision, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut actual = fixture.calls();
        actual.sort();
        let mut expected = expected_calls(&expected);
        expected.sort();
        assert_eq!(actual, expected);
    }
}

#[test]
fn symmetric_shallow_ranges_deepen_until_shared_history_is_excluded() {
    let mut fixture = Fixture::new();
    let shared = fixture.commits[3];
    let mut left = shared;
    let mut right = shared;
    let mut selected = Vec::new();
    for i in 0..3 {
        left = history_commit(&fixture, &format!("Left {i}"), &[left]);
        selected.push(left);
    }
    for i in 0..20 {
        right = history_commit(&fixture, &format!("Right {i}"), &[right]);
        selected.push(right);
    }
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/queue", &left.to_string()],
    );
    git_at(
        &fixture.repo,
        &["update-ref", "refs/heads/right", &right.to_string()],
    );
    clone_shallow(&mut fixture, 1, "queue");
    let output = fixture.run(&format!("HEAD...{right}"), None);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut actual = fixture.calls();
    actual.sort();
    let mut expected = expected_calls(&selected);
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn unavailable_parent_revisions_fail_without_fetching() {
    let (fixture, base, _) = shallow_fixture(1);
    let shallow = fs::read(fixture.repo.join(".git/shallow")).unwrap();
    let before = fixture.dirty_checkout();
    for range in ["HEAD~1..HEAD".to_owned(), format!("{base}~1..HEAD")] {
        let output = fixture.run(&range, None);
        assert!(!output.status.success(), "{range}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Deepening history"), "{range}: {stderr}");
        assert!(fixture.calls().is_empty());
        assert_eq!(
            fs::read(fixture.repo.join(".git/shallow")).unwrap(),
            shallow
        );
        fixture.assert_checkout(&before);
    }
    // The missing explicit SHA was not fetched merely to retry parent notation.
    assert!(!fixture.repo.join(".git/FETCH_HEAD").exists());
    assert!(!fixture.repository.objects.exists(&base));
}
