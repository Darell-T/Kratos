use serde_json::Value;

const UUID_SHAPE: &str = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
const TIMESTAMP_SHAPE: &str = "9999-99-99T99:99:99.999Z";

/// Replaces ids and times so the output of two runs, or of two engines, compares with a plain diff.
///
/// Each distinct id becomes `id-1`, `id-2`, and so on, in order of first appearance, so a swapped
/// id still shows as a difference. Each time becomes `<time>`.
#[derive(Default)]
pub struct Normalizer {
    ids: Vec<String>,
}

impl Normalizer {
    pub fn apply(&mut self, value: &mut Value) {
        match value {
            Value::String(text) => self.rewrite(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.apply(item)),
            Value::Object(fields) => fields.values_mut().for_each(|field| self.apply(field)),
            _ => {}
        }
    }

    fn rewrite(&mut self, text: &mut String) {
        if has_shape(text, TIMESTAMP_SHAPE) {
            *text = "<time>".to_owned();
        } else if has_shape(text, UUID_SHAPE) {
            let position = self
                .ids
                .iter()
                .position(|id| id == text)
                .unwrap_or_else(|| {
                    self.ids.push(text.clone());
                    self.ids.len() - 1
                });
            *text = format!("id-{}", position + 1);
        }
    }
}

/// True when `text` has the length of `shape` and matches it character by character: `x` is a hex
/// digit, `9` is a digit, and anything else stands for itself.
fn has_shape(text: &str, shape: &str) -> bool {
    text.len() == shape.len()
        && text
            .bytes()
            .zip(shape.bytes())
            .all(|(character, expected)| match expected {
                b'x' => character.is_ascii_hexdigit(),
                b'9' => character.is_ascii_digit(),
                _ => character == expected,
            })
}
