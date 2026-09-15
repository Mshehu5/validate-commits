use gix::{ObjectId, Repository, revision::plumbing::Spec};
use std::collections::{HashMap, HashSet};

use crate::{CommitId, Error};

pub(crate) fn resolve_commits(repo: &Repository, input: &str) -> Result<Vec<CommitId>, Error> {
    let spec = repo
        .rev_parse(input)
        .map_err(|source| Error::ParseRevision(Box::new(source)))?
        .detach();
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
        }
    }
    let tips: Vec<_> = selection
        .tips
        .iter()
        .chain(&selection.hidden)
        .copied()
        .collect();
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
    Ok(history.select(&selection.tips, &excluded))
}

#[derive(Default)]
struct Selection {
    tips: Vec<ObjectId>,
    hidden: Vec<ObjectId>,
    symmetric: bool,
}

fn peel(repo: &Repository, id: ObjectId) -> Result<ObjectId, Error> {
    repo.find_object(id)
        .map_err(|source| Error::FindObject(Box::new(source)))?
        .peel_to_commit()
        .map(|commit| commit.id)
        .map_err(|source| Error::PeelToCommit(Box::new(source)))
}

struct AvailableHistory {
    parents: HashMap<ObjectId, Vec<ObjectId>>,
}

impl AvailableHistory {
    fn read(repo: &Repository, tips: &[ObjectId]) -> Result<Self, Error> {
        let mut parents = HashMap::new();
        let mut pending = tips.to_vec();
        while let Some(id) = pending.pop() {
            if parents.contains_key(&id) {
                continue;
            }
            let commit = repo
                .find_commit(id)
                .map_err(|source| Error::FindCommit(Box::new(source)))?;
            let ancestors: Vec<_> = commit
                .decode()
                .map_err(Error::DecodeCommit)?
                .parents()
                .collect();
            pending.extend(ancestors.iter().copied());
            parents.insert(id, ancestors);
        }
        Ok(Self { parents })
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
