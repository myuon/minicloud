//! A file of a deploy cut into coloured pieces, for the code window
//! ([`crate::code`], issue 100).
//!
//! **This answers pieces, never HTML.** A piece is a kind and a run of text,
//! and the admin app wraps each one in the `<span>` its stylesheet colours.
//! Every other value those pages show goes through their `escapeHtml`, and a
//! host that returned markup would move the escaping of the one value most
//! likely to *be* markup — a file holding `<img src=y>` — out of the place
//! that does it for everything else. One escape, in one place, is the whole
//! of why this crosses the boundary as data.
//!
//! **`.cove` is lexed, not guessed.** [`cove`] calls [`lex_recovered`],
//! which is the lexer
//! the host compiled the file with, so a colour here cannot disagree with
//! what the host runs — and the first time the language gains a keyword, a
//! literal or an operator, this gains it too. A hand-written approximation
//! would have been wrong from that moment on and gone on looking right.
//! Diagnostics are ignored and the tokens are used: a file that does not lex
//! is exactly the file somebody is looking at, and `lex_recovered` answers
//! what it got.
//!
//! **The gaps are where the comments are.** The lexer keeps a `///` as a
//! [`TokenKind::DocComment`] and drops every other comment, recording only
//! that a line break happened (`cove_syntax::token::Token`). So an ordinary
//! `//` is not a token and lives in the gap between two spans, and this walks
//! the gaps to find it. That has a second effect worth more than the first:
//! because the text between two spans is emitted whatever is in it, **no byte
//! of a file is dropped** — including whatever a lexical error made the lexer
//! skip. `crate::code`'s tests check that the pieces concatenated are the
//! file, byte for byte, which is the property every scanner here is written
//! to keep.
//!
//! The other languages a deploy carries have no lexer to borrow, so they have
//! scanners: [`toml()`] for `app.toml`, which is the file people come to
//! read, and [`css`] and [`xml`] for the assets that are text. Each is written the
//! same way — it either recognises the run in front of it or advances a byte
//! into the `plain` run between — so each keeps the same property for free.
//! A file no scanner here claims is answered by [`scanned`] as `None` and
//! shown plain: a colouring guessed from a name is a claim about the file,
//! and no colour is no claim.

use cove_diag::{FileId, SourceMap};
use cove_syntax::lexer::lex_recovered;
use cove_syntax::token::TokenKind;

/// One run of a file's text and what it is.
pub struct Piece {
    /// One of the kinds below.
    pub kind: &'static str,
    /// The text itself, which is the file's bytes and not an escaping of
    /// them: whoever shows it escapes it.
    pub text: String,
}

/// Text with no colour of its own: whitespace, an ordinary identifier, and
/// every byte of a file read by no scanner here.
///
/// The vocabulary is deliberately short — nine kinds for four languages —
/// so that one set of rules in the admin app's stylesheet colours all of
/// them. A kind is a *role*, which is why a Cove type name, a CSS selector
/// and an XML tag are all [`TYPE`]: they are the name the language gives
/// meaning to, and they want the same colour in all three.
pub const PLAIN: &str = "plain";
/// A word of the language: `fn`, `true`, a TOML `true`, a CSS at-rule.
pub const KEYWORD: &str = "keyword";
/// The name of a thing: a Cove type, a TOML table, a CSS selector, an XML
/// tag.
pub const TYPE: &str = "type";
/// The name of something being given a value: a TOML key, a CSS property,
/// an XML attribute.
pub const IDENT: &str = "ident";
/// A string literal, quotes included.
pub const STRING: &str = "string";
/// A number, and anything written like one: a duration, a date, a CSS
/// length, a hex colour.
pub const NUMBER: &str = "number";
/// A comment.
pub const COMMENT: &str = "comment";
/// A documentation comment, which is prose a reader wants to find.
pub const DOC: &str = "doc";
/// Punctuation and operators.
pub const PUNCT: &str = "punct";

/// What a scanner is building.
#[derive(Default)]
struct Pieces {
    out: Vec<Piece>,
}

impl Pieces {
    /// `text` as one piece, joined onto the last where that one is the same
    /// kind.
    ///
    /// Two pieces of one kind are one `<span>` worth of meaning and twice
    /// the HTML, and a run of punctuation or of whitespace is the common
    /// case — so joining them here is most of what keeps a file's piece
    /// count near its line count rather than near its byte count. An empty
    /// run is nothing at all, which is what lets every scanner push the gap
    /// in front of a run without first asking whether there was one.
    fn push(&mut self, kind: &'static str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.out.last_mut() {
            if last.kind == kind {
                last.text.push_str(text);
                return;
            }
        }
        self.out.push(Piece {
            kind,
            text: text.to_string(),
        });
    }
}

/// `file` of `sources`, coloured by the lexer that compiled it.
///
/// The whole file is coloured, so the caller wanting part of one has to
/// colour the part rather than cut the colouring: the pieces are the text of
/// `sources.get(file)` and nothing else.
pub fn cove(sources: &SourceMap, file: FileId) -> Vec<Piece> {
    let text = sources.get(file).text.as_str();
    // The diagnostics are not an answer here. A file that does not lex is
    // the file somebody is looking at, and these are the tokens that pass
    // produced; what it skipped is in a gap and comes out plain.
    let (tokens, _) = lex_recovered(sources, file);
    let mut out = Pieces::default();
    let mut at = 0;
    for token in &tokens {
        // The end of file is an empty span at the end, which would be a
        // piece with nothing in it.
        if matches!(token.kind, TokenKind::Eof) {
            continue;
        }
        let (start, end) = (token.span.start as usize, token.span.end as usize);
        // A span that is not a forward step into this file is skipped rather
        // than trusted: the gap it would have ended is emitted by the next
        // token or by the tail below, so the text survives either way.
        if start < at || start > end || end > text.len() {
            continue;
        }
        trivia(&text[at..start], &mut out);
        out.push(kind_of(&token.kind), &text[start..end]);
        at = end;
    }
    trivia(&text[at..], &mut out);
    out.out
}

/// What one token is.
///
/// An identifier that starts with a capital is a [`TYPE`], which is a
/// *naming convention* and not something the lexer knows: Cove's types are
/// `String`, `Vector`, `Result`, and a value named with a capital is the only
/// thing this gets wrong. The alternative was to ask the compiler, which
/// means a colouring that needs an app to have been checked — and a refused
/// app is one whose code somebody wants to read.
fn kind_of(kind: &TokenKind) -> &'static str {
    match kind {
        // `true` and `false` are words of the language wherever they are
        // written, so they are coloured as the other words are.
        TokenKind::Keyword(_) | TokenKind::Bool(_) => KEYWORD,
        TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::Duration(_) => NUMBER,
        // One token spans the whole literal, interpolations included, so
        // `"at {name}"` is one string. The lexer re-parses an interpolation
        // from the text inside; what is on the page is the literal as it was
        // written. A tagged literal, `html"…"`, is one token with its tag, and
        // is a string the same way.
        TokenKind::Str(_) | TokenKind::Tagged { .. } => STRING,
        TokenKind::DocComment(_) => DOC,
        TokenKind::Ident(name) => {
            if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                TYPE
            } else {
                // Most of a file is ordinary names. Colouring them all would
                // leave nothing uncoloured for the colours to stand out
                // from, and spend a `<span>` on every one.
                PLAIN
            }
        }
        _ => PUNCT,
    }
}

/// The text between two tokens: whitespace, the comments the lexer dropped,
/// and whatever a lexical error made it skip.
///
/// Every byte of it is pushed, which is what makes the pieces the file.
fn trivia(text: &str, out: &mut Pieces) {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut plain = 0;
    while at < bytes.len() {
        // `//` is reached only for a comment the lexer dropped: it keeps a
        // `///` as a token of its own, and `////` is an ordinary comment to
        // it as well as to this.
        let end = if bytes[at..].starts_with(b"//") {
            text[at..].find('\n').map_or(text.len(), |found| at + found)
        } else if bytes[at..].starts_with(b"/*") {
            block_end(text, at)
        } else {
            // A continuation byte of a multi-byte character cannot be `/`,
            // so stepping a byte at a time never matches inside one and
            // never slices inside one either.
            at += 1;
            continue;
        };
        out.push(PLAIN, &text[plain..at]);
        out.push(COMMENT, &text[at..end]);
        at = end;
        plain = end;
    }
    out.push(PLAIN, &text[plain..]);
}

/// Where the `/* */` opening at `from` ends, or the end of the text for one
/// that is never closed.
///
/// Cove's block comments nest (`cove_syntax`'s `skip_block_comment`), so
/// these do: a comment that ended at the first `*/` would colour the code
/// after it as code the lexer never saw.
fn block_end(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = from + 2;
    let mut depth = 1u32;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"*/") {
            at += 2;
            depth -= 1;
            if depth == 0 {
                return at;
            }
        } else if bytes[at..].starts_with(b"/*") {
            at += 2;
            depth += 1;
        } else {
            at += 1;
        }
    }
    text.len()
}

/// `text` coloured by the scanner its name calls for, or `None` for a name
/// no scanner here claims.
///
/// `.cove` is not among them: it has the real lexer ([`cove`]), which needs
/// the source map the app was compiled with and so cannot be reached from a
/// name and a text alone.
pub fn scanned(name: &str, text: &str) -> Option<Vec<Piece>> {
    match extension(name) {
        "toml" => Some(toml(text)),
        "css" => Some(css(text)),
        // An SVG is XML, and the scanner is written for XML so that it is
        // the same one either way.
        "svg" | "xml" => Some(xml(text)),
        _ => None,
    }
}

/// The lower-case extension of `name`, or `""` for a name with none.
fn extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((_, extension)) if extension.chars().all(|c| c.is_ascii_lowercase()) => extension,
        _ => "",
    }
}

/// `text` scanned by `next`, which answers what the run at an offset is and
/// where it ends, or `None` for a byte that is nothing in particular.
///
/// The bytes `next` claims nothing about collect into the [`PLAIN`] runs
/// between the ones it does, so a scanner is written as "what can start
/// here" and nothing else — and every byte comes out either way, which is
/// the property that matters.
fn scan(
    text: &str,
    out: &mut Pieces,
    mut next: impl FnMut(&str, usize) -> Option<(&'static str, usize)>,
) {
    let mut at = 0;
    let mut plain = 0;
    while at < text.len() {
        match next(text, at) {
            Some((kind, end)) if end > at => {
                out.push(PLAIN, &text[plain..at]);
                out.push(kind, &text[at..end]);
                at = end;
                plain = end;
            }
            // Every offset `next` recognises something at is the offset of an
            // ASCII byte, so a step of one never lands inside a character in
            // a way that a later slice could cut.
            _ => at += 1,
        }
    }
    out.push(PLAIN, &text[plain..]);
}

/// Where the string quoted at `from` ends, past its closing quote.
///
/// A quote the line does not close ends at the line break rather than
/// running on: an unclosed quote is a file being edited, and a scanner that
/// swallowed the rest of it would colour the whole file as a string.
fn quoted_end(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let quote = bytes[from];
    let mut at = from + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\n' => return at,
            byte if byte == quote => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Where the run of `allowed` bytes starting at `from` ends.
fn run_end(text: &str, from: usize, allowed: impl Fn(u8) -> bool) -> usize {
    let bytes = text.as_bytes();
    let mut at = from;
    while at < bytes.len() && allowed(bytes[at]) {
        at += 1;
    }
    at
}

/// Where the text closed by `close` and opened at `from` ends, or the end of
/// the text for one that is never closed.
fn closed_end(text: &str, from: usize, close: &str) -> usize {
    text[from..]
        .find(close)
        .map_or(text.len(), |found| from + found + close.len())
}

/// Whether `byte` may start a name in TOML, CSS or XML.
fn name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'-'
}

/// Whether `byte` may continue one.
fn name_rest(byte: u8) -> bool {
    name_start(byte) || byte.is_ascii_digit()
}

/// `app.toml`, scanned a line at a time: the comments, the `[table]`
/// headers, the keys, the strings, the numbers and the two booleans.
///
/// A line is the unit because TOML's grammar is: a key, a header or a
/// comment is one line's worth, which makes the scanner small and keeps a
/// mistake on one line from spreading down the file. The cost is a
/// multi-line `"""` string, whose lines are read as though each were a line
/// of TOML — wrong in its colours, and still every byte of the file.
pub fn toml(text: &str) -> Vec<Piece> {
    let mut out = Pieces::default();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let body = body.strip_suffix('\r').unwrap_or(body);
        toml_line(body, &mut out);
        // The line break is the line's and belongs to nothing on it.
        out.push(PLAIN, &line[body.len()..]);
    }
    out.out
}

/// One line of TOML, the line break off it.
fn toml_line(line: &str, out: &mut Pieces) {
    let indent = line.len() - line.trim_start().len();
    out.push(PLAIN, &line[..indent]);
    let rest = &line[indent..];
    if rest.starts_with('#') {
        out.push(COMMENT, rest);
        return;
    }
    if rest.starts_with('[') {
        // `[limits]` and `[[queues]]`: the brackets are punctuation and the
        // name between them is what the reader is looking for.
        let open = run_end(rest, 0, |byte| byte == b'[');
        out.push(PUNCT, &rest[..open]);
        let named = &rest[open..];
        let name = named.find(']').unwrap_or(named.len());
        out.push(TYPE, &named[..name]);
        toml_value(&named[name..], out);
        return;
    }
    match rest.find('=') {
        Some(equals) => {
            let key = rest[..equals].trim_end();
            out.push(IDENT, key);
            out.push(PLAIN, &rest[key.len()..equals]);
            out.push(PUNCT, "=");
            toml_value(&rest[equals + 1..], out);
        }
        // A line of an array that spans several: `  "kv",`.
        None => toml_value(rest, out),
    }
}

/// What is to the right of a TOML `=`, or the rest of a header's line.
fn toml_value(text: &str, out: &mut Pieces) {
    scan(text, out, |text, at| {
        let bytes = text.as_bytes();
        let byte = bytes[at];
        if byte == b'#' {
            return Some((COMMENT, text.len()));
        }
        if byte == b'"' || byte == b'\'' {
            return Some((STRING, quoted_end(text, at)));
        }
        if byte.is_ascii_digit() {
            // A date and a time are written like numbers and read like them:
            // `1979-05-27T07:32:00Z` is one run rather than a sum.
            return Some((
                NUMBER,
                run_end(text, at, |byte| {
                    byte.is_ascii_alphanumeric() || b"+-.:_".contains(&byte)
                }),
            ));
        }
        if byte.is_ascii_alphabetic() {
            let end = run_end(text, at, name_rest);
            let word = &text[at..end];
            return Some((
                if word == "true" || word == "false" {
                    KEYWORD
                } else {
                    PLAIN
                },
                end,
            ));
        }
        if b"[]{},=".contains(&byte) {
            return Some((PUNCT, at + 1));
        }
        None
    });
}

/// A stylesheet: the comments, the strings, the lengths and colours, the
/// at-rules, and the names — a selector outside a block, a property inside
/// one.
///
/// Where the scanner is in the sheet is the whole of its state, and it is
/// two counters: a name is a selector until a `{` or a `(` has been opened,
/// and what follows a `:` inside one is the value rather than another name.
pub fn css(text: &str) -> Vec<Piece> {
    let mut out = Pieces::default();
    let mut blocks = 0u32;
    let mut parens = 0u32;
    let mut in_value = false;
    scan(text, &mut out, |text, at| {
        let bytes = text.as_bytes();
        let byte = bytes[at];
        if bytes[at..].starts_with(b"/*") {
            // CSS comments do not nest: `/* /* */` is closed.
            return Some((COMMENT, closed_end(text, at + 2, "*/")));
        }
        if byte == b'"' || byte == b'\'' {
            return Some((STRING, quoted_end(text, at)));
        }
        if byte == b'@' {
            return Some((KEYWORD, run_end(text, at + 1, name_rest)));
        }
        if byte == b'#' && bytes.get(at + 1).is_some_and(u8::is_ascii_hexdigit) {
            return Some((
                NUMBER,
                run_end(text, at + 1, |byte| byte.is_ascii_hexdigit()),
            ));
        }
        if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            // The unit is part of the number: `.58em`, `761px`, `999px`.
            return Some((
                NUMBER,
                run_end(text, at, |byte| {
                    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'%'
                }),
            ));
        }
        if byte == b'{' {
            blocks += 1;
            in_value = false;
            return Some((PUNCT, at + 1));
        }
        if byte == b'}' {
            blocks = blocks.saturating_sub(1);
            in_value = false;
            return Some((PUNCT, at + 1));
        }
        if byte == b'(' {
            parens += 1;
            return Some((PUNCT, at + 1));
        }
        if byte == b')' {
            parens = parens.saturating_sub(1);
            return Some((PUNCT, at + 1));
        }
        if byte == b':' {
            // `a:hover` is a selector and `color:red` is a declaration; the
            // difference is whether a block or a parenthesis is open.
            in_value = blocks > 0 || parens > 0;
            return Some((PUNCT, at + 1));
        }
        if byte == b';' {
            in_value = false;
            return Some((PUNCT, at + 1));
        }
        if name_start(byte) {
            let end = run_end(text, at, name_rest);
            let kind = if in_value {
                PLAIN
            } else if blocks > 0 || parens > 0 {
                IDENT
            } else {
                TYPE
            };
            return Some((kind, end));
        }
        if b",>+~*=[]!".contains(&byte) {
            return Some((PUNCT, at + 1));
        }
        None
    });
    out.out
}

/// XML, which is what the assets that are markup are: an SVG icon, and
/// whatever else a deploy carries.
///
/// A tag's first name is the element and the rest are its attributes, which
/// is the one thing the scanner has to remember.
pub fn xml(text: &str) -> Vec<Piece> {
    let mut out = Pieces::default();
    let mut in_tag = false;
    let mut want_element = false;
    scan(text, &mut out, |text, at| {
        let bytes = text.as_bytes();
        let byte = bytes[at];
        if !in_tag {
            if bytes[at..].starts_with(b"<!--") {
                return Some((COMMENT, closed_end(text, at + 4, "-->")));
            }
            // `</a`, and `<?xml` and `<!DOCTYPE`, whose names are read as
            // elements because that is what they are in that position.
            if bytes[at..].starts_with(b"</")
                || bytes[at..].starts_with(b"<?")
                || bytes[at..].starts_with(b"<!")
            {
                in_tag = true;
                want_element = true;
                return Some((PUNCT, at + 2));
            }
            if byte == b'<' {
                in_tag = true;
                want_element = true;
                return Some((PUNCT, at + 1));
            }
            // Text between tags, entities included, is what it says.
            return None;
        }
        if bytes[at..].starts_with(b"/>") || bytes[at..].starts_with(b"?>") {
            in_tag = false;
            return Some((PUNCT, at + 2));
        }
        if byte == b'>' {
            in_tag = false;
            return Some((PUNCT, at + 1));
        }
        if byte == b'"' || byte == b'\'' {
            return Some((STRING, quoted_end(text, at)));
        }
        if byte == b'=' {
            return Some((PUNCT, at + 1));
        }
        if name_start(byte) {
            // A name may hold `:` for a namespace and `.` in a DOCTYPE.
            let end = run_end(text, at, |byte| {
                name_rest(byte) || byte == b':' || byte == b'.'
            });
            if want_element {
                want_element = false;
                return Some((TYPE, end));
            }
            return Some((IDENT, end));
        }
        None
    });
    out.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces concatenated, which has to be the text that was scanned.
    fn whole(pieces: &[Piece]) -> String {
        pieces.iter().map(|piece| piece.text.as_str()).collect()
    }

    /// The kinds a text came out as, each with its text: what to assert a
    /// colour by.
    fn kinds(pieces: &[Piece]) -> Vec<(&'static str, &str)> {
        pieces
            .iter()
            .map(|piece| (piece.kind, piece.text.as_str()))
            .collect()
    }

    /// `text` as one Cove file, lexed as the host lexes one.
    fn as_cove(text: &str) -> Vec<Piece> {
        let mut sources = SourceMap::new();
        let file = sources.add("a/a.cove", text);
        cove(&sources, file)
    }

    #[test]
    fn a_cove_file_is_coloured_by_the_lexer_that_compiles_it() {
        let pieces = as_cove("export fn f(n: Int) -> Bool {\n  n > 2\n}\n");
        assert_eq!(
            whole(&pieces),
            "export fn f(n: Int) -> Bool {\n  n > 2\n}\n"
        );
        let coloured: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind != PLAIN)
            .collect();
        assert_eq!(
            coloured,
            [
                (KEYWORD, "export"),
                (KEYWORD, "fn"),
                (PUNCT, "("),
                (PUNCT, ":"),
                (TYPE, "Int"),
                (PUNCT, ")"),
                (PUNCT, "->"),
                (TYPE, "Bool"),
                (PUNCT, "{"),
                (PUNCT, ">"),
                (NUMBER, "2"),
                (PUNCT, "}"),
            ]
        );
    }

    /// The point of walking the gaps: an ordinary `//` is not a token at
    /// all, and a `///` is.
    #[test]
    fn both_kinds_of_comment_are_coloured_as_comments() {
        let text = "/// what it is\n// why\nlet a = 1 // after\n/* over\n   two */\n";
        let pieces = as_cove(text);
        assert_eq!(whole(&pieces), text);
        let said: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind == COMMENT || *kind == DOC)
            .collect();
        assert_eq!(
            said,
            [
                (DOC, "/// what it is"),
                (COMMENT, "// why"),
                (COMMENT, "// after"),
                (COMMENT, "/* over\n   two */"),
            ]
        );
    }

    /// A `"""` literal is one string over its lines, and so is a tagged
    /// one: `html"…"` is a single token with its tag, and colouring it as
    /// the punctuation every other token falls to would paint a page's
    /// markup the colour of a comma.
    #[test]
    fn a_tagged_and_a_triple_quoted_literal_are_each_one_string() {
        let text = "let a = html\"<p>{n}</p>\"\nlet b = \"\"\"\n  x\n  \"\"\"\n";
        let pieces = as_cove(text);
        assert_eq!(whole(&pieces), text);
        let strings: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind == STRING)
            .collect();
        assert_eq!(
            strings,
            [
                (STRING, "html\"<p>{n}</p>\""),
                (STRING, "\"\"\"\n  x\n  \"\"\""),
            ]
        );
    }

    /// A nested `/* /* */ */` is one comment to the lexer, so it is one
    /// here: a scanner that stopped at the first `*/` would colour the rest
    /// as code the compiler never read.
    #[test]
    fn a_nested_block_comment_is_one_comment() {
        let text = "let a = 1\n/* out /* in */ still out */\nlet b = 2\n";
        let pieces = as_cove(text);
        assert_eq!(whole(&pieces), text);
        assert!(
            kinds(&pieces).contains(&(COMMENT, "/* out /* in */ still out */")),
            "{:?}",
            kinds(&pieces)
        );
    }

    /// A file that does not lex is the file somebody is reading, so it is
    /// coloured by what the lexer recovered and loses nothing.
    #[test]
    fn a_file_that_does_not_lex_still_comes_out_whole() {
        for text in [
            "let a = \"never closed\nlet b = 2\n",
            "let a = £\nfn f() {}\n",
            "/* never closed\n",
            "\"",
            "",
        ] {
            assert_eq!(whole(&as_cove(text)), text, "{text:?}");
        }
    }

    /// A string literal is one token over the whole literal, interpolation
    /// and all, and a `{` in it is the file's.
    #[test]
    fn a_string_literal_is_one_piece_whatever_is_in_it() {
        let text = "let a = \"<img src=y> {name}\"\n";
        let pieces = as_cove(text);
        assert_eq!(whole(&pieces), text);
        assert!(
            kinds(&pieces).contains(&(STRING, "\"<img src=y> {name}\"")),
            "{:?}",
            kinds(&pieces)
        );
    }

    #[test]
    fn a_toml_file_is_its_tables_keys_and_values() {
        let text = "# what it may do\ngrant = [\"kv\", \"log\"]\n\n\
                    [limits]\ndeadline = \"2s\"\nmax_in_flight = 4\nquiet = true\n";
        let pieces = toml(text);
        assert_eq!(whole(&pieces), text);
        let coloured: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind != PLAIN)
            .collect();
        assert_eq!(
            coloured,
            [
                (COMMENT, "# what it may do"),
                (IDENT, "grant"),
                (PUNCT, "="),
                (PUNCT, "["),
                (STRING, "\"kv\""),
                (PUNCT, ","),
                (STRING, "\"log\""),
                (PUNCT, "]"),
                (PUNCT, "["),
                (TYPE, "limits"),
                (PUNCT, "]"),
                (IDENT, "deadline"),
                (PUNCT, "="),
                (STRING, "\"2s\""),
                (IDENT, "max_in_flight"),
                (PUNCT, "="),
                (NUMBER, "4"),
                (IDENT, "quiet"),
                (PUNCT, "="),
                (KEYWORD, "true"),
            ]
        );
    }

    #[test]
    fn a_stylesheet_is_its_selectors_properties_and_values() {
        let pieces = css("/* a */\n:root{--bg:#f4f6f8}\na:hover{color:var(--bg)}\n");
        assert_eq!(
            whole(&pieces),
            "/* a */\n:root{--bg:#f4f6f8}\na:hover{color:var(--bg)}\n"
        );
        let coloured: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind != PLAIN && *kind != PUNCT)
            .collect();
        assert_eq!(
            coloured,
            [
                (COMMENT, "/* a */"),
                (TYPE, "root"),
                (IDENT, "--bg"),
                (NUMBER, "#f4f6f8"),
                (TYPE, "a"),
                (TYPE, "hover"),
                (IDENT, "color"),
            ]
        );
    }

    #[test]
    fn an_svg_is_its_tags_and_their_attributes() {
        let text = "<!-- an icon -->\n<svg viewBox=\"0 0 16 16\"><circle r=\"8\"/></svg>\n";
        let pieces = xml(text);
        assert_eq!(whole(&pieces), text);
        let coloured: Vec<(&str, &str)> = kinds(&pieces)
            .into_iter()
            .filter(|(kind, _)| *kind != PLAIN && *kind != PUNCT)
            .collect();
        assert_eq!(
            coloured,
            [
                (COMMENT, "<!-- an icon -->"),
                (TYPE, "svg"),
                (IDENT, "viewBox"),
                (STRING, "\"0 0 16 16\""),
                (TYPE, "circle"),
                (IDENT, "r"),
                (STRING, "\"8\""),
                (TYPE, "svg"),
            ]
        );
    }

    /// Every scanner answers the text it was given, byte for byte, over the
    /// files this repository actually ships — which are longer and stranger
    /// than anything written into a test.
    #[test]
    fn the_pieces_of_a_real_file_are_the_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/admin");
        for name in [
            "app.toml",
            "pages.cove",
            "admin.cove",
            "text/text.cove",
            "assets/style.css",
            "assets/favicon.svg",
        ] {
            let text = std::fs::read_to_string(root.join(name)).expect(name);
            let pieces = match name.ends_with(".cove") {
                true => as_cove(&text),
                false => scanned(name, &text).expect(name),
            };
            assert_eq!(whole(&pieces), text, "{name}");
            // And it is colouring something, not answering one plain run.
            assert!(
                pieces.iter().any(|piece| piece.kind != PLAIN),
                "{name} came out with no colour at all"
            );
        }
    }

    /// Text holding markup is text: a piece carries the file's bytes and
    /// whoever shows it escapes them.
    #[test]
    fn a_piece_carries_the_bytes_and_escapes_nothing() {
        let text = "// <img src=y> & \"quoted\"\nlet a = \"<script>\"\n";
        let pieces = as_cove(text);
        assert_eq!(whole(&pieces), text);
        assert!(
            kinds(&pieces).contains(&(COMMENT, "// <img src=y> & \"quoted\"")),
            "{:?}",
            kinds(&pieces)
        );
    }

    /// A file no scanner here claims is answered by nothing rather than by
    /// a guess.
    #[test]
    fn a_name_no_scanner_claims_is_not_coloured() {
        for name in [
            "assets/logo.png",
            "README",
            "assets/app.js",
            "assets/data.JSON",
            "a.cove",
        ] {
            assert!(scanned(name, "x = 1\n").is_none(), "{name}");
        }
        for name in ["app.toml", "a/b.css", "assets/icon.svg", "feed.xml"] {
            assert!(scanned(name, "x = 1\n").is_some(), "{name}");
        }
    }

    /// Runs of one kind are joined, which is what keeps a file's pieces near
    /// its line count: a `<span>` a byte would be most of the page.
    #[test]
    fn runs_of_one_kind_are_one_piece() {
        let pieces = as_cove("let a = f(((1)))\n");
        let punct: Vec<&str> = pieces
            .iter()
            .filter(|piece| piece.kind == PUNCT)
            .map(|piece| piece.text.as_str())
            .collect();
        assert_eq!(punct, ["=", "(((", ")))"], "{:?}", kinds(&pieces));
    }
}
