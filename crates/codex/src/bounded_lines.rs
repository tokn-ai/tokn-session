//! JSONL record bounds are independent of the length of their source stream.
use std::io::{self, BufRead, Read};

pub(crate) fn lines(reader: impl BufRead, max_record_bytes: usize) -> impl Iterator<Item = io::Result<String>> {
  Lines {
    reader,
    max_record_bytes,
    finished: false,
  }
}

struct Lines<R> {
  reader: R,
  max_record_bytes: usize,
  finished: bool,
}

impl<R: BufRead> Iterator for Lines<R> {
  type Item = io::Result<String>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.finished {
      return None;
    }
    let mut bytes = Vec::new();
    let read = Read::by_ref(&mut self.reader)
      .take(self.max_record_bytes.saturating_add(1) as u64)
      .read_until(b'\n', &mut bytes);
    match read {
      Ok(0) => {
        self.finished = true;
        None
      }
      Ok(_) if bytes.len() > self.max_record_bytes => {
        self.finished = true;
        Some(Err(io::Error::other(
          "Codex history record exceeds the record size limit",
        )))
      }
      Ok(_) => {
        if bytes.last() == Some(&b'\n') {
          bytes.pop();
          if bytes.last() == Some(&b'\r') {
            bytes.pop();
          }
        }
        Some(String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)))
      }
      Err(err) => {
        self.finished = true;
        Some(Err(err))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn limits_each_row_instead_of_the_stream_and_rejects_partial_oversized_rows() {
    assert_eq!(
      lines(&b"abc\ndef\nghi\r\njkl"[..], 5)
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
      ["abc", "def", "ghi", "jkl"]
    );
    for source in ["abcdef\n", "abcdef"] {
      assert!(
        lines(source.as_bytes(), 5)
          .next()
          .unwrap()
          .unwrap_err()
          .to_string()
          .contains("record size limit")
      );
    }
  }
}
