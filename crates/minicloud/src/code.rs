//! The code an app is running, read back: its `app.toml`, its `.cove` files
//! and its `assets/` — what a deploy carries ([`crate::deploy`]), which is
//! what "the code that is deployed" means (issue 89).
//!
//! **Nothing here is read for this.** Every part was already kept, for its
//! own reason, and this module collects them:
//!
//! - the `.cove` text is in the [`SourceMap`] the app
//!   was compiled with, kept because a runtime error is rendered against it
//!   ([`crate::apps::Ready::sources`]);
//! - the asset bytes are in [`Assets`](crate::assets), read at load because
//!   a request for one must cost no disk access;
//! - `app.toml` is kept as nothing but its [`Settings`](crate::config): it
//!   is parsed at load and the text does not survive, so that one is a read
//!   of `App.dir`.
//!
//! What is in memory is what is **running**, and that is the honest default:
//! it cannot disagree with the version the page shows. The directory can — a
//! deploy staged and not switched to, an edit made on the server — and the
//! disagreement is sometimes the thing worth seeing, so every file says how
//! the one beside it on disk compares ([`CodeFile::on_disk`]). `app.toml`
//! comes from disk either way, which is why the question is a real one here
//! and not a theoretical one.
//!
//! # A name is never a path
//!
//! A listing is built from what the host holds; one file's read finds its
//! entry in that listing by name. So a name a request made up — `../`, an
//! absolute path, another app's file — matches nothing instead of reaching
//! for it, and the only path this module joins from a name is the one it
//! stats to compare with disk, and that one is refused unless every
//! component of the name is an ordinary one.
//!
//! # The shape, and the colours, are the host's (issue 100)
//!
//! A listing is a **tree**: the names carry the nesting already, and
//! `tree` puts a row where a directory begins so that each row needs only
//! its [`CodeFile::depth`] to be shown. Grouping paths is a sort and a walk,
//! which Cove can do and awkwardly; the host sorted them in the first place.
//! The page indents by the depth it is told and decides nothing.
//!
//! A file's text comes with the **pieces** it is coloured by
//! ([`crate::highlight`]), which are a kind and a run of text and never
//! markup: the admin app escapes every value it shows, and the file it shows
//! is the value most likely to be markup itself. Both of the limits on that
//! are said rather than quietly applied — a file that was cut and a file
//! past [`HIGHLIGHT_PIECES`] are shown plain, and [`CodeFile::plain`] is why.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use cove_diag::{FileId, SourceMap};

use crate::apps::{App, AppState};
use crate::highlight::{self, Piece};

/// The config file of an app, which is the first thing to look at: it is
/// what decides what the app may do.
const CONFIG: &str = "app.toml";

/// The most files one listing carries. An app's deploy is bounded already —
/// its `.cove` files and an `assets/` under `[assets] max_bytes` — so this
/// is not paging but a ceiling, and a listing that reaches it says so rather
/// than ending quietly.
pub const CODE_FILES: i64 = 500;

/// The longest text one file's read carries, cut at a character boundary;
/// [`CodeFile::cut`] says when it was reached. A page shows one file, and a
/// file an editor is happy with is still more than a browser wants in one
/// `<pre>`.
pub const READ_TEXT_BYTES: usize = 128 * 1024;

/// The most pieces one file's colouring carries; past it the file is shown
/// plain and [`CodeFile::plain`] says so.
///
/// The limit is on *pieces* rather than on bytes because a piece is what the
/// page carries: a `<span>` with a class, for every one of them. Bytes are a
/// poor guess at how many there will be — [`crate::highlight`] joins each
/// run of one kind, so this
/// repository's longest file is 58 KiB and 6,500 pieces, while the same
/// length of nothing but alternating operators and names would be twenty
/// times that. So the ceiling is high enough that every file the window
/// shows whole is coloured and low enough that a pathological one is not.
pub const HIGHLIGHT_PIECES: usize = 20_000;

/// One row of an app's deploy, as the admin app sees it: a file, or a
/// directory that holds the rows after it.
///
/// A directory is a row of the same type rather than one of its own, and
/// `kind` says which it is rather than a `bool` beside `kind` saying the
/// same thing twice. The page already asks what a row is — it has to, to
/// label a configuration file differently from an asset — so a fourth
/// answer to the question it asks costs it one branch and no new field.
pub struct CodeFile {
    /// Its name within the app: `app.toml`, `admin.cove`, `text/text.cove`,
    /// `assets/style.css`. A directory's is its path without a trailing
    /// slash: `text`, `assets`.
    pub name: String,
    /// The last part of `name`, which is what a row at its depth shows:
    /// `text.cove` for `text/text.cove`. Split here rather than in the page
    /// because the host made the name.
    pub label: String,
    /// How many directories deep `name` is: 0 for `app.toml`, 1 for
    /// `text/text.cove` and for the `assets` directory's own files, and for
    /// a directory row, the depth of the row it holds minus one.
    ///
    /// This is the whole of what the page needs to draw the tree: it indents
    /// by what it is told, and the order and the nesting were decided here
    /// (`tree`).
    pub depth: i64,
    /// `config`, `cove`, `asset`, or `dir` for a directory.
    pub kind: &'static str,
    /// The whole file's length in bytes; for a directory, what the files
    /// under it come to, so that "how large is `assets/`" is a question the
    /// page answers without a reader adding up a column.
    pub bytes: u64,
    /// Its text, cut past [`READ_TEXT_BYTES`]; empty in a listing, which
    /// carries none, and empty for a file that is not UTF-8.
    pub text: String,
    /// What `text` is coloured by ([`crate::highlight`]): a kind and a run
    /// of text each, which concatenated are `text` itself. Empty in a
    /// listing, and empty wherever `plain` says why there are none.
    pub pieces: Vec<Piece>,
    /// Why `text` is shown plain, and empty where `pieces` is the text:
    /// `cut` (the text stops short of the file, and colouring it would say
    /// the rest of the file is whatever the cut fell inside), `dense` (more
    /// than [`HIGHLIGHT_PIECES`] of them), or `unknown` (no reader for this
    /// kind of file — a guess from the name would be a claim about the
    /// file). Empty for a listing and for a file that is not UTF-8 as well,
    /// there being no text in either to colour or to say anything about.
    pub plain: &'static str,
    /// Whether the file is UTF-8, so that `text` is the file. An asset need
    /// not be: a stylesheet and an SVG are text, a PNG and a font are not,
    /// and the one that is not is to be looked at where the host serves it.
    pub is_text: bool,
    /// Whether `text` is shorter than the whole.
    pub cut: bool,
    /// How the file of the same name in the app's directory compares with
    /// this one: `same`, `differs`, `missing`, or `unread` for one that
    /// cannot be read. `app.toml` is read from the directory, so it is
    /// always `same`; a directory's is empty, there being no file of it to
    /// compare.
    pub on_disk: &'static str,
}

/// What a file of the deploy is, and where the host already holds it.
enum Where<'a> {
    /// `app.toml`, read from the app's directory because the load keeps
    /// only what it parsed to.
    Config,
    /// A `.cove` file, whose text the compile's source map holds — and
    /// which is coloured by the lexer that compiled it, so the map and the
    /// file's id in it travel with the text ([`crate::highlight::cove`]).
    /// A file that is not in a source map — `app.toml`, an asset — cannot
    /// reach that path, which is why the two below carry bytes alone.
    Cove {
        text: &'a str,
        sources: &'a SourceMap,
        id: FileId,
    },
    /// A file of `assets/`, whose bytes the app's [`Assets`](crate::assets)
    /// holds.
    Asset(&'a [u8]),
}

impl Where<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Where::Config => "config",
            Where::Cove { .. } => "cove",
            Where::Asset(_) => "asset",
        }
    }
}

/// Every file of `app`'s deploy, in the order there is a reason to read
/// them: `app.toml` first — it decides what the app may do and is the usual
/// reason to look — then the `.cove` files by path, then the assets by path.
///
/// A refused app has no source map, so it contributes its `app.toml` and its
/// assets and no code: the reason it was refused is already on its page.
fn deploy(app: &App) -> Vec<(String, Where<'_>)> {
    let mut out = vec![(CONFIG.to_string(), Where::Config)];
    if let AppState::Ready(ready) = &app.state {
        for file in ready.sources.files() {
            // The standard library is in the same map, attached by
            // `cove_sema::stdlib`; it is the toolchain's code, not the
            // deploy's.
            if ready.sources.is_library(file.id) {
                continue;
            }
            // Named relative to the apps directory, so that a diagnostic
            // reads `admin/admin.cove:3` ([`crate::apps::load_package`]):
            // the app's own name comes off the front.
            let Ok(within) = file.path.strip_prefix(&app.name) else {
                continue;
            };
            let Some(name) = within.to_str() else {
                continue;
            };
            out.push((
                name.to_string(),
                Where::Cove {
                    text: &file.text,
                    sources: ready.sources.as_ref(),
                    id: file.id,
                },
            ));
        }
    }
    for (path, asset) in app.assets.iter() {
        // `path` is the request path the host answers it on (`/style.css`);
        // the deploy's name for it is `assets/style.css`.
        out.push((
            format!("{}{path}", crate::assets::DIR),
            Where::Asset(asset.bytes.as_ref()),
        ));
    }
    out
}

/// The whole content of one file: borrowed where the host holds it, read
/// from the app's directory for `app.toml`.
fn content<'a>(app: &App, at: &Where<'a>) -> Result<Cow<'a, [u8]>, String> {
    match at {
        Where::Config => std::fs::read(app.dir.join(CONFIG))
            .map(Cow::Owned)
            .map_err(|e| format!("cannot read `{}`'s {CONFIG}: {e}", app.name)),
        Where::Cove { text, .. } => Ok(Cow::Borrowed(text.as_bytes())),
        Where::Asset(bytes) => Ok(Cow::Borrowed(bytes)),
    }
}

/// One file as the admin app sees it. `carry` is how much of its text to
/// take: `None` for a listing, which carries none.
fn entry(app: &App, name: &str, at: &Where<'_>, carry: Option<usize>) -> Result<CodeFile, String> {
    let content = content(app, at)?;
    let whole = std::str::from_utf8(&content);
    let (text, cut) = match (carry, whole) {
        (Some(limit), Ok(whole)) => {
            let shown = truncate(whole, limit);
            (shown.to_string(), shown.len() < whole.len())
        }
        _ => (String::new(), false),
    };
    // A listing carries no text, so there is nothing in one to colour.
    let (pieces, plain) = match carry {
        Some(_) if whole.is_ok() => coloured(name, &text, cut, at),
        _ => (Vec::new(), ""),
    };
    let (label, depth) = split_name(name);
    Ok(CodeFile {
        name: name.to_string(),
        label,
        depth,
        kind: at.kind(),
        bytes: content.len() as u64,
        text,
        pieces,
        plain,
        is_text: whole.is_ok(),
        cut,
        on_disk: on_disk(app, name, at, &content),
    })
}

/// One file of a listing: a file that cannot be read is still in it, saying
/// so, because leaving `app.toml` out of the list of what is deployed would
/// be the one thing nobody would notice.
fn listed(app: &App, name: &str, at: &Where<'_>) -> CodeFile {
    entry(app, name, at, None).unwrap_or_else(|_| {
        let (label, depth) = split_name(name);
        CodeFile {
            name: name.to_string(),
            label,
            depth,
            kind: at.kind(),
            bytes: 0,
            text: String::new(),
            pieces: Vec::new(),
            plain: "",
            is_text: false,
            cut: false,
            on_disk: "unread",
        }
    })
}

/// A name's last part and how many directories deep it is:
/// `("text.cove", 1)` for `text/text.cove`, `("app.toml", 0)` for
/// `app.toml`.
fn split_name(name: &str) -> (String, i64) {
    match name.rsplit_once('/') {
        Some((dirs, label)) => (label.to_string(), dirs.split('/').count() as i64),
        None => (name.to_string(), 0),
    }
}

/// How one file's text is coloured ([`crate::highlight`]): the pieces, and
/// the word for why there are none.
///
/// A text that was cut is not coloured at all, whatever it is. The pieces of
/// half a file would say the rest of the file is whatever the cut fell
/// inside — an unclosed string literal colours everything after it — and
/// that is a page claiming something about a part of the file it is not
/// showing.
fn coloured(name: &str, text: &str, cut: bool, at: &Where<'_>) -> (Vec<Piece>, &'static str) {
    if cut {
        return (Vec::new(), "cut");
    }
    let found = match at {
        // The lexer the host compiled this file with, over the source map it
        // compiled it from: a colour here cannot disagree with what runs.
        Where::Cove { sources, id, .. } => Some(highlight::cove(sources, *id)),
        Where::Config | Where::Asset(_) => highlight::scanned(name, text),
    };
    let Some(pieces) = found else {
        return (Vec::new(), "unknown");
    };
    if pieces.len() > HIGHLIGHT_PIECES {
        return (Vec::new(), "dense");
    }
    (pieces, "")
}

/// At most the first `limit` bytes of `text`, cut at a character boundary.
fn truncate(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// How the file at `name` in the app's directory compares with `held`, what
/// the host is running.
///
/// The whole file is read and compared rather than its size or its time:
/// a hand edit that keeps the length is exactly the case this is for. It
/// costs one read per file of a page a person asked for, which is the right
/// trade for an admin page over a deploy bounded by `[assets] max_bytes`.
fn on_disk(app: &App, name: &str, at: &Where<'_>, held: &[u8]) -> &'static str {
    if matches!(at, Where::Config) {
        // Read from the directory, so there is nothing it can disagree with.
        return "same";
    }
    let Some(path) = within(&app.dir, name) else {
        return "unread";
    };
    match std::fs::read(&path) {
        Ok(bytes) if bytes == held => "same",
        Ok(_) => "differs",
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unread",
    }
}

/// `dir` joined with `name`, or `None` for a name that would not stay under
/// it. Every name here is the host's own, so none should ever be refused;
/// the check is where a name that came from a request would otherwise
/// escape, and costs nothing.
fn within(dir: &Path, name: &str) -> Option<PathBuf> {
    let name = Path::new(name);
    if name.components().count() == 0
        || !name
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(dir.join(name))
}

/// Every file of `app`'s deploy, at most `limit` of them (0–[`CODE_FILES`]),
/// as a tree and carrying no text: a listing says what is there and how
/// large, and one file's read says what is in it.
///
/// `limit` counts *files*, so that the ceiling means the same thing it did
/// before there were directory rows: the directories a page shows are the
/// ones holding the files it shows, and a page asking for 200 can still tell
/// from the file count whether there are more.
pub fn listing(app: &App, limit: i64) -> Vec<CodeFile> {
    let limit = limit.clamp(0, CODE_FILES) as usize;
    tree(
        deploy(app)
            .iter()
            .take(limit)
            .map(|(name, at)| listed(app, name, at))
            .collect(),
    )
}

/// `files` in the order [`deploy`] put them, with a row inserted wherever a
/// directory begins.
///
/// One pass is enough, and that is the reason the order is the one above.
/// Every file of a directory is next to the others in it — the source map
/// holds an app's own files before each of its module directories, and the
/// assets are a sorted walk — so "has the directory changed since the last
/// file" is the whole test, and a directory is opened once and never
/// reopened. A listing sorted some other way would need a real grouping
/// pass, and the order it has is the one there is a reason to read in:
/// `app.toml` first because it decides what the app may do, then the code,
/// then the assets.
fn tree(files: Vec<CodeFile>) -> Vec<CodeFile> {
    let mut out: Vec<CodeFile> = Vec::new();
    // The directories the last row was in, outermost first.
    let mut open: Vec<String> = Vec::new();
    for file in files {
        let parts: Vec<&str> = file.name.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        let shared = open
            .iter()
            .zip(dirs)
            .take_while(|(was, now)| was.as_str() == **now)
            .count();
        open.truncate(shared);
        for part in &dirs[shared..] {
            open.push((*part).to_string());
            out.push(directory(&open));
        }
        out.push(file);
    }
    sizes(&mut out);
    out
}

/// The row for the directory `open` names, the outermost component first.
fn directory(open: &[String]) -> CodeFile {
    CodeFile {
        name: open.join("/"),
        label: open.last().cloned().unwrap_or_default(),
        depth: open.len() as i64 - 1,
        kind: "dir",
        // Filled in by `sizes`, which needs the rows under it.
        bytes: 0,
        text: String::new(),
        pieces: Vec::new(),
        plain: "",
        is_text: false,
        cut: false,
        on_disk: "",
    }
}

/// Each directory row's `bytes`: what the files under it come to.
///
/// The rows under a directory are the ones after it and deeper than it,
/// which holds because [`tree`] opened it for the file that follows it and
/// closes it the moment one does not.
fn sizes(rows: &mut [CodeFile]) {
    for at in 0..rows.len() {
        if rows[at].kind != "dir" {
            continue;
        }
        let depth = rows[at].depth;
        rows[at].bytes = rows[at + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .filter(|row| row.kind != "dir")
            .map(|row| row.bytes)
            .sum();
    }
}

/// One file of `app`'s deploy, by the name a listing gave it, its text cut
/// past [`READ_TEXT_BYTES`].
///
/// `Err` for a name the deploy has no file under — which is what a name
/// holding `..`, an absolute path or another app's file gets, since the
/// listing's names are the only names there are.
pub fn one(app: &App, name: &str) -> Result<CodeFile, String> {
    let deploy = deploy(app);
    let Some((found, at)) = deploy.iter().find(|(held, _)| held == name) else {
        return Err(format!("`{}` has no file `{name}` deployed", app.name));
    };
    entry(app, found, at, Some(READ_TEXT_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_would_leave_the_apps_directory_is_refused() {
        let dir = Path::new("/apps/notes");
        assert_eq!(
            within(dir, "text/text.cove"),
            Some(PathBuf::from("/apps/notes/text/text.cove"))
        );
        for name in [
            "../hello/hello.cove",
            "..",
            "a/../../b",
            "/etc/passwd",
            "./a.cove",
            "",
        ] {
            assert!(within(dir, name).is_none(), "{name}");
        }
    }

    /// A row of a listing, as far as the tree is concerned.
    fn file(name: &str, bytes: u64) -> CodeFile {
        let (label, depth) = split_name(name);
        CodeFile {
            name: name.to_string(),
            label,
            depth,
            kind: "cove",
            bytes,
            text: String::new(),
            pieces: Vec::new(),
            plain: "",
            is_text: true,
            cut: false,
            on_disk: "same",
        }
    }

    /// Each row as `depth kind name`, which is the whole of what the page
    /// is given to draw the tree with.
    fn rows(rows: &[CodeFile]) -> Vec<String> {
        rows.iter()
            .map(|row| format!("{} {} {}", row.depth, row.kind, row.name))
            .collect()
    }

    /// A directory begins a row of its own, and the order the files were in
    /// survives: `app.toml` first, then the code, then the assets.
    #[test]
    fn a_directory_is_a_row_where_the_names_begin_to_nest() {
        let listed = tree(vec![
            file("app.toml", 1),
            file("admin.cove", 2),
            file("pages.cove", 4),
            file("text/text.cove", 8),
            file("assets/style.css", 16),
            file("assets/img/logo.png", 32),
            file("assets/img/deep/one.svg", 64),
        ]);
        assert_eq!(
            rows(&listed),
            [
                "0 cove app.toml",
                "0 cove admin.cove",
                "0 cove pages.cove",
                "0 dir text",
                "1 cove text/text.cove",
                "0 dir assets",
                "1 cove assets/style.css",
                "1 dir assets/img",
                "2 cove assets/img/logo.png",
                "2 dir assets/img/deep",
                "3 cove assets/img/deep/one.svg",
            ]
        );
        // A directory's size is what is under it, however deep.
        let sized: Vec<(&str, u64)> = listed
            .iter()
            .filter(|row| row.kind == "dir")
            .map(|row| (row.name.as_str(), row.bytes))
            .collect();
        assert_eq!(
            sized,
            [
                ("text", 8),
                ("assets", 16 + 32 + 64),
                ("assets/img", 32 + 64),
                ("assets/img/deep", 64),
            ]
        );
        // The label is what a row at its depth shows.
        let labels: Vec<&str> = listed.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "app.toml",
                "admin.cove",
                "pages.cove",
                "text",
                "text.cove",
                "assets",
                "style.css",
                "img",
                "logo.png",
                "deep",
                "one.svg",
            ]
        );
    }

    /// A directory is opened once: two of its files do not open it twice,
    /// and leaving it and coming back to a sibling does not reopen the one
    /// that was left.
    #[test]
    fn a_directory_is_opened_once_for_the_files_in_it() {
        let listed = tree(vec![
            file("a/one.cove", 1),
            file("a/two.cove", 1),
            file("b/one.cove", 1),
            file("c.cove", 1),
        ]);
        assert_eq!(
            rows(&listed),
            [
                "0 dir a",
                "1 cove a/one.cove",
                "1 cove a/two.cove",
                "0 dir b",
                "1 cove b/one.cove",
                "0 cove c.cove",
            ]
        );
        assert!(tree(Vec::new()).is_empty());
    }

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 2), "he");
        // Three bytes each: a limit inside one takes none of it.
        assert_eq!(truncate("あい", 4), "あ");
        assert_eq!(truncate("あい", 2), "");
        assert_eq!(truncate("あい", 0), "");
    }
}
