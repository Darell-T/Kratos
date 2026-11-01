//! Columns that hold a protocol value as JSON text. A value that no longer parses is a row this
//! build cannot read, and reading fails rather than guess.

use rusqlite::Row;
use rusqlite::types::Type;
use serde::Serialize;
use serde::de::DeserializeOwned;

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
