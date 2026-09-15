#![cfg(unix)]

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
