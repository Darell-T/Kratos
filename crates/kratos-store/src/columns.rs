//! How columns hold protocol values: as JSON text, or as one fixed name per enum variant. A value
//! that no longer parses is a row this build cannot read, and reading fails rather than guess.

use kratos_protocol::{ApprovalDecision, Permission, Provider};
use rusqlite::Row;
use rusqlite::types::Type;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The names the `provider` columns store.
pub(crate) const PROVIDERS: [(Provider, &str); 2] =
    [(Provider::Codex, "codex"), (Provider::Claude, "claude")];

/// The names the `permission` column stores.
pub(crate) const PERMISSIONS: [(Permission, &str); 3] = [
    (Permission::AskMe, "askMe"),
    (Permission::ApproveForMe, "approveForMe"),
    (Permission::FullAccess, "fullAccess"),
];

/// The names the `decision` column stores.
pub(crate) const DECISIONS: [(ApprovalDecision, &str); 4] = [
    (ApprovalDecision::Accept, "accept"),
    (ApprovalDecision::AcceptForSession, "acceptForSession"),
    (ApprovalDecision::Decline, "decline"),
    (ApprovalDecision::Cancel, "cancel"),
];

/// The name `names` stores for `value`.
pub(crate) fn stored_name<T: Copy + Eq>(names: &[(T, &'static str)], value: T) -> &'static str {
    names
        .iter()
        .find(|(known, _)| *known == value)
        .map(|(_, name)| *name)
        .expect("every value has a stored name")
}

/// The value that `names` stores as `name`, read from column `index`. `what` names the value in
/// the error for a name this build does not know.
pub(crate) fn from_stored_name<T: Copy>(
    names: &[(T, &str)],
    name: &str,
    index: usize,
    what: &str,
) -> rusqlite::Result<T> {
    names
        .iter()
        .find(|(_, known)| *known == name)
        .map(|(value, _)| *value)
        .ok_or_else(|| unreadable(index, format!("unknown {what} {name}")))
}

/// The error for text in column `index` that this build cannot read, saying why.
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
