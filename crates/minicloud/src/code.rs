//! The code an app is running, read back: its `app.toml`, its `.cove` files
//! and its `assets/` — what a deploy carries ([`crate::deploy`]), which is
//! what "the code that is deployed" means (issue 89).
//!
//! **Nothing here is read for this.** Every part was already kept, for its
//! own reason, and this module collects them:
//!
//! - the `.cove` text is in the [`SourceMap`](cove_diag::SourceMap) the app
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

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use crate::apps::{App, AppState};

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

/// One file of an app's deploy, as the admin app sees it.
pub struct CodeFile {
    /// Its name within the app: `app.toml`, `admin.cove`, `text/text.cove`,
    /// `assets/style.css`.
    pub name: String,
    /// `config`, `cove` or `asset`.
    pub kind: &'static str,
    /// The whole file's length in bytes.
    pub bytes: u64,
    /// Its text, cut past [`READ_TEXT_BYTES`]; empty in a listing, which
    /// carries none, and empty for a file that is not UTF-8.
    pub text: String,
    /// Whether the file is UTF-8, so that `text` is the file. An asset need
    /// not be: a stylesheet and an SVG are text, a PNG and a font are not,
    /// and the one that is not is to be looked at where the host serves it.
    pub is_text: bool,
    /// Whether `text` is shorter than the whole.
    pub cut: bool,
    /// How the file of the same name in the app's directory compares with
    /// this one: `same`, `differs`, `missing`, or `unread` for one that
    /// cannot be read. `app.toml` is read from the directory, so it is
    /// always `same`.
    pub on_disk: &'static str,
}

/// What a file of the deploy is, and where the host already holds it.
enum Where<'a> {
    /// `app.toml`, read from the app's directory because the load keeps
    /// only what it parsed to.
    Config,
    /// A `.cove` file, whose text the compile's source map holds.
    Cove(&'a str),
    /// A file of `assets/`, whose bytes the app's [`Assets`](crate::assets)
    /// holds.
    Asset(&'a [u8]),
}

impl Where<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Where::Config => "config",
            Where::Cove(_) => "cove",
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
            out.push((name.to_string(), Where::Cove(&file.text)));
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
        Where::Cove(text) => Ok(Cow::Borrowed(text.as_bytes())),
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
    Ok(CodeFile {
        name: name.to_string(),
        kind: at.kind(),
        bytes: content.len() as u64,
        text,
        is_text: whole.is_ok(),
        cut,
        on_disk: on_disk(app, name, at, &content),
    })
}

/// One file of a listing: a file that cannot be read is still in it, saying
/// so, because leaving `app.toml` out of the list of what is deployed would
/// be the one thing nobody would notice.
fn listed(app: &App, name: &str, at: &Where<'_>) -> CodeFile {
    entry(app, name, at, None).unwrap_or_else(|_| CodeFile {
        name: name.to_string(),
        kind: at.kind(),
        bytes: 0,
        text: String::new(),
        is_text: false,
        cut: false,
        on_disk: "unread",
    })
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
/// carrying no text: a listing says what is there and how large, and one
/// file's read says what is in it.
pub fn listing(app: &App, limit: i64) -> Vec<CodeFile> {
    let limit = limit.clamp(0, CODE_FILES) as usize;
    deploy(app)
        .iter()
        .take(limit)
        .map(|(name, at)| listed(app, name, at))
        .collect()
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
