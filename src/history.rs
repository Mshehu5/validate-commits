use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

use gix::objs::Exists;
use gix::{ObjectId, Repository, revision::plumbing::Spec};

use crate::{CommitId, Error};

pub(crate) fn resolve_commits(repo: &Repository, input: &str) -> Result<Vec<CommitId>, Error> {
    let spec = parse(repo, input)?;
    let mut selection = Selection::default();
    match spec {
        Spec::Exclude(_) => {}
        Spec::ExcludeParents(id) => {
            return Ok(vec![CommitId(peel(repo, id)?)]);
        }
        Spec::Include(id) => selection.tips.push(peel(repo, id)?),
        Spec::Range { from, to } => {
            selection.tips.push(peel(repo, to)?);
            selection.hidden.push(peel(repo, from)?);
        }
        Spec::Merge { theirs, ours } => {
            selection.tips = vec![peel(repo, theirs)?, peel(repo, ours)?];
            selection.symmetric = true;
        }
        Spec::IncludeOnlyParents(id) => {
            let id = peel(repo, id)?;
            selection.tips = repo
                .find_commit(id)
                .map_err(|source| Error::FindCommit(Box::new(source)))?
                .parent_ids()
                .map(|id| id.detach())
                .collect();
            if selection.tips.iter().any(|id| !repo.objects.exists(id)) && repo.is_shallow() {
                fetch(repo, &[id], FetchDepth::Deepen(8))?;
            }
        }
    }
    let tips: Vec<_> = selection
        .tips
        .iter()
        .chain(&selection.hidden)
        .copied()
        .collect();
    let mut deepen = 8u32;
    loop {
        let history = AvailableHistory::read(repo, &tips)?;
        let excluded = if selection.symmetric {
            history
                .reachable(&[selection.tips[0]])
                .intersection(&history.reachable(&[selection.tips[1]]))
                .copied()
                .collect()
        } else {
            history.reachable(&selection.hidden)
        };
        let included = history
            .reachable(&selection.tips)
            .difference(&excluded)
            .copied()
            .collect();
        if !history.needs_deepening(&included, &excluded) {
            return Ok(history.select(&selection.tips, &excluded));
        }
        eprintln!("Deepening history by {deepen} commits");
        fetch(repo, &tips, FetchDepth::Deepen(deepen))?;
        deepen = deepen.saturating_mul(2);
    }
}

#[derive(Default)]
struct Selection {
    tips: Vec<ObjectId>,
    hidden: Vec<ObjectId>,
    symmetric: bool,
}

fn parse(repo: &Repository, input: &str) -> Result<Spec, Error> {
    if let Ok(spec) = repo.rev_parse(input) {
        return Ok(spec.detach());
    }
    // Recover only missing explicit SHAs. Parent-qualified revisions must
    // already resolve, as in the CI helper that accepts a base SHA.
    if repo.is_shallow() {
        for revision in input.split("..").map(|part| part.trim_start_matches('.')) {
            if let Ok(id) = ObjectId::from_hex(revision.as_bytes())
                && !repo.objects.exists(&id)
            {
                fetch(repo, &[id], FetchDepth::Initial)?;
            }
        }
    }
    repo.rev_parse(input)
        .map(|spec| spec.detach())
        .map_err(|source| Error::ParseRevision(Box::new(source)))
}

fn peel(repo: &Repository, id: ObjectId) -> Result<ObjectId, Error> {
    repo.find_object(id)
        .map_err(|source| Error::FindObject(Box::new(source)))?
        .peel_to_commit()
        .map(|commit| commit.id)
        .map_err(|source| Error::PeelToCommit(Box::new(source)))
}

enum FetchDepth {
    Initial,
    Deepen(u32),
}

fn fetch(repo: &Repository, tips: &[ObjectId], depth: FetchDepth) -> Result<(), Error> {
    let before = boundaries(repo)?;
    let depth_flag = match depth {
        FetchDepth::Initial => "--depth=1".to_owned(),
        FetchDepth::Deepen(count) => format!("--deepen={count}"),
    };
    let status = Command::new("git")
        .current_dir(repo.workdir().ok_or(Error::BareRepository)?)
        .args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
        ])
        .arg(depth_flag)
        .arg("origin")
        .args(tips.iter().map(ToString::to_string))
        .stdout(Stdio::null())
        .status()
        .map_err(Error::SpawnGit)?;
    if !status.success() {
        return Err(Error::FetchFailed { status });
    }
    if matches!(depth, FetchDepth::Deepen(_)) && boundaries(repo)? == before {
        return Err(Error::NoHistoryProgress);
    }
    Ok(())
}

fn boundaries(repo: &Repository) -> Result<HashSet<ObjectId>, Error> {
    Ok(repo
        .shallow_commits()
        .map_err(Error::ReadShallowCommits)?
        .map(|commits| commits.iter().copied().collect())
        .unwrap_or_default())
}

struct AvailableHistory {
    parents: HashMap<ObjectId, Vec<ObjectId>>,
    truncated: HashSet<ObjectId>,
}

impl AvailableHistory {
    fn read(repo: &Repository, tips: &[ObjectId]) -> Result<Self, Error> {
        let shallow = boundaries(repo)?;
        let mut parents = HashMap::new();
        let mut truncated = HashSet::new();
        let mut pending = tips.to_vec();
        while let Some(id) = pending.pop() {
            if parents.contains_key(&id) {
                continue;
            }
            let commit = repo
                .find_commit(id)
                .map_err(|source| Error::FindCommit(Box::new(source)))?;
            let mut ancestors: Vec<_> = commit
                .decode()
                .map_err(Error::DecodeCommit)?
                .parents()
                .collect();
            if shallow.contains(&id) && !ancestors.is_empty() {
                truncated.insert(id);
                ancestors.clear();
            }
            pending.extend(ancestors.iter().copied());
            parents.insert(id, ancestors);
        }
        Ok(Self { parents, truncated })
    }

    fn reachable(&self, tips: &[ObjectId]) -> HashSet<ObjectId> {
        let mut seen = HashSet::new();
        let mut pending = tips.to_vec();
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                pending.extend(self.parents[&id].iter().copied());
            }
        }
        seen
    }

    fn needs_deepening(&self, included: &HashSet<ObjectId>, excluded: &HashSet<ObjectId>) -> bool {
        if !included.is_disjoint(&self.truncated) {
            return true;
        }
        let hidden_boundaries: HashSet<_> =
            excluded.intersection(&self.truncated).copied().collect();
        // Each earliest included commit must descend from every hidden boundary.
        // Otherwise the missing base history could still exclude that commit.
        !hidden_boundaries.is_empty()
            && included.iter().any(|id| {
                self.parents[id]
                    .iter()
                    .all(|parent| excluded.contains(parent))
                    && !hidden_boundaries.is_subset(&self.reachable(&[*id]))
            })
    }

    fn select(&self, tips: &[ObjectId], excluded: &HashSet<ObjectId>) -> Vec<CommitId> {
        let mut seen = HashSet::new();
        let mut commits = Vec::new();
        let mut pending: Vec<_> = tips.iter().rev().map(|&id| (id, false)).collect();
        while let Some((id, visited)) = pending.pop() {
            if visited {
                commits.push(CommitId(id));
            } else if !excluded.contains(&id) && seen.insert(id) {
                pending.push((id, true));
                pending.extend(
                    self.parents[&id]
                        .iter()
                        .rev()
                        .map(|&parent| (parent, false)),
                );
            }
        }
        commits
    }
}
