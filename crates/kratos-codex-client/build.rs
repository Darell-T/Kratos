//! Generates the Codex protocol types from the JSON Schema bundle Codex exports.

use std::path::PathBuf;

use serde_json::{Value, json};

const SCHEMA: &str = "schema/codex_app_server_protocol.schemas.json";

fn main() {
    println!("cargo::rerun-if-changed={SCHEMA}");
    let text = std::fs::read_to_string(SCHEMA).expect("the schema bundle is committed");
    let mut bundle = flatten(&text);
    objects_for_true(&mut bundle);
    let root: schemars::schema::RootSchema =
        serde_json::from_value(bundle).expect("the bundle is a JSON Schema");
    let settings = typify::TypeSpaceSettings::default()
        .with_struct_builder(false)
        .with_derive("PartialEq".to_owned())
        .to_owned();
    let mut space = typify::TypeSpace::new(&settings);
    space
        .add_root_schema(root)
        .expect("typify converts every definition");
    let file = syn::parse2::<syn::File>(space.to_stream()).expect("typify emits valid Rust");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("codex_types.rs"), prettyplease::unparse(&file))
        .expect("OUT_DIR is writable");
}

/// The bundle keeps most types under `definitions.v2`, referenced as `#/definitions/v2/<Name>`.
/// Typify wants one flat map. The two names defined at both levels, `AbsolutePathBuf` and
/// `RequestId`, accept the same values at each.
fn flatten(text: &str) -> Value {
    let mut bundle: Value =
        serde_json::from_str(&text.replace("#/definitions/v2/", "#/definitions/"))
            .expect("the bundle is JSON");
    let definitions = bundle["definitions"]
        .as_object_mut()
        .expect("the bundle has definitions");
    let nested = definitions
        .remove("v2")
        .expect("the bundle nests the v2 definitions");
    for (name, schema) in nested.as_object().expect("v2 holds definitions") {
        definitions
            .entry(name.clone())
            .or_insert_with(|| schema.clone());
    }
    bundle
}

/// A property schema of `true` accepts any value, as `{}` does. Typify only merges the object
/// form: on `McpServerElicitationRequestParams` the `true` form makes it panic.
fn objects_for_true(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get_mut("properties") {
                for property in properties.values_mut() {
                    if *property == Value::Bool(true) {
                        *property = json!({});
                    }
                }
            }
            map.values_mut().for_each(objects_for_true);
        }
        Value::Array(items) => items.iter_mut().for_each(objects_for_true),
        _ => {}
    }
}
