//! Claude Code 2.1.322 can silently ignore malformed structured output schemas. Kratos checks new schemas before dispatch.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The requested shape of a final reply. A provider can finish without returning structured output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReportFormat(Map<String, Value>);

impl ReportFormat {
    /// Checks `schema` and wraps it.
    ///
    /// # Errors
    /// Returns where and why the schema is not one the providers can follow: the root must be an
    /// object schema, and each keyword below it the providers read must have its JSON Schema shape.
    pub fn new(schema: Map<String, Value>) -> Result<ReportFormat, ReportFormatError> {
        let format = Self(schema);
        format.validate()?;
        Ok(format)
    }

    /// Checks whether this schema can be used for a new turn.
    ///
    /// # Errors
    /// Returns the offending JSON pointer for an unsupported schema.
    pub fn validate(&self) -> Result<(), ReportFormatError> {
        if self.0.get("type").and_then(Value::as_str) != Some("object") {
            return Err(ReportFormatError::at(
                "",
                "the root must have \"type\": \"object\"",
            ));
        }
        check_schema_object(&self.0, "")
    }

    pub fn schema(&self) -> &Map<String, Value> {
        &self.0
    }
}

impl TryFrom<Map<String, Value>> for ReportFormat {
    type Error = ReportFormatError;

    fn try_from(schema: Map<String, Value>) -> Result<ReportFormat, ReportFormatError> {
        ReportFormat::new(schema)
    }
}

impl From<ReportFormat> for Map<String, Value> {
    fn from(format: ReportFormat) -> Map<String, Value> {
        format.0
    }
}

/// Why a report format was refused, and where in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportFormatError {
    /// A JSON pointer to the offending value, empty for the root.
    pub pointer: String,
    pub problem: &'static str,
}

impl ReportFormatError {
    fn at(pointer: &str, problem: &'static str) -> ReportFormatError {
        ReportFormatError {
            pointer: pointer.to_owned(),
            problem,
        }
    }
}

impl fmt::Display for ReportFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = if self.pointer.is_empty() {
            "the root"
        } else {
            &self.pointer
        };
        write!(formatter, "reportFormat at {at}: {}", self.problem)
    }
}

impl std::error::Error for ReportFormatError {}

type Check = fn(&Value, &str) -> Result<(), ReportFormatError>;

const KEYWORDS: [(&str, Check); 15] = [
    ("type", types),
    ("properties", schema_map),
    ("required", names),
    ("items", schema_or_bool),
    ("additionalProperties", schema_or_bool),
    ("enum", non_empty_list),
    ("anyOf", schema_list),
    ("allOf", schema_list),
    ("oneOf", schema_list),
    ("not", schema),
    ("$defs", schema_map),
    ("definitions", schema_map),
    ("$ref", text),
    ("description", text),
    ("title", text),
];

const TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

fn check_schema_object(
    schema: &Map<String, Value>,
    pointer: &str,
) -> Result<(), ReportFormatError> {
    for (keyword, check) in KEYWORDS {
        if let Some(value) = schema.get(keyword) {
            check(value, &format!("{pointer}/{keyword}"))?;
        }
    }
    Ok(())
}

fn schema(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    match value {
        Value::Object(object) => check_schema_object(object, pointer),
        _ => Err(ReportFormatError::at(pointer, "must be a schema object")),
    }
}

fn schema_or_bool(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    match value {
        Value::Bool(_) => Ok(()),
        value => schema(value, pointer),
    }
}

fn schema_map(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    let Value::Object(map) = value else {
        return Err(ReportFormatError::at(
            pointer,
            "must map names to schema objects",
        ));
    };
    map.iter().try_for_each(|(name, value)| {
        schema(
            value,
            &format!("{pointer}/{}", name.replace('~', "~0").replace('/', "~1")),
        )
    })
}

fn schema_list(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    match value.as_array() {
        Some(list) if !list.is_empty() => list
            .iter()
            .enumerate()
            .try_for_each(|(index, value)| schema(value, &format!("{pointer}/{index}"))),
        _ => Err(ReportFormatError::at(
            pointer,
            "must be a non-empty list of schema objects",
        )),
    }
}

fn names(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    let names = value
        .as_array()
        .is_some_and(|list| list.iter().all(Value::is_string));
    names
        .then_some(())
        .ok_or_else(|| ReportFormatError::at(pointer, "must be a list of property names"))
}

fn non_empty_list(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    let filled = value.as_array().is_some_and(|list| !list.is_empty());
    filled
        .then_some(())
        .ok_or_else(|| ReportFormatError::at(pointer, "must be a non-empty list"))
}

fn types(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    let is_type = |value: &Value| value.as_str().is_some_and(|name| TYPES.contains(&name));
    let valid = match value {
        Value::Array(list) => !list.is_empty() && list.iter().all(is_type),
        value => is_type(value),
    };
    valid.then_some(()).ok_or_else(|| {
        ReportFormatError::at(
            pointer,
            "must be one of object, array, string, number, integer, boolean or null, or a list of them",
        )
    })
}

fn text(value: &Value, pointer: &str) -> Result<(), ReportFormatError> {
    value
        .is_string()
        .then_some(())
        .ok_or_else(|| ReportFormatError::at(pointer, "must be a string"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn format(schema: Value) -> Result<ReportFormat, ReportFormatError> {
        ReportFormat::new(
            schema
                .as_object()
                .cloned()
                .expect("a test schema is an object"),
        )
    }

    #[test]
    fn a_schema_the_providers_can_follow_is_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["pass", "fail"] },
                "findings": { "type": "array", "items": { "$ref": "#/$defs/finding" } },
                "score": { "type": ["number", "null"], "minimum": 0 },
                "note": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
            },
            "required": ["verdict", "findings"],
            "additionalProperties": false,
            "$defs": { "finding": { "type": "object", "properties": { "file": { "type": "string" } } } }
        });
        assert_eq!(
            format(schema.clone()).unwrap().schema(),
            schema.as_object().unwrap()
        );
    }

    #[test]
    fn a_schema_a_provider_would_ignore_or_refuse_is_refused_with_where_and_why() {
        for (schema, pointer) in [
            (json!({ "type": "objekt" }), ""),
            (
                json!({ "type": "array", "items": { "type": "string" } }),
                "",
            ),
            (json!({ "properties": {} }), ""),
            (
                json!({ "type": "object", "properties": "verdict" }),
                "/properties",
            ),
            (
                json!({ "type": "object", "properties": { "a": { "type": "text" } } }),
                "/properties/a/type",
            ),
            (json!({ "type": "object", "required": "a" }), "/required"),
            (
                json!({ "type": "object", "properties": { "a": { "items": 3 } } }),
                "/properties/a/items",
            ),
            (json!({ "type": "object", "anyOf": [] }), "/anyOf"),
            (json!({ "type": "object", "enum": [] }), "/enum"),
        ] {
            let refused = format(schema.clone()).expect_err(&schema.to_string());
            assert_eq!(refused.pointer, pointer, "{schema}");
            assert!(
                refused.to_string().starts_with("reportFormat at "),
                "{refused}"
            );
        }
    }
}
