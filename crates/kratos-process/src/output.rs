//! Reading a child's output one line at a time within a budget (L3), so a child that writes an
//! endless line cannot take the engine's memory.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// One line a child wrote, without its newline.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Data(Vec<u8>),
    /// A line longer than the budget, read and dropped. It holds the line's length in bytes.
    Oversized(usize),
    /// The output ended.
    Eof,
}

/// Reads the next line, keeping at most `max_line_bytes` of it. A longer line is consumed to its
/// end and reported as [`Line::Oversized`], so the line after it reads normally.
///
/// # Errors
/// Returns the reader's error.
pub async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_line_bytes: usize,
) -> io::Result<Line> {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(buffer.len());
        bytes = bytes.saturating_add(count);
        if bytes <= max_line_bytes {
            kept.extend_from_slice(&buffer[..count]);
        }
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(ended(kept, bytes, max_line_bytes));
        }
    }
    Ok(if bytes == 0 {
        Line::Eof
    } else {
        ended(kept, bytes, max_line_bytes)
    })
}

fn ended(kept: Vec<u8>, bytes: usize, max_line_bytes: usize) -> Line {
    if bytes > max_line_bytes {
        Line::Oversized(bytes)
    } else {
        Line::Data(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_line_over_the_budget_is_reported_and_the_next_line_reads() {
        // Catches an oversized line kept in memory, or one that swallows the line after it.
        let input = format!("short\n{}\nafter\nlast", "x".repeat(40));
        let mut reader = tokio::io::BufReader::with_capacity(8, input.as_bytes());
        let mut lines = Vec::new();
        loop {
            let line = read_line(&mut reader, 16).await.unwrap();
            if line == Line::Eof {
                break;
            }
            lines.push(line);
        }
        assert_eq!(
            lines,
            [
                Line::Data(b"short".to_vec()),
                Line::Oversized(40),
                Line::Data(b"after".to_vec()),
                Line::Data(b"last".to_vec()),
            ]
        );
    }
}
