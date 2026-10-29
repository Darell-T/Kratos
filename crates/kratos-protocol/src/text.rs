use serde::de::{Deserializer, Error};
use serde::{Deserialize, Serialize};

/// Text a client must supply: surrounding whitespace removed, and something left afterwards.
///
/// The engine stores the trimmed form: posting `"  hello  "` records `"hello"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequiredText(String);

impl RequiredText {
    pub fn new(text: &str) -> Option<RequiredText> {
        let trimmed = text.trim_matches(is_javascript_whitespace);
        (!trimmed.is_empty()).then(|| RequiredText(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// What JavaScript's `String.prototype.trim` removes, so both engines accept and store the same
/// text. `char::is_whitespace` differs in two characters: it keeps U+FEFF and strips U+0085.
fn is_javascript_whitespace(character: char) -> bool {
    character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
}

impl<'de> Deserialize<'de> for RequiredText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        RequiredText::new(&text).ok_or_else(|| D::Error::custom("must not be blank"))
    }
}
