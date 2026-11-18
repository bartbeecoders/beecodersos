//! HTML to lines of text, for the web page viewer (`browse`).
//!
//! No style sheets, no scripts, no pictures: the text of a page, laid out
//! in lines of a given width, with headings, lists, links (numbered in the
//! order they appear) and preformatted blocks. A page is untrusted data:
//! everything is bounded (`MAX_LINES`, `MAX_LINKS`, the length of a link)
//! and nothing in it can make this fail; whatever cannot be understood is
//! shown as text or left out.

use alloc::string::String;
use alloc::vec::Vec;

/// A page is cut off after this many lines.
pub const MAX_LINES: usize = 20_000;
/// Links after this many are shown as plain text.
pub const MAX_LINKS: usize = 2_000;
/// A link address longer than this is left out (the text stays).
pub const MAX_HREF: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Text,
    Heading,
    Bold,
    Link,
    /// Preformatted text (`<pre>`, `<code>`).
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    /// The link this character belongs to: an index into `Page::links`.
    pub link: Option<u16>,
}

#[derive(Debug, Default)]
pub struct Page {
    pub title: String,
    pub lines: Vec<Vec<Cell>>,
    /// Link addresses as the page wrote them (relative ones included).
    pub links: Vec<String>,
    /// The page was longer than `MAX_LINES`.
    pub cut: bool,
}

impl Page {
    /// A line as plain text (for tests and for the host's view of a window).
    pub fn text(&self, line: usize) -> String {
        self.lines.get(line).map_or_else(String::new, |l| l.iter().map(|c| c.ch).collect())
    }

    /// The first line on which a link appears.
    pub fn line_of_link(&self, link: usize) -> Option<usize> {
        self.lines.iter().position(|l| l.iter().any(|c| c.link == Some(link as u16)))
    }
}

/// Plain text (not HTML), in lines of at most `width` characters.
pub fn plain(text: &str, width: usize) -> Page {
    let mut layout = Layout::new(width);
    layout.pre = 1;
    layout.style = Style::Text;
    layout.text(text);
    layout.finish()
}

/// Lay out an HTML page in lines of at most `width` characters.
pub fn render(html: &str, width: usize) -> Page {
    let mut layout = Layout::new(width);
    let mut rest = html;
    // Inside <script>, <style>, ...: skip to the closing tag.
    let mut skip_until: Option<&'static str> = None;
    let mut in_title = false;
    while !rest.is_empty() {
        if layout.full() {
            break;
        }
        let Some(open) = rest.find('<') else {
            if skip_until.is_none() {
                layout.text(&decode(rest));
            }
            break;
        };
        let (text, after) = rest.split_at(open);
        if !text.is_empty() && skip_until.is_none() {
            let text = decode(text);
            if in_title {
                if layout.page.title.len() < 200 {
                    layout.page.title.push_str(text.trim());
                }
            } else {
                layout.text(&text);
            }
        }
        rest = after;
        if let Some(body) = rest.strip_prefix("<!--") {
            rest = body.find("-->").map_or("", |end| &body[end + 3..]);
            continue;
        }
        let Some(close) = rest.find('>') else {
            // A '<' that opens nothing: show it as text.
            if skip_until.is_none() {
                layout.text(&decode(rest));
            }
            break;
        };
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        let Some(tag) = Tag::parse(tag) else {
            // "a < b": not a tag.
            if skip_until.is_none() {
                layout.text("<");
                layout.text(&decode(tag));
                layout.text(">");
            }
            continue;
        };
        if let Some(until) = skip_until {
            if tag.closing && tag.name == until {
                skip_until = None;
            }
            continue;
        }
        match (tag.name.as_str(), tag.closing) {
            ("script" | "style" | "template" | "svg" | "select" | "iframe" | "object", false) => {
                skip_until = Some(match tag.name.as_str() {
                    "script" => "script",
                    "style" => "style",
                    "template" => "template",
                    "svg" => "svg",
                    "select" => "select",
                    "iframe" => "iframe",
                    _ => "object",
                });
            }
            ("title", closing) => in_title = !closing,
            ("br", _) => layout.new_line(),
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "ul" | "ol" | "dl" | "blockquote" | "table" | "figure", false) => {
                layout.paragraph();
                match tag.name.as_str() {
                    "pre" => layout.pre += 1,
                    "ul" | "ol" | "dl" | "blockquote" => layout.indent += 1,
                    name if name.starts_with('h') => layout.heading += 1,
                    _ => {}
                }
            }
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "ul" | "ol" | "dl" | "blockquote" | "table" | "figure", true) => {
                match tag.name.as_str() {
                    "pre" => layout.pre = layout.pre.saturating_sub(1),
                    "ul" | "ol" | "dl" | "blockquote" => layout.indent = layout.indent.saturating_sub(1),
                    name if name.starts_with('h') => layout.heading = layout.heading.saturating_sub(1),
                    _ => {}
                }
                layout.paragraph();
            }
            ("li", false) => {
                layout.new_line();
                layout.bullet();
            }
            ("dt" | "dd" | "div" | "tr" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside" | "form" | "center"
            | "address" | "figcaption" | "li", _) => layout.new_line(),
            ("hr", _) => {
                layout.new_line();
                let width = layout.width.saturating_sub(layout.indent * 2).min(40);
                layout.raw_word(&"-".repeat(width), Style::Text);
                layout.new_line();
            }
            ("td" | "th", false) => layout.space(),
            ("b" | "strong", closing) => layout.bold = if closing { layout.bold.saturating_sub(1) } else { layout.bold + 1 },
            ("code" | "tt" | "kbd" | "samp", closing) => layout.code = if closing { layout.code.saturating_sub(1) } else { layout.code + 1 },
            ("a", false) => {
                layout.link = None;
                let href = tag.attr("href").map(|h| decode(h.trim()));
                if let Some(href) = href.filter(|h| !h.is_empty() && h.len() <= MAX_HREF && !h.starts_with('#')) {
                    if layout.page.links.len() < MAX_LINKS {
                        layout.link = Some(layout.page.links.len() as u16);
                        layout.page.links.push(href);
                    }
                }
            }
            ("a", true) => layout.link = None,
            ("img", _) => {
                if let Some(alt) = tag.attr("alt").map(decode).filter(|a| !a.trim().is_empty()) {
                    layout.text("[");
                    layout.text(alt.trim());
                    layout.text("]");
                }
            }
            _ => {}
        }
    }
    layout.finish()
}

/// A tag: `<name attrs>` or `</name>`; `None` if it is not one.
struct Tag<'a> {
    name: String,
    closing: bool,
    attrs: &'a str,
}

impl<'a> Tag<'a> {
    fn parse(inside: &'a str) -> Option<Tag<'a>> {
        let (closing, inside) = match inside.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, inside),
        };
        if inside.starts_with('!') || inside.starts_with('?') {
            // <!DOCTYPE ...>, <?xml ...?>: a tag that does nothing.
            return Some(Tag { name: String::new(), closing, attrs: "" });
        }
        let end = inside.find(|c: char| c.is_ascii_whitespace() || c == '/').unwrap_or(inside.len());
        let name = &inside[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !name.as_bytes()[0].is_ascii_alphabetic() {
            return None;
        }
        Some(Tag { name: name.to_ascii_lowercase(), closing, attrs: &inside[end..] })
    }

    /// The value of an attribute (`name="v"`, `name='v'` or `name=v`).
    fn attr(&self, wanted: &str) -> Option<&'a str> {
        let mut rest = self.attrs;
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
            if rest.is_empty() {
                return None;
            }
            let end = rest.find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/').unwrap_or(rest.len());
            let (name, after) = rest.split_at(end);
            let after = after.trim_start();
            let (value, next) = match after.strip_prefix('=') {
                Some(v) => {
                    let v = v.trim_start();
                    match v.chars().next() {
                        Some(q @ ('"' | '\'')) => {
                            let body = &v[1..];
                            let close = body.find(q).unwrap_or(body.len());
                            (&body[..close], body.get(close + 1..).unwrap_or(""))
                        }
                        _ => {
                            let close = v.find(|c: char| c.is_ascii_whitespace()).unwrap_or(v.len());
                            (&v[..close], &v[close..])
                        }
                    }
                }
                None => ("", after),
            };
            if name.eq_ignore_ascii_case(wanted) {
                return Some(value);
            }
            if next.len() >= rest.len() {
                return None; // no progress: give up
            }
            rest = next;
        }
    }
}

/// Replace character references: `&amp;`, `&#233;`, `&#xE9;` and the
/// common named ones. Unknown ones stay as they are.
pub fn decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !c.is_ascii_alphanumeric() && c != '#').map_or(rest.len(), |i| i + 1);
        let name = &rest[1..end];
        let found = if let Some(number) = name.strip_prefix('#') {
            let value = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => number.parse::<u32>().ok(),
            };
            value.and_then(char::from_u32).filter(|c| *c != '\0')
        } else {
            named(name)
        };
        match found {
            Some(c) => {
                out.push(c);
                rest = rest.get(end..).unwrap_or("");
                rest = rest.strip_prefix(';').unwrap_or(rest);
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named(name: &str) -> Option<char> {
    Some(match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{A0}',
        "copy" => '©',
        "reg" => '®',
        "deg" => '°',
        "middot" => '·',
        "bull" => '•',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "laquo" => '«',
        "raquo" => '»',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "euro" => '€',
        "pound" => '£',
        "times" => '×',
        "eacute" => 'é',
        "egrave" => 'è',
        "euml" => 'ë',
        "agrave" => 'à',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "ccedil" => 'ç',
        "szlig" => 'ß',
        _ => return None,
    })
}

struct Layout {
    page: Page,
    width: usize,
    line: Vec<Cell>,
    /// A space is owed before the next word on this line.
    space: bool,
    /// Lines still to add before the next text (a paragraph's gap), at most 1.
    gap: bool,
    pre: usize,
    indent: usize,
    heading: usize,
    bold: usize,
    code: usize,
    link: Option<u16>,
    /// Fixed for `plain`; otherwise from the counters above.
    style: Style,
}

impl Layout {
    fn new(width: usize) -> Layout {
        Layout {
            page: Page::default(),
            width: width.max(10),
            line: Vec::new(),
            space: false,
            gap: false,
            pre: 0,
            indent: 0,
            heading: 0,
            bold: 0,
            code: 0,
            link: None,
            style: Style::Text,
        }
    }

    fn full(&self) -> bool {
        self.page.lines.len() >= MAX_LINES
    }

    fn current_style(&self) -> Style {
        if self.link.is_some() {
            Style::Link
        } else if self.heading > 0 {
            Style::Heading
        } else if self.pre > 0 || self.code > 0 {
            Style::Code
        } else if self.bold > 0 {
            Style::Bold
        } else {
            self.style
        }
    }

    fn push_line(&mut self) {
        if self.full() {
            self.page.cut = true;
            self.line.clear();
            return;
        }
        let line = core::mem::take(&mut self.line);
        self.page.lines.push(line);
        self.space = false;
    }

    /// End the line, if anything is on it.
    fn new_line(&mut self) {
        if !self.line.is_empty() {
            self.push_line();
        }
        self.space = false;
    }

    /// End the line and leave an empty one before what comes next.
    fn paragraph(&mut self) {
        self.new_line();
        self.gap = !self.page.lines.is_empty();
    }

    fn space(&mut self) {
        if !self.line.is_empty() {
            self.space = true;
        }
    }

    fn start_text(&mut self) {
        if self.gap {
            self.gap = false;
            if self.page.lines.last().is_some_and(|l| !l.is_empty()) {
                self.push_line();
            }
        }
        if self.line.is_empty() {
            for _ in 0..(self.indent * 2).min(self.width / 2) {
                self.line.push(Cell { ch: ' ', style: Style::Text, link: None });
            }
        }
    }

    /// A list item's mark, indented as deep as its list.
    fn bullet(&mut self) {
        self.raw_word("*", Style::Text);
        self.space = true;
    }

    /// Text from the page: words separated by white space (as it is, in `pre`).
    fn text(&mut self, text: &str) {
        if self.pre > 0 {
            for (i, piece) in text.split('\n').enumerate() {
                if i > 0 {
                    self.start_text();
                    self.push_line();
                }
                let piece: String = piece.chars().map(|c| if c == '\t' { ' ' } else { c }).filter(|c| *c != '\r').collect();
                if !piece.is_empty() {
                    self.start_text();
                    for c in piece.chars() {
                        if self.line.len() >= self.width {
                            self.push_line();
                        }
                        let style = self.current_style();
                        self.line.push(Cell { ch: c, style, link: self.link });
                    }
                }
                if self.full() {
                    return;
                }
            }
            return;
        }
        if text.starts_with(|c: char| c.is_whitespace() && c != '\u{A0}') {
            self.space();
        }
        let mut words = text.split(|c: char| c.is_whitespace() && c != '\u{A0}').filter(|w| !w.is_empty()).peekable();
        while let Some(word) = words.next() {
            let style = self.current_style();
            self.word(word, style);
            if words.peek().is_some() {
                self.space = true;
            }
            if self.full() {
                return;
            }
        }
        if text.ends_with(|c: char| c.is_whitespace() && c != '\u{A0}') {
            self.space();
        }
    }

    /// One word, on this line if it fits, otherwise on the next; a word
    /// longer than a line is cut.
    fn word(&mut self, word: &str, style: Style) {
        self.start_text();
        let len = word.chars().count();
        let used = self.line.len() + usize::from(self.space);
        let indent = (self.indent * 2).min(self.width / 2);
        if used + len > self.width && self.line.len() > indent {
            self.push_line();
            self.start_text();
        }
        if self.space && self.line.len() > indent {
            // The space before a link belongs to the text, not to the link.
            self.line.push(Cell { ch: ' ', style: Style::Text, link: None });
        }
        self.space = false;
        for c in word.chars() {
            if self.line.len() >= self.width {
                self.push_line();
                self.start_text();
            }
            self.line.push(Cell { ch: if c == '\u{A0}' { ' ' } else { c }, style, link: self.link });
        }
    }

    fn raw_word(&mut self, word: &str, style: Style) {
        let link = self.link.take();
        self.word(word, style);
        self.link = link;
    }

    fn finish(mut self) -> Page {
        self.new_line();
        while self.page.lines.last().is_some_and(|l| l.is_empty()) {
            self.page.lines.pop();
        }
        self.page
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(page: &Page) -> Vec<String> {
        (0..page.lines.len()).map(|i| page.text(i)).collect()
    }

    #[test]
    fn paragraphs_headings_and_wrapping() {
        let page = render("<html><head><title>A &amp; B</title><style>p{color:red}</style></head>\
            <body><h1>Hello</h1><p>one two three four five six seven</p><p>end</p></body></html>", 20);
        assert_eq!(page.title, "A & B");
        assert_eq!(lines(&page), ["Hello", "", "one two three four", "five six seven", "", "end"]);
        assert_eq!(page.lines[0][0].style, Style::Heading);
        assert_eq!(page.lines[2][0].style, Style::Text);
        assert!(lines(&page).iter().all(|l| l.chars().count() <= 20));
    }

    #[test]
    fn links_are_numbered_and_marked() {
        let page = render("Go <a href=\"/a.html\">there</a> or <A HREF='b.html'>here <b>now</b></a>. <a href=#top>top</a> <a>none</a>", 80);
        assert_eq!(page.links, ["/a.html", "b.html"]);
        assert_eq!(lines(&page), ["Go there or here now. top none"]);
        let line = &page.lines[0];
        assert_eq!(line[3].link, Some(0));
        assert_eq!(line[2].link, None); // the space before it
        assert_eq!(line[12].link, Some(1));
        assert_eq!(line[17].link, Some(1)); // "now" in bold, still the link
        assert_eq!(line[17].style, Style::Link);
        assert_eq!(line[22].link, None); // "#top" goes nowhere
        assert_eq!(page.line_of_link(1), Some(0));
        assert_eq!(page.line_of_link(2), None);
    }

    #[test]
    fn lists_pre_and_rules() {
        let page = render("<ul><li>one<li>two</ul><pre>a  b\n  c</pre><hr>x<br>y", 30);
        assert_eq!(lines(&page), ["  * one", "  * two", "", "a  b", "  c", "", "-".repeat(30).as_str(), "x", "y"]);
        assert_eq!(page.lines[4][0].style, Style::Code);
    }

    #[test]
    fn scripts_comments_and_entities() {
        let page = render("<script>if (a < b) document.write('<p>no</p>')</script><!-- <p>hidden</p> -->\
            caf&eacute; &#233;&#xE9; &lt;tag&gt; &unknown; 5 < 6 &", 80);
        assert_eq!(lines(&page), ["café éé <tag> &unknown; 5 < 6 &"]);
    }

    #[test]
    fn long_words_and_plain_text() {
        let page = render(&"x".repeat(25), 10);
        assert_eq!(lines(&page), ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]);
        let page = plain("line one\n\n  indented\tand tab\n", 80);
        assert_eq!(lines(&page), ["line one", "", "  indented and tab"]);
        assert!(page.links.is_empty());
    }

    #[test]
    fn images_and_tables() {
        let page = render("<img src=x.png alt=\"Logo\"><img src=y.png><table><tr><td>a<td>b<tr><td>c</table>", 80);
        assert_eq!(lines(&page), ["[Logo]", "", "a b", "c"]);
    }

    #[test]
    fn hostile_input_is_bounded() {
        // Cut off anywhere, the parser never fails.
        let html = "<html><title>t</title><a href='x'>link</a> &#x110000; &#0; <pre>p</pre><!-- open <a href=\"";
        for end in 0..=html.len() {
            if html.is_char_boundary(end) {
                let _ = render(&html[..end], 12);
            }
        }
        // Endless lines stop at MAX_LINES; endless links at MAX_LINKS.
        let page = render(&"<br>a".repeat(MAX_LINES + 50), 80);
        assert_eq!(page.lines.len(), MAX_LINES);
        let page = render(&"<a href=x>l</a> ".repeat(MAX_LINKS + 10), 80);
        assert_eq!(page.links.len(), MAX_LINKS);
        // A narrow window still gets whole lines of at most its width.
        let page = render("<ul><ul><ul><ul><ul><ul><li>deep text here</ul></ul></ul></ul></ul></ul>", 3);
        assert!(lines(&page).iter().all(|l| l.chars().count() <= 10));
        let href = format!("<a href='{}'>long</a>", "y".repeat(MAX_HREF + 1));
        assert!(render(&href, 80).links.is_empty());
    }
}
