//! An app's static files: the host answers them itself.
//!
//! An app's response is a UTF-8 `String`, so it cannot return an image or a
//! font. Those live in the app's `assets/` directory and the host serves them
//! directly, never entering the app.
//!
//! | on disk | request path |
//! | --- | --- |
//! | `assets/style.css` | `/style.css` |
//! | `assets/img/logo.png` | `/img/logo.png` |
//!
//! # Read once, at load
//!
//! [`Assets::load`] reads every file into memory when the app is loaded. A
//! request then costs a map lookup and no disk access, and a file that
//! changes on disk changes nothing until the app is deployed again, so the
//! bytes an `ETag` names are the bytes that are served.
//!
//! # What is refused
//!
//! The error is the reason the app is refused, so it names the file.
//!
//! - A symbolic link, to a file or a directory. The host answers an app's own
//!   files only; a link could name any file the host can read.
//! - A name that is not UTF-8, or that has a `\` or a control character in
//!   it. No request path could name it faithfully.
//! - A total past `[assets] max_bytes`. Everything is held in memory, so the
//!   total is bounded.
//!
//! A name that starts with `.` is skipped, not refused: it is an editor's or
//! a file system's litter (`.DS_Store`, macOS's `._logo.png`), not something
//! the app meant to publish.

use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;

/// The directory in an app that holds its static files. The name is fixed
/// rather than configured: `minicloud deploy` has to know what travels with
/// an app ([`crate::deploy`]) and the version hash what belongs to it
/// ([`crate::apps`]) before any `app.toml` of it has been read.
pub const DIR: &str = "assets";

/// One file the host answers for an app.
pub struct Asset {
    pub bytes: Bytes,
    /// From the file's extension; `application/octet-stream` for one not in the table.
    pub content_type: &'static str,
    /// A strong entity tag, quoted: `"a1b2c3d4e5f6a7b8"`.
    pub etag: String,
}

impl Asset {
    /// Whether an `If-None-Match` names this version of the file: a `304`.
    /// Handles a list (`"a", "b"`), `*`, and the weak prefix `W/`.
    ///
    /// The weak prefix is ignored because `If-None-Match` uses weak
    /// comparison: two tags match when their opaque parts do.
    pub fn matches(&self, if_none_match: &str) -> bool {
        if_none_match.split(',').map(str::trim).any(|tag| {
            let tag = tag.strip_prefix("W/").unwrap_or(tag);
            tag == "*" || tag == self.etag
        })
    }
}

/// An app's `assets/` directory, read once at load.
#[derive(Default)]
pub struct Assets {
    /// Keyed by request path, such as `/style.css`.
    files: BTreeMap<String, Asset>,
}

impl Assets {
    /// Reads `<dir>/assets`, refusing the app (`Err`, the reason) for a
    /// symbolic link, an unusable name, or a total past `max_bytes`.
    ///
    /// No `assets/` directory is not an error: most apps have none.
    pub fn load(dir: &Path, max_bytes: u64) -> Result<Assets, String> {
        let root = dir.join(DIR);
        if !root.is_dir() {
            return Ok(Assets::default());
        }
        let mut assets = Assets::default();
        let mut total = 0;
        walk(&root, "", max_bytes, &mut total, &mut assets.files)?;
        Ok(assets)
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// How many files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// The files' sizes, summed.
    pub fn bytes(&self) -> u64 {
        self.files.values().map(|a| a.bytes.len() as u64).sum()
    }

    /// The file at a request path such as `/style.css`. Only an exact match
    /// answers: no `index.html` for a directory and no added extension, so
    /// what the host serves is what the app wrote and nothing is guessed.
    pub fn get(&self, path: &str) -> Option<&Asset> {
        self.files.get(path)
    }

    /// Every file, by request path, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Asset)> {
        self.files
            .iter()
            .map(|(path, asset)| (path.as_str(), asset))
    }
}

/// Reads `dir` and everything under it. `rel` is `dir`'s request path
/// without the leading `/` (`""` for the root). Entries go by name, so the
/// file that crosses the limit is the same on every load.
fn walk(
    dir: &Path,
    rel: &str,
    max_bytes: u64,
    total: &mut u64,
    files: &mut BTreeMap<String, Asset>,
) -> Result<(), String> {
    let shown = |name: &str| format!("assets/{rel}{name}");
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read `{}`: {e}", shown("").trim_end_matches('/')))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read `{}`: {e}", shown("")))?;
        let name = entry.file_name().into_string().map_err(|raw| {
            format!(
                "`{}` has a name that is not UTF-8: {raw:?}",
                shown("").trim_end_matches('/')
            )
        })?;
        names.push(name);
    }
    names.sort();

    for name in names {
        if name.starts_with('.') {
            continue;
        }
        if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
            return Err(format!(
                "`{}` has a character in its name that a request path cannot carry",
                shown(&name)
            ));
        }
        let path = dir.join(&name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot read `{}`: {e}", shown(&name)))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "`{}` is a symbolic link; the host answers an app's own files only",
                shown(&name)
            ));
        }
        if meta.is_dir() {
            walk(&path, &format!("{rel}{name}/"), max_bytes, total, files)?;
        } else if meta.is_file() {
            let size = meta.len();
            if total.saturating_add(size) > max_bytes {
                return Err(format!(
                    "`{}` puts this app's assets over `[assets] max_bytes` ({max_bytes}): {} bytes in {} file(s)",
                    shown(&name),
                    total.saturating_add(size),
                    files.len() + 1
                ));
            }
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read `{}`: {e}", shown(&name)))?;
            *total += bytes.len() as u64;
            let asset = Asset {
                content_type: content_type(&name),
                etag: etag(&bytes),
                bytes: Bytes::from(bytes),
            };
            files.insert(format!("/{rel}{name}"), asset);
        }
    }
    Ok(())
}

/// The content type for a file name or path, from its extension (compared
/// in lower case). Text types carry `charset=utf-8` because a browser would
/// otherwise guess the encoding of a file that says nothing about it.
pub fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return "application/octet-stream";
    };
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/vnd.microsoft.icon",
        "pdf" => "application/pdf",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// A strong, quoted entity tag: FNV-1a (64 bit) over the content, the same
/// hash this repository already makes its app version ids with. It is not
/// cryptographic, and need not be: it tells a browser whether a file it
/// holds is the one the host has, not who wrote it.
fn etag(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("\"{hash:016x}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("minicloud-assets-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn asset_with(content: &[u8]) -> Asset {
        Asset {
            bytes: Bytes::copy_from_slice(content),
            content_type: "text/plain; charset=utf-8",
            etag: etag(content),
        }
    }

    #[test]
    fn an_app_without_an_assets_directory_has_no_assets() {
        let dir = scratch("none");
        let assets = Assets::load(&dir, 1024).unwrap();
        assert!(assets.is_empty());
        assert_eq!(assets.len(), 0);
        assert_eq!(assets.bytes(), 0);
    }

    #[test]
    fn a_nested_file_is_reached_by_its_request_path() {
        let dir = scratch("nested");
        let png = [0x89, b'P', b'N', b'G', 0x00, 0xff];
        std::fs::create_dir_all(dir.join("assets/img")).unwrap();
        std::fs::write(dir.join("assets/img/logo.png"), png).unwrap();
        std::fs::write(dir.join("assets/style.css"), "body {}").unwrap();

        let assets = Assets::load(&dir, 1024).unwrap();
        assert_eq!(assets.len(), 2);
        assert_eq!(assets.bytes(), png.len() as u64 + 7);
        let logo = assets.get("/img/logo.png").unwrap();
        assert_eq!(logo.bytes.as_ref(), &png[..]);
        assert_eq!(logo.content_type, "image/png");
        assert_eq!(
            assets.get("/style.css").unwrap().content_type,
            "text/css; charset=utf-8"
        );
        let paths: Vec<&str> = assets.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["/img/logo.png", "/style.css"]);
        assert!(assets.get("/img").is_none());
        assert!(assets.get("img/logo.png").is_none());
    }

    #[test]
    fn an_empty_file_is_an_asset_like_any_other() {
        let dir = scratch("empty");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/empty.txt"), "").unwrap();
        let assets = Assets::load(&dir, 0).unwrap();
        assert_eq!(assets.get("/empty.txt").unwrap().bytes.len(), 0);
    }

    #[test]
    fn files_and_directories_that_start_with_a_dot_are_left_out() {
        let dir = scratch("dots");
        std::fs::create_dir_all(dir.join("assets/.git")).unwrap();
        std::fs::write(dir.join("assets/.git/config"), "x").unwrap();
        std::fs::write(dir.join("assets/.DS_Store"), "x").unwrap();
        std::fs::write(dir.join("assets/._logo.png"), "x").unwrap();
        std::fs::write(dir.join("assets/logo.png"), "x").unwrap();

        let assets = Assets::load(&dir, 1024).unwrap();
        let paths: Vec<&str> = assets.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["/logo.png"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_refuses_the_app_and_the_message_names_it() {
        let dir = scratch("symlink");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("secret.txt"), dir.join("assets/passwd.txt")).unwrap();
        let err = Assets::load(&dir, 1024).err().unwrap();
        assert!(err.contains("assets/passwd.txt"), "{err}");
        assert!(err.contains("symbolic link"), "{err}");

        let dir = scratch("symlink-dir");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("assets/linked")).unwrap();
        let err = Assets::load(&dir, 1024).err().unwrap();
        assert!(err.contains("assets/linked"), "{err}");
    }

    #[test]
    fn a_total_past_max_bytes_refuses_the_app_naming_the_file_that_crossed_it() {
        let dir = scratch("over");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/a.txt"), vec![b'a'; 60]).unwrap();
        std::fs::write(dir.join("assets/big.png"), vec![0u8; 60]).unwrap();
        std::fs::write(dir.join("assets/c.txt"), "c").unwrap();

        let err = Assets::load(&dir, 100).err().unwrap();
        assert!(err.contains("`assets/big.png`"), "{err}");
        assert!(err.contains("(100)"), "{err}");
        assert!(err.contains("120 bytes in 2 file(s)"), "{err}");
        assert!(Assets::load(&dir, 121).is_ok());
    }

    #[test]
    fn the_same_content_has_the_same_etag_and_one_byte_more_changes_it() {
        let a = asset_with(b"hello");
        let b = asset_with(b"hello");
        let c = asset_with(b"hellp");
        assert_eq!(a.etag, b.etag);
        assert_ne!(a.etag, c.etag);
        assert!(a.etag.starts_with('"') && a.etag.ends_with('"'));
        assert_eq!(a.etag.len(), 18);
    }

    #[test]
    fn if_none_match_matches_an_exact_tag_a_list_a_star_and_a_weak_tag() {
        let a = asset_with(b"hello");
        let other = asset_with(b"other").etag;
        assert!(a.matches(&a.etag));
        assert!(a.matches(&format!("{other}, {}", a.etag)));
        assert!(a.matches(&format!("{other},{}", a.etag)));
        assert!(a.matches("*"));
        assert!(a.matches(&format!("W/{}", a.etag)));
        assert!(!a.matches(&other));
        assert!(!a.matches(""));
        assert!(!a.matches(&format!("W/{other}")));
    }

    #[test]
    fn an_extension_not_in_the_table_is_an_octet_stream() {
        assert_eq!(content_type("/data.xyz"), "application/octet-stream");
        assert_eq!(content_type("/Makefile"), "application/octet-stream");
        assert_eq!(content_type("/a.dir/file"), "application/octet-stream");
        assert_eq!(content_type("/LOGO.PNG"), "image/png");
        assert_eq!(content_type("/a.woff2"), "font/woff2");
        assert_eq!(content_type("/a.js"), "text/javascript; charset=utf-8");
    }
}
