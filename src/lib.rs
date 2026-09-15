mod history;

use std::ffi::OsStr;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

pub fn run(revision_range: &str, output: &mut impl Write) -> Result<(), Error> {
    let repository = DiscoverRepository.run()?;
    let commits = history::resolve_commits(&repository.git_repo, revision_range)?;

    if commits.is_empty() {
        writeln!(output, "No commits in range.").map_err(Error::WriteOutput)?;
        return Ok(());
    }

    for commit in &commits {
        writeln!(output, "Validating {commit}").map_err(Error::WriteOutput)?;
        output.flush().map_err(Error::WriteOutput)?;
        CheckFlake {
            repository: &repository,
            commit,
        }
        .run()?;
    }

    Ok(())
}

struct Repository {
    git_repo: gix::Repository,
    root: PathBuf,
}

struct DiscoverRepository;

impl DiscoverRepository {
    fn run(self) -> Result<Repository, Error> {
        let git_repo =
            gix::discover(".").map_err(|source| Error::DiscoverRepository(Box::new(source)))?;
        let root = git_repo
            .workdir()
            .ok_or(Error::BareRepository)?
            .canonicalize()
            .map_err(Error::CanonicalizeRepository)?;
        Ok(Repository { git_repo, root })
    }
}

struct CheckFlake<'a> {
    repository: &'a Repository,
    commit: &'a CommitId,
}

impl CheckFlake<'_> {
    fn run(self) -> Result<(), Error> {
        let flake = FlakeReference::new(self.commit);
        let status = Command::new("nix")
            .args([
                OsStr::new("flake"),
                OsStr::new("check"),
                OsStr::new("--no-update-lock-file"),
                OsStr::new("--print-build-logs"),
                flake.as_os_str(),
            ])
            .current_dir(&self.repository.root)
            .status()
            .map_err(|source| Error::SpawnNix {
                commit: self.commit.clone(),
                source,
            })?;

        if status.success() {
            Ok(())
        } else {
            Err(Error::ValidationFailed {
                commit: self.commit.clone(),
                status,
            })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitId(gix::ObjectId);

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A relative flake reference: Nix resolves it against the working directory
/// (which `CheckFlake` pins to the repository root) and percent-encodes the
/// resulting path itself.
#[derive(Debug)]
struct FlakeReference(String);

impl FlakeReference {
    fn new(commit: &CommitId) -> Self {
        Self(format!("git+file:./?rev={commit}&shallow=1"))
    }

    fn as_os_str(&self) -> &OsStr {
        OsStr::new(self.0.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not discover the Git repository: {0}")]
    DiscoverRepository(#[source] Box<gix::discover::Error>),
    #[error("cannot validate a bare Git repository")]
    BareRepository,
    #[error("could not resolve the repository path: {0}")]
    CanonicalizeRepository(#[source] std::io::Error),
    #[error("could not parse the revision: {0}")]
    ParseRevision(#[source] Box<gix::Error>),
    #[error("could not read a selected object: {0}")]
    FindObject(#[source] Box<gix::object::find::existing::Error>),
    #[error("could not resolve a selected commit: {0}")]
    PeelToCommit(#[source] Box<gix::object::peel::to_kind::Error>),
    #[error("could not read a selected commit: {0}")]
    FindCommit(#[source] Box<gix::object::find::existing::with_conversion::Error>),
    #[error("could not decode a selected commit: {0}")]
    DecodeCommit(#[source] gix::objs::decode::Error),
    #[error("could not write output: {0}")]
    WriteOutput(#[source] std::io::Error),
    #[error("could not start Nix while checking commit {commit}: {source}")]
    SpawnNix {
        commit: CommitId,
        source: std::io::Error,
    },
    #[error("flake checks failed for commit {commit}{}", status_suffix(.status))]
    ValidationFailed {
        commit: CommitId,
        status: ExitStatus,
    },
}

fn status_suffix(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!(" with exit status {code}"),
        None => " after termination by signal".to_owned(),
    }
}
