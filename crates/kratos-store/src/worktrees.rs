use kratos_protocol::{Ownership, Place, Worktree, WorktreeId};
use rusqlite::{Row, Transaction, params};
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
    /// Records a worktree Kratos is about to make, so that if the engine stops before a thread
    /// uses it, the next start knows the folder is Kratos's own leftover. `Err(WorktreeTaken)`
    /// when a worktree is already recorded at the path.
    pub fn reserve_worktree(&self, checkout: &Checkout) -> Result<WorktreeId, StoreError> {
        let id = WorktreeId::new(Uuid::new_v4().to_string());
        let inserted = self.connection.execute(
            "INSERT INTO worktrees (id, path, repository, branch, ownership, state, created_at)
             VALUES (?1, ?2, ?3, ?4, 'createdByKratos', 'creating', ?5)
             ON CONFLICT (path) DO NOTHING",
            params![
                id.as_str(),
                checkout.path,
                checkout.repository,
                checkout.branch,
                timestamp_now()?.as_str()
            ],
        )?;
        if inserted == 0 {
            return Err(StoreError::WorktreeTaken(checkout.path.clone()));
        }
        Ok(id)
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
                 ON CONFLICT (path) DO NOTHING",
                params![
                    Uuid::new_v4().to_string(),
                    checkout.path,
                    checkout.repository,
                    checkout.branch,
                    timestamp_now()?.as_str()
                ],
            )?;
            let (id, state): (String, String) = transaction.query_row(
                "SELECT id, state FROM worktrees WHERE path = ?1",
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

/// The place in columns `first..first + 4`, `worktrees.id, path, branch, ownership`, of a thread
/// row joined to its worktree. A thread with no worktree has NULL there.
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
    }))
}

fn ownership_name(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::CreatedByKratos => "createdByKratos",
        Ownership::Borrowed => "borrowed",
    }
}
