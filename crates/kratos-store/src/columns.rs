//! How columns hold protocol values: as JSON text, or as one fixed name per enum variant. A value
//! that no longer parses is a row this build cannot read, and reading fails rather than guess.

use kratos_protocol::{ApprovalDecision, Permission, Provider};
use rusqlite::Row;
use rusqlite::types::Type;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
    }
}

pub(crate) fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::AskMe => "askMe",
        Permission::ApproveForMe => "approveForMe",
        Permission::FullAccess => "fullAccess",
    }
}

pub(crate) fn decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Accept => "accept",
        ApprovalDecision::AcceptForSession => "acceptForSession",
        ApprovalDecision::Decline => "decline",
        ApprovalDecision::Cancel => "cancel",
    }
}

pub(crate) fn from_stored_name<T: Copy>(
    all: impl IntoIterator<Item = T>,
    name_of: fn(T) -> &'static str,
    name: &str,
    index: usize,
    what: &str,
) -> rusqlite::Result<T> {
    all.into_iter()
        .find(|value| name_of(*value) == name)
        .ok_or_else(|| unreadable(index, format!("unknown {what} {name}")))
}

pub(crate) fn unreadable(index: usize, reason: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, reason.into())
}

pub(crate) fn to_column<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("protocol values serialize")
}

/// The column at `index` as `T`, or `None` when it is NULL.
pub(crate) fn from_column<T: DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    let Some(text) = row.get::<_, Option<String>>(index)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, error.into()))
}

/// The column at `index` as `T`, for a column that is never NULL.
pub(crate) fn required_column<T: DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    from_column(row, index)?.ok_or(rusqlite::Error::InvalidColumnType(
        index,
        "a JSON value".to_owned(),
        Type::Null,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads_back<T: Copy + Eq + std::fmt::Debug, const N: usize>(
        all: [T; N],
        name_of: fn(T) -> &'static str,
    ) {
        for value in all {
            let read = from_stored_name(all, name_of, name_of(value), 0, "value").unwrap();
            assert_eq!(read, value, "{} reads back as {read:?}", name_of(value));
        }
    }

    #[test]
    fn every_stored_name_reads_back_as_its_value() {
        // Catches two variants stored under one name, so a row reads back as the other variant.
        reads_back(Provider::ALL, provider_name);
        reads_back(Permission::ALL, permission_name);
        reads_back(ApprovalDecision::ALL, decision_name);
    }
}
