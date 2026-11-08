//! DAP wire framing: `Content-Length` headers around JSON bodies.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// Read one message. Returns None at a clean end of stream.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            length = Some(value.parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length.ok_or_else(|| io::Error::other("missing Content-Length header"))?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn write_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn messages_round_trip_back_to_back() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &json!({"seq": 1})).unwrap();
        write_message(&mut bytes, &json!({"text": "多字节"})).unwrap();
        let mut reader = Cursor::new(bytes);
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["seq"], 1);
        assert_eq!(
            read_message(&mut reader).unwrap().unwrap()["text"],
            "多字节"
        );
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn header_name_is_case_insensitive_and_others_are_ignored() {
        let raw = "content-length: 2\r\nContent-Type: x\r\n\r\n{}";
        assert_eq!(
            read_message(&mut Cursor::new(raw)).unwrap().unwrap(),
            json!({})
        );
    }

    #[test]
    fn missing_length_is_an_error() {
        assert!(read_message(&mut Cursor::new("X: 1\r\n\r\n{}")).is_err());
    }

    #[test]
    fn truncated_body_is_an_error() {
        assert!(read_message(&mut Cursor::new("Content-Length: 9\r\n\r\n{}")).is_err());
    }
}
