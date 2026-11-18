//! The text tools of the shell (`head`, `tail`, `wc`, `grep`): what they do
//! to a text, without files or printing, so the self-tests can cover it.

/// The first `n` lines (each with its line end, if it had one).
pub fn head(text: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    match text.match_indices('\n').nth(n - 1) {
        Some((at, _)) => &text[..at + 1],
        None => text,
    }
}

/// The last `n` lines (a line end at the very end does not start a new line).
pub fn tail(text: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    match body.rmatch_indices('\n').nth(n - 1) {
        Some((at, _)) => &text[at + 1..],
        None => text,
    }
}

/// Lines (counted as line ends, plus a last line without one), words, bytes.
pub fn count(text: &str) -> (usize, usize, usize) {
    let ends = text.bytes().filter(|b| *b == b'\n').count();
    let lines = ends + usize::from(!text.is_empty() && !text.ends_with('\n'));
    (lines, text.split_whitespace().count(), text.len())
}

/// Does `line` contain `pattern`? `ignore_case` for ASCII letters.
pub fn contains(line: &str, pattern: &str, ignore_case: bool) -> bool {
    if !ignore_case {
        return line.contains(pattern);
    }
    let (line, pattern) = (line.as_bytes(), pattern.as_bytes());
    pattern.is_empty() || line.windows(pattern.len()).any(|w| w.eq_ignore_ascii_case(pattern))
}

/// The lines that contain `pattern`: (line number from 1, the line).
pub fn grep<'a>(text: &'a str, pattern: &'a str, ignore_case: bool) -> impl Iterator<Item = (usize, &'a str)> + 'a {
    text.lines().enumerate().filter(move |(_, l)| contains(l, pattern, ignore_case)).map(|(i, l)| (i + 1, l))
}

/// Text or not? A file with a zero byte in its first 8 KiB is not.
pub fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|b| *b == 0)
}

/// `-n N` or `-N` in front of a file name: how many lines (default 10).
pub fn parse_count<'a>(args: &[&'a str]) -> Option<(usize, &'a str)> {
    match args {
        [file] => Some((10, file)),
        ["-n", n, file] => Some((n.parse().ok()?, file)),
        [flag, file] if flag.starts_with('-') => Some((flag[1..].parse().ok()?, file)),
        _ => None,
    }
}
