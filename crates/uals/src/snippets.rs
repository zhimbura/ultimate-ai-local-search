//! Reads the snippet lines of one page of hits from disk.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use tracing::{debug, warn};
use uals_core::config::OutputConfig;
use uals_core::render::{Format, MAX_LINE_CHARS, snippet_range};
use uals_core::types::{FusedHit, LineRange, Snippet};

/// The most bytes kept of one line; the rest of a longer line is skipped unread into memory, so
/// a minified file is never loaded whole. UTF-8 takes at most 4 bytes per character, so the
/// bytes kept hold more than `MAX_LINE_CHARS` characters and the cut still shows.
const MAX_LINE_BYTES: u64 = MAX_LINE_CHARS as u64 * 4 + 4;

/// Sets `snippet` on every hit to the lines `render::snippet_range` picks. A hit whose file cannot
/// be read, or is shorter than the range start, keeps `None`.
pub(crate) async fn fill(hits: &mut [FusedHit], format: Format, config: &OutputConfig) {
    let wanted: Vec<(PathBuf, LineRange)> =
        hits.iter().map(|hit| (hit.path.clone(), snippet_range(hit, format, config))).collect();
    match tokio::task::spawn_blocking(move || read_ranges(&wanted)).await {
        Ok(snippets) => {
            for (hit, snippet) in hits.iter_mut().zip(snippets) {
                hit.snippet = snippet;
            }
        }
        Err(error) => warn!(%error, "reading snippets failed; the page goes out without them"),
    }
}

/// Reads every `(file, lines)` pair. Each file is opened once and read only up to the last line
/// any pair needs.
fn read_ranges(wanted: &[(PathBuf, LineRange)]) -> Vec<Option<Snippet>> {
    let mut last_line: HashMap<&Path, u32> = HashMap::new();
    for (path, lines) in wanted {
        let last = last_line.entry(path.as_path()).or_insert(0);
        *last = (*last).max(lines.end);
    }
    let files: HashMap<&Path, Vec<String>> = last_line
        .into_iter()
        .filter_map(|(path, last)| match read_lines(path, last) {
            Ok(lines) => Some((path, lines)),
            Err(error) => {
                debug!(path = %path.display(), %error, "cannot read a snippet");
                None
            }
        })
        .collect();
    wanted.iter().map(|(path, lines)| files.get(path.as_path()).and_then(|text| cut(text, *lines))).collect()
}

/// The first `last` lines of the file, without line endings, decoded lossily as UTF-8.
fn read_lines(path: &Path, last: u32) -> io::Result<Vec<String>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut lines = Vec::new();
    let mut buffer = Vec::new();
    while lines.len() < last as usize && read_line_capped(&mut reader, &mut buffer)? {
        lines.push(shorten(&String::from_utf8_lossy(&buffer)));
    }
    Ok(lines)
}

/// Reads the next line into `line` without its line ending, keeping at most `MAX_LINE_BYTES`
/// bytes and skipping the rest. Returns false at the end of the input.
fn read_line_capped(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    if reader.by_ref().take(MAX_LINE_BYTES).read_until(b'\n', line)? == 0 {
        return Ok(false);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    } else {
        skip_rest_of_line(reader)?;
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(true)
}

/// Consumes `reader` up to and including the next `\n` without keeping the bytes.
fn skip_rest_of_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        match available.iter().position(|&byte| byte == b'\n') {
            Some(end) => {
                reader.consume(end + 1);
                return Ok(());
            }
            None => {
                let length = available.len();
                reader.consume(length);
            }
        }
    }
}

/// The lines of `range` that exist in `lines` (1-based), or `None` when the file ends before
/// the range starts.
fn cut(lines: &[String], range: LineRange) -> Option<Snippet> {
    let start = range.start.max(1);
    let end = range.end.min(u32::try_from(lines.len()).unwrap_or(u32::MAX));
    if start > end {
        return None;
    }
    let text = lines.get(start as usize - 1..end as usize)?.join("\n");
    Some(Snippet { lines: LineRange::new(start, end), text })
}

/// `line` cut to `MAX_LINE_CHARS` characters, with `…` marking the cut.
fn shorten(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut_at, _)) => format!("{}…", &line[..cut_at]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use pretty_assertions::assert_eq;

    use super::*;

    fn file_with(content: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(content).expect("write temp file");
        file
    }

    fn snippet(start: u32, end: u32, text: &str) -> Option<Snippet> {
        Some(Snippet { lines: LineRange::new(start, end), text: text.into() })
    }

    #[test]
    fn reads_the_range_and_stops_at_the_end_of_the_file() {
        let file = file_with(b"one\r\ntwo\nthree\nfour");
        let path = file.path().to_path_buf();
        let read = read_ranges(&[
            (path.clone(), LineRange::new(2, 3)),
            (path.clone(), LineRange::new(3, 9)),
            (path.clone(), LineRange::new(7, 9)),
        ]);
        assert_eq!(read, vec![snippet(2, 3, "two\nthree"), snippet(3, 4, "three\nfour"), None]);
    }

    #[test]
    fn a_missing_file_gives_no_snippet() {
        let read = read_ranges(&[(PathBuf::from("/nonexistent/uals/File.kt"), LineRange::new(1, 3))]);
        assert_eq!(read, vec![None]);
    }

    #[test]
    fn a_huge_line_keeps_only_its_start_and_the_next_line_still_reads() {
        let mut content = "x".repeat(1_000_000).into_bytes();
        content.extend_from_slice(b"\r\nnext\n");
        let mut reader = io::Cursor::new(content);
        let mut line = Vec::new();

        let first = read_line_capped(&mut reader, &mut line).expect("first line");
        let kept = line.len();
        let second = read_line_capped(&mut reader, &mut line).expect("second line");
        let next = String::from_utf8_lossy(&line).into_owned();
        let end = read_line_capped(&mut reader, &mut line).expect("end");

        assert_eq!((first, kept), (true, MAX_LINE_BYTES as usize));
        assert_eq!((second, next.as_str(), end), (true, "next", false));
    }

    #[test]
    fn long_lines_are_cut_and_invalid_utf8_is_replaced() {
        let mut content = "x".repeat(MAX_LINE_CHARS + 50).into_bytes();
        content.extend_from_slice(b"\nbad \xff byte\n");
        let file = file_with(&content);
        let read = read_ranges(&[(file.path().to_path_buf(), LineRange::new(1, 2))]);
        let expected = format!("{}…\nbad \u{fffd} byte", "x".repeat(MAX_LINE_CHARS));
        assert_eq!(read, vec![snippet(1, 2, &expected)]);
    }
}
