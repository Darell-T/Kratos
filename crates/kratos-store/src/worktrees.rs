use kratos_protocol::{Ownership, Place, Worktree, WorktreeId};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use crate::columns::{from_stored_name, timestamp_now};
use crate::{Store, StoreError};

/// A git worktree as git describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The worktree's top folder, with links resolved.
    pub path: String,
    /// The repository's shared git folder, the same for every worktree of one repository.
    pub repository: String,
    /// `None` when no branch is checked out.
    pub branch: Option<String>,
}

/// A worktree an earlier engine left unfinished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnfinishedWorktree {
    pub id: WorktreeId,
    pub path: String,
}

/// A worktree Kratos made and has not removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KratosWorktree {
    pub id: WorktreeId,
    pub path: String,
    /// The folder git keeps for the worktree, which tells it apart from another checkout at its
    /// path. `None` until Kratos has made the worktree.
    pub git_dir: Option<String>,
}

/// What [`Store::begin_removal`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The worktree is marked as being removed, and the caller removes it from `repository`, the
    /// shared git folder, once the checkout at its path is the one `git_dir` names.
    Started {
        repository: String,
        git_dir: Option<String>,
    },
    /// A thread that is not archived still works in it.
    InUse,
    /// An earlier archive already removed it.
    Done,
}

/// How a removal ended, for [`Store::end_removal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalEnd {
    /// The worktree is gone from its path: Kratos removed it, or another checkout stands there.
    Removed,
    /// The worktree held files the user did not choose to discard.
    Kept,
}

/// Where a new thread works, once the engine has checked it with git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewPlace {
    ProjectFolder,
    /// A worktree Kratos made after [`Store::reserve_worktree`] recorded it.
    Created(WorktreeId),
    /// A worktree that already existed. A thread attaches to its record if one exists.
    Borrowed(Checkout),
}

impl Store {
    /// Records a worktree Kratos is about to make from commit `base`, so that if the engine stops
    /// before a thread uses it, the next start knows the folder is Kratos's own leftover.
    /// `Err(WorktreeTaken)` when a worktree is already recorded at the path.
    pub fn reserve_worktree(
        &self,
        checkout: &Checkout,
        base: &str,
    ) -> Result<WorktreeId, StoreError> {
        let id = WorktreeId::new(Uuid::new_v4().to_string());
        let inserted = self.connection.execute(
            "INSERT INTO worktrees (id, path, repository, branch, base, ownership, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'createdByKratos', 'creating', ?6)
             ON CONFLICT (path) WHERE state != 'removed' DO NOTHING",
            params![
                id.as_str(),
                checkout.path,
                checkout.repository,
                checkout.branch,
                base,
                timestamp_now()?.as_str()
            ],
        )?;
        if inserted == 0 {
            return Err(StoreError::WorktreeTaken(checkout.path.clone()));
        }
        Ok(id)
    }

    /// Records `git_dir`, the folder git made for the worktree Kratos just checked out.
    pub fn record_git_dir(&self, id: &WorktreeId, git_dir: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE worktrees SET git_dir = ?2 WHERE id = ?1",
            params![id.as_str(), git_dir],
        )?;
        Ok(())
    }

    /// Worktrees an earlier engine left unfinished: reserved and never given to a thread, being
    /// removed when git refused, or made by Kratos with every thread archived. The last covers an
    /// archive that stopped before its removal began, and a worktree kept for its files, which
    /// goes once the user has dealt with them. Read before the engine serves, since a worktree
    /// reserved after that may be one a request is making.
    pub fn unfinished_worktrees(&self) -> Result<Vec<UnfinishedWorktree>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, path FROM worktrees
             WHERE state IN ('creating', 'removing')
                OR (state = 'ready' AND ownership = 'createdByKratos'
                    AND NOT EXISTS (SELECT 1 FROM threads
                                    WHERE worktree_id = worktrees.id AND archived_at IS NULL))
             ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(UnfinishedWorktree {
                id: WorktreeId::new(row.get(0)?),
                path: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Marks a worktree Kratos made as being removed, once no thread that is not archived works
    /// in it. From then on no thread may join it. A worktree already being removed counts as
    /// started again, so an archive can retry a removal git refused. So does a reservation no
    /// thread took, which only [`Store::unfinished_worktrees`] names.
    pub fn begin_removal(&mut self, id: &WorktreeId) -> Result<Removal, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT state, repository, git_dir FROM worktrees
                 WHERE id = ?1 AND ownership = 'createdByKratos'",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((state, repository, git_dir)) = found else {
            return Err(StoreError::UnknownWorktree(id.clone()));
        };
        let removal = match state.as_str() {
            "removed" => Removal::Done,
            _ => {
                let started = transaction.execute(
                    "UPDATE worktrees SET state = 'removing'
                     WHERE id = ?1 AND state IN ('creating', 'ready', 'removing')
                       AND NOT EXISTS (SELECT 1 FROM threads
                                       WHERE worktree_id = ?1 AND archived_at IS NULL)",
                    [id.as_str()],
                )?;
                if started == 0 {
                    Removal::InUse
                } else {
                    Removal::Started {
                        repository,
                        git_dir,
                    }
                }
            }
        };
        transaction.commit()?;
        Ok(removal)
    }

    /// Records how a removal [`Store::begin_removal`] started ended. A kept worktree may take
    /// threads again.
    pub fn end_removal(&self, id: &WorktreeId, end: RemovalEnd) -> Result<(), StoreError> {
        let state = match end {
            RemovalEnd::Removed => "removed",
            RemovalEnd::Kept => "ready",
        };
        self.connection.execute(
            "UPDATE worktrees SET state = ?1 WHERE id = ?2 AND state = 'removing'",
            params![state, id.as_str()],
        )?;
        Ok(())
    }

    /// The worktrees Kratos made that it has not removed.
    pub fn kratos_worktrees(&self) -> Result<Vec<KratosWorktree>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, path, git_dir FROM worktrees
             WHERE ownership = 'createdByKratos' AND state != 'removed'",
        )?;
        let worktrees = statement.query_map([], |row| {
            Ok(KratosWorktree {
                id: WorktreeId::new(row.get(0)?),
                path: row.get(1)?,
                git_dir: row.get(2)?,
            })
        })?;
        Ok(worktrees.collect::<Result<_, _>>()?)
    }

    /// Closes the record of a worktree Kratos made that no longer stands at its path, because
    /// another checkout took its place, so that checkout can be recorded as borrowed.
    pub fn close_replaced_worktree(&self, id: &WorktreeId) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE worktrees SET state = 'removed'
             WHERE id = ?1 AND ownership = 'createdByKratos' AND state = 'ready'",
            [id.as_str()],
        )?;
        Ok(())
    }

    /// Forgets a reservation that git never turned into a worktree.
    pub fn release_reservation(&self, id: &WorktreeId) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM worktrees WHERE id = ?1 AND state = 'creating'",
            [id.as_str()],
        )?;
        Ok(())
    }
}

/// The worktree id a new thread records, after attaching it to `place` inside `transaction`.
pub(crate) fn attach(
    transaction: &Transaction<'_>,
    place: &NewPlace,
) -> Result<Option<WorktreeId>, StoreError> {
    match place {
        NewPlace::ProjectFolder => Ok(None),
        NewPlace::Created(id) => {
            let made = transaction.execute(
                "UPDATE worktrees SET state = 'ready' WHERE id = ?1 AND state = 'creating'",
                [id.as_str()],
            )?;
            if made == 0 {
                return Err(StoreError::UnknownWorktree(id.clone()));
            }
            Ok(Some(id.clone()))
        }
        NewPlace::Borrowed(checkout) => {
            transaction.execute(
                "INSERT INTO worktrees (id, path, repository, branch, ownership, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'borrowed', 'ready', ?5)
                 ON CONFLICT (path) WHERE state != 'removed' DO NOTHING",
                params![
                    Uuid::new_v4().to_string(),
                    checkout.path,
                    checkout.repository,
                    checkout.branch,
                    timestamp_now()?.as_str()
                ],
            )?;
            let (id, state): (String, String) = transaction.query_row(
                "SELECT id, state FROM worktrees WHERE path = ?1 AND state != 'removed'",
                [&checkout.path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if state != "ready" {
                return Err(StoreError::WorktreeTaken(checkout.path.clone()));
            }
            Ok(Some(WorktreeId::new(id)))
        }
    }
}

/// The place in columns `first..first + 5`, `worktrees.id, path, branch, ownership, base`, of a
/// thread row joined to its worktree. A thread with no worktree has NULL there.
pub(crate) fn place_from_row(row: &Row<'_>, first: usize) -> rusqlite::Result<Place> {
    let Some(id) = row.get::<_, Option<String>>(first)? else {
        return Ok(Place::ProjectFolder);
    };
    let ownership: String = row.get(first + 3)?;
    Ok(Place::Worktree(Worktree {
        id: WorktreeId::new(id),
        path: row.get(first + 1)?,
        branch: row.get(first + 2)?,
        ownership: from_stored_name(
            [Ownership::CreatedByKratos, Ownership::Borrowed],
            ownership_name,
            &ownership,
            first + 3,
            "ownership",
        )?,
        base: row.get(first + 4)?,
    }))
}

fn ownership_name(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::CreatedByKratos => "createdByKratos",
        Ownership::Borrowed => "borrowed",
    }
}
