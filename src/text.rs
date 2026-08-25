//! Loading a file into something diffable: read, sniff, decode, split into lines.
//!
//! Line text is stored with its terminator **stripped**, and the terminator's length kept
//! alongside. That is what makes inline byte offsets from the diff line up with exactly
//! the `&str` the renderer draws, and it keeps the original bytes reconstructible.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::model::LineNo;

/// How many bytes to inspect when deciding whether a file is binary.
pub const SNIFF_WINDOW: usize = 8192;

/// Chunk size for the streaming byte comparison.
const CHUNK: usize = 64 * 1024;

/// Default refusal threshold. A diff tool that declines a huge file is better than one
/// that dies on it.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bom {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

impl Bom {
    pub const fn byte_len(self) -> usize {
        match self {
            Self::Utf8 => 3,
            Self::Utf16Le | Self::Utf16Be => 2,
            Self::Utf32Le | Self::Utf32Be => 4,
        }
    }

    pub const fn is_utf8(self) -> bool {
        matches!(self, Self::Utf8)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Utf32Le => "UTF-32LE",
            Self::Utf32Be => "UTF-32BE",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryReason {
    /// A NUL byte inside the sniff window.
    Nul,
    /// A byte-order mark for an encoding yadt does not decode.
    Bom(Bom),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sniff {
    Text { bom: Option<Bom> },
    Binary(BinaryReason),
}

/// Which line terminators a file uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EolStyle {
    /// No terminators at all (a single unterminated line, or an empty file).
    None,
    Lf,
    Crlf,
    Mixed,
}

impl EolStyle {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
            Self::Mixed => "mixed",
        }
    }
}

/// Byte range of one line inside [`TextFile::text`].
///
/// `end` excludes the terminator; `eol_len` (0, 1 or 2) records how long it was, so the
/// exact original bytes stay reconstructible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
    pub eol_len: u8,
}

#[derive(Clone, Debug)]
pub struct TextFile {
    pub path: PathBuf,
    /// Decoded contents, BOM stripped. Terminators are still present in this buffer; the
    /// line ranges exclude them.
    pub text: String,
    pub lines: Vec<LineRange>,
    pub eol: EolStyle,
    /// False when the last line has no terminator.
    pub final_newline: bool,
    pub had_invalid_utf8: bool,
    pub bom: Option<Bom>,
    pub size: u64,
}

impl TextFile {
    /// One line, terminator stripped.
    pub fn line(&self, n: LineNo) -> &str {
        self.lines
            .get(n.0 as usize)
            .map_or("", |r| &self.text[r.start as usize..r.end as usize])
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// All lines as slices, for feeding to the diff algorithm.
    pub fn slices(&self) -> Vec<&str> {
        self.lines
            .iter()
            .map(|r| &self.text[r.start as usize..r.end as usize])
            .collect()
    }

    /// Display name for the pane header.
    pub fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// What one side of a comparison turned out to hold.
#[derive(Clone, Debug)]
pub enum FileContent {
    Text(Box<TextFile>),
    Binary {
        path: PathBuf,
        size: u64,
        reason: BinaryReason,
        /// Kept so the file can be shown as hex. Bounded by `--max-file-size`, the same
        /// guard that bounds a text file's decoded contents.
        bytes: Vec<u8>,
    },
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    Unreadable {
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
}

impl FileContent {
    /// The raw bytes of a binary side, for the hex view.
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Self::Binary { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&TextFile> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Text(t) => &t.path,
            Self::Binary { path, .. }
            | Self::TooLarge { path, .. }
            | Self::Unreadable { path, .. } => path,
        }
    }
}

/// Detect a byte-order mark.
///
/// Order matters: UTF-32LE starts with the UTF-16LE mark, so it must be tested first.
pub fn detect_bom(bytes: &[u8]) -> Option<Bom> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(Bom::Utf8)
    } else if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        Some(Bom::Utf32Le)
    } else if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        Some(Bom::Utf32Be)
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Some(Bom::Utf16Le)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Some(Bom::Utf16Be)
    } else {
        None
    }
}

/// Decide whether these bytes are text.
///
/// This is deliberately hand-rolled rather than pulling in `content_inspector`, whose
/// entire implementation is the same BOM check plus a NUL scan, and which is unmaintained.
///
/// UTF-16 and UTF-32 land in `Binary` via their BOM. That is honest rather than ideal, and
/// is documented as a limitation.
pub fn sniff(bytes: &[u8]) -> Sniff {
    if let Some(bom) = detect_bom(bytes) {
        return if bom.is_utf8() {
            Sniff::Text { bom: Some(bom) }
        } else {
            Sniff::Binary(BinaryReason::Bom(bom))
        };
    }
    let window = &bytes[..bytes.len().min(SNIFF_WINDOW)];
    if window.contains(&0) {
        Sniff::Binary(BinaryReason::Nul)
    } else {
        Sniff::Text { bom: None }
    }
}

/// Split `text` into lines.
///
/// Recognises `\n` and `\r\n`. A lone `\r` is *not* a terminator in v1 — it stays in the
/// line and renders as a control picture. A trailing terminator does not produce a final
/// empty line; `final_newline` records it instead.
pub fn split_lines(text: &str) -> (Vec<LineRange>, EolStyle, bool) {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let (mut saw_lf, mut saw_crlf) = (false, false);
    let mut start = 0usize;

    for i in 0..bytes.len() {
        if bytes[i] != b'\n' {
            continue;
        }
        let crlf = i > start && bytes[i - 1] == b'\r';
        let end = if crlf { i - 1 } else { i };
        lines.push(LineRange {
            start: start as u32,
            end: end as u32,
            eol_len: if crlf { 2 } else { 1 },
        });
        if crlf {
            saw_crlf = true;
        } else {
            saw_lf = true;
        }
        start = i + 1;
    }

    let final_newline = start == bytes.len();
    if !final_newline {
        lines.push(LineRange {
            start: start as u32,
            end: bytes.len() as u32,
            eol_len: 0,
        });
    }

    let eol = match (saw_lf, saw_crlf) {
        (false, false) => EolStyle::None,
        (true, false) => EolStyle::Lf,
        (false, true) => EolStyle::Crlf,
        (true, true) => EolStyle::Mixed,
    };
    (lines, eol, final_newline)
}

/// Build a [`TextFile`] from already-read bytes.
pub fn decode(path: &Path, bytes: Vec<u8>, bom: Option<Bom>) -> TextFile {
    let size = bytes.len() as u64;
    let body = &bytes[bom.map_or(0, Bom::byte_len)..];
    let decoded = String::from_utf8_lossy(body);
    let had_invalid_utf8 = matches!(decoded, std::borrow::Cow::Owned(_));
    let text = decoded.into_owned();
    let (lines, eol, final_newline) = split_lines(&text);

    TextFile {
        path: path.to_path_buf(),
        text,
        lines,
        eol,
        final_newline,
        had_invalid_utf8,
        bom,
        size,
    }
}

/// Read one side of a comparison.
pub fn load(path: &Path, max_size: u64) -> FileContent {
    let unreadable = |e: io::Error| FileContent::Unreadable {
        path: path.to_path_buf(),
        kind: e.kind(),
        message: e.to_string(),
    };

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return unreadable(e),
    };
    if meta.len() > max_size {
        return FileContent::TooLarge {
            path: path.to_path_buf(),
            size: meta.len(),
            limit: max_size,
        };
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return unreadable(e),
    };

    match sniff(&bytes) {
        Sniff::Binary(reason) => FileContent::Binary {
            path: path.to_path_buf(),
            size: bytes.len() as u64,
            reason,
            bytes,
        },
        Sniff::Text { bom } => FileContent::Text(Box::new(decode(path, bytes, bom))),
    }
}

/// Are these two files byte-identical?
///
/// The ladder is cheapest-first: a size mismatch answers without reading anything, and
/// only equal-sized files are ever streamed. mtime is deliberately not consulted — it is
/// unreliable across checkouts — and equal size is never taken as equal content.
pub fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    if ma.len() == 0 {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (ma.dev(), ma.ino()) == (mb.dev(), mb.ino()) {
            return Ok(true);
        }
    }
    stream_eq(a, b)
}

fn stream_eq(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let mut buf_a = vec![0u8; CHUNK];
    let mut buf_b = vec![0u8; CHUNK];

    loop {
        let na = fill(&mut fa, &mut buf_a)?;
        let nb = fill(&mut fb, &mut buf_b)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}

/// Read until the buffer is full or EOF, so both sides compare aligned chunks even when
/// the underlying reads come back short.
fn fill(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(text: &str) -> Vec<&str> {
        let (ranges, _, _) = split_lines(text);
        ranges
            .iter()
            .map(|r| &text[r.start as usize..r.end as usize])
            .collect()
    }

    #[test]
    fn empty_file_has_no_lines() {
        let (lines, eol, final_newline) = split_lines("");
        assert!(lines.is_empty());
        assert_eq!(eol, EolStyle::None);
        // Nothing is missing a terminator, so nothing is flagged.
        assert!(final_newline);
    }

    #[test]
    fn a_bare_newline_is_one_empty_line() {
        let (lines, eol, final_newline) = split_lines("\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            LineRange {
                start: 0,
                end: 0,
                eol_len: 1
            }
        );
        assert_eq!(eol, EolStyle::Lf);
        assert!(final_newline);
    }

    #[test]
    fn unterminated_last_line_is_flagged() {
        let (lines, eol, final_newline) = split_lines("a");
        assert_eq!(lines_of("a"), vec!["a"]);
        assert_eq!(lines[0].eol_len, 0);
        assert_eq!(eol, EolStyle::None);
        assert!(!final_newline);

        let (_, _, final_newline) = split_lines("a\nb");
        assert!(!final_newline);
        assert_eq!(lines_of("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn trailing_newline_does_not_create_an_empty_line() {
        assert_eq!(lines_of("a\n"), vec!["a"]);
        assert_eq!(lines_of("a\nb\n"), vec!["a", "b"]);
    }

    #[test]
    fn crlf_is_stripped_and_recorded() {
        let (lines, eol, _) = split_lines("a\r\nb\r\n");
        assert_eq!(lines_of("a\r\nb\r\n"), vec!["a", "b"]);
        assert_eq!(lines[0].eol_len, 2);
        assert_eq!(eol, EolStyle::Crlf);
    }

    #[test]
    fn mixed_terminators_are_reported_as_mixed() {
        let (_, eol, _) = split_lines("a\r\nb\n");
        assert_eq!(eol, EolStyle::Mixed);
    }

    #[test]
    fn a_lone_cr_is_not_a_terminator() {
        assert_eq!(lines_of("a\rb\n"), vec!["a\rb"]);
    }

    #[test]
    fn empty_crlf_line() {
        let (lines, eol, _) = split_lines("\r\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            LineRange {
                start: 0,
                end: 0,
                eol_len: 2
            }
        );
        assert_eq!(eol, EolStyle::Crlf);
    }

    #[test]
    fn nul_at_the_sniff_window_boundary() {
        // The window is the first SNIFF_WINDOW bytes, so the last byte it covers is
        // index SNIFF_WINDOW - 1. This boundary is a genuine off-by-one.
        let mut bytes = vec![b'a'; SNIFF_WINDOW + 16];
        bytes[SNIFF_WINDOW - 1] = 0;
        assert_eq!(sniff(&bytes), Sniff::Binary(BinaryReason::Nul));

        let mut bytes = vec![b'a'; SNIFF_WINDOW + 16];
        bytes[SNIFF_WINDOW] = 0;
        assert_eq!(sniff(&bytes), Sniff::Text { bom: None });
    }

    #[test]
    fn plain_text_sniffs_as_text() {
        assert_eq!(sniff(b"hello\nworld\n"), Sniff::Text { bom: None });
        assert_eq!(sniff(b""), Sniff::Text { bom: None });
    }

    #[test]
    fn utf8_bom_is_text_others_are_binary() {
        assert_eq!(
            sniff(b"\xEF\xBB\xBFhi"),
            Sniff::Text {
                bom: Some(Bom::Utf8)
            }
        );
        assert_eq!(
            sniff(b"\xFF\xFEh\x00i\x00"),
            Sniff::Binary(BinaryReason::Bom(Bom::Utf16Le))
        );
        // UTF-32LE starts with the UTF-16LE mark; order of the checks matters.
        assert_eq!(
            sniff(b"\xFF\xFE\x00\x00h\x00\x00\x00"),
            Sniff::Binary(BinaryReason::Bom(Bom::Utf32Le))
        );
    }

    #[test]
    fn decode_strips_the_bom_and_reports_lossy_input() {
        let f = decode(
            Path::new("x"),
            b"\xEF\xBB\xBFa\nb\n".to_vec(),
            Some(Bom::Utf8),
        );
        assert_eq!(f.line_count(), 2);
        assert_eq!(f.line(LineNo(0)), "a");
        assert!(!f.had_invalid_utf8);

        let f = decode(Path::new("x"), b"a\xC3\x28b".to_vec(), None);
        assert!(f.had_invalid_utf8);
    }

    #[test]
    fn slices_match_the_line_accessor() {
        let f = decode(Path::new("x"), b"alpha\nbeta\ngamma".to_vec(), None);
        assert_eq!(f.slices(), vec!["alpha", "beta", "gamma"]);
        assert_eq!(f.line(LineNo(2)), "gamma");
        assert!(!f.final_newline);
    }
}
