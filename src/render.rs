//! Turns line spans into text for people and agents. The bytes come from the
//! local file core pointed to (big content never crosses the socket).
//! Long string fields can be shortened in the output; the file is untouched.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use serde_json::Value;

use crate::proto::LineSpan;

pub fn read_spans(path: &str, spans: &[LineSpan]) -> std::io::Result<Vec<Vec<u8>>> {
    let mut file = File::open(path)?;
    spans
        .iter()
        .map(|span| {
            file.seek(SeekFrom::Start(span.offset))?;
            let mut buf = vec![0; span.length as usize];
            file.read_exact(&mut buf)?;
            Ok(buf)
        })
        .collect()
}

/// One line as text. With `max_chars > 0`, every JSON string longer than that
/// is cut and marked with how many characters were left out.
pub fn line_text(bytes: &[u8], max_chars: usize) -> String {
    if max_chars == 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(mut value) => {
            shorten(&mut value, max_chars);
            value.to_string()
        }
        Err(_) => cut(&String::from_utf8_lossy(bytes), max_chars),
    }
}

fn shorten(value: &mut Value, max_chars: usize) {
    match value {
        Value::String(s) => {
            if s.chars().count() > max_chars {
                *s = cut(s, max_chars);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| shorten(v, max_chars)),
        Value::Object(map) => map.values_mut().for_each(|v| shorten(v, max_chars)),
        _ => {}
    }
}

fn cut(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let kept: String = s.chars().take(max_chars).collect();
    format!("{kept}…[+{} chars]", total - max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_long_strings_and_keeps_key_order() {
        let line = br#"{"type":"user","message":{"content":"abcdefghij"},"a":1}"#;
        assert_eq!(
            line_text(line, 4),
            r#"{"type":"user","message":{"content":"abcd…[+6 chars]"},"a":1}"#
        );
        assert_eq!(line_text(line, 0), String::from_utf8_lossy(line));
    }

    #[test]
    fn counts_characters_not_bytes() {
        assert_eq!(line_text("\"你好世界\"".as_bytes(), 2), "\"你好…[+2 chars]\"");
    }
}
