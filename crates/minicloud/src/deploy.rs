//! Deploying an app from a directory anywhere: `minicloud deploy` and
//! `minicloud rollback`, and the files they move.
//!
//! An app travels as a **tar archive of the files it consists of**: its
//! `app.toml`, its `.cove` files at any depth, and everything under its
//! `assets/` directory, whatever the extension: the host serves those files
//! as they are, so an image or a font travels as the bytes it is. Nothing
//! else is packed — a README, a sample, a `.git` — and anything hidden (a
//! name starting with `.`, which includes macOS's `._` resource forks) is
//! skipped. The archive is
//! refused, whole, if it holds a symbolic or hard link, a device or a pipe, a
//! path that is absolute or climbs out with `..`, the same path twice, no
//! `app.toml`, or more than [`Limits::standard`] allows. Packing a directory
//! refuses a symbolic link found in it for the same reason. Both ends check:
//! the CLI before it sends, the admin listener before it writes.
//!
//! On the host, `POST /apps/<app>/deploy` (body: the archive) does, under the
//! update lock:
//!
//! 1. writes the files to `<apps>/.deploy/<app>` (the staged `.new` copy);
//! 2. loads them as the app's next version with the running host's config —
//!    its env and secrets, the admin's changes to the app's grant, the
//!    hostnames other apps claim — exactly as an update would. If that
//!    fails, the staged copy is removed and the diagnostics are the answer:
//!    **nothing else changed**, and the current version still serves;
//! 3. moves `<apps>/<app>` to `<apps>/.previous/<app>` (replacing what was
//!    kept there), renames the staged copy to `<apps>/<app>`, and
//! 4. performs the ordinary versioned update ([`crate::server::Host::update`]):
//!    requests in flight finish on the old version.
//!
//! `POST /apps/<app>/rollback` swaps `<apps>/<app>` and `<apps>/.previous/<app>`
//! — after loading the kept copy the same way, so a previous version that no
//! longer checks (a secret since removed) is refused and nothing moves — and
//! performs the update. A second rollback undoes the first.
//!
//! The staged and kept copies live **inside the apps directory** so that
//! every step is a `rename` within one filesystem — under systemd's
//! `ReadWritePaths=` each listed directory is a mount of its own, and a rename
//! between two of them fails — and in dot-directories, which no app can be
//! named (an app's name starts with a letter) and which loading skips.
//!
//! `minicloud deploy --into <apps>` does steps 1–3 without a running host,
//! checking with `minicloud check`'s rules: it is how `deploy/install.sh`
//! installs the bundled apps, and the host picks them up at its next start.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Where a deploy stages the new copy, under the apps directory.
pub const STAGING: &str = ".deploy";
/// Where the version a deploy replaced is kept, under the apps directory.
pub const PREVIOUS: &str = ".previous";

/// How much an app may be.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The files' bytes, together.
    pub max_bytes: u64,
    /// How many files.
    pub max_files: usize,
}

impl Limits {
    /// 16 MiB over at most 4096 files: far more than any app here needs for
    /// its code and a handful of images, far less than would hurt the host.
    pub const fn standard() -> Limits {
        Limits {
            max_bytes: 16 << 20,
            max_files: 4096,
        }
    }

    /// The most an archive of an app within these limits can take: the
    /// files, a header and padding each, and the end.
    pub fn max_archive_bytes(&self) -> u64 {
        self.max_bytes + (self.max_files as u64 + 2) * 1536 + 10240
    }
}

/// An app's files, by path relative to its directory (`/`-separated), and
/// what was left out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppFiles {
    pub files: Vec<(String, Vec<u8>)>,
    /// Files present but not part of an app (a README, a sample), skipped.
    pub skipped: Vec<String>,
}

impl AppFiles {
    /// The files' bytes, together.
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|(_, data)| data.len() as u64).sum()
    }
}

/// Whether a relative path names a file an app consists of: its `app.toml`,
/// a `.cove` file, or anything under the `assets/` directory at its root
/// (any extension, since the host serves those as they are). A file or a
/// directory named `assets` anywhere deeper is not that directory.
fn is_app_file(path: &str) -> bool {
    path == "app.toml"
        || path.ends_with(".cove")
        || path
            .strip_prefix(crate::assets::DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a name is hidden: skipped, with whatever is under it.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Collects the app in `dir`: its `app.toml`, `.cove` files and `assets/`.
/// Refuses a symbolic link anywhere in it, a directory without `app.toml`,
/// and an app over `limits`.
pub fn collect(dir: &Path, limits: Limits) -> Result<AppFiles, String> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "`{}` is a symbolic link; deploy the directory it points to",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("`{}` is not a directory", dir.display()));
    }
    let mut out = AppFiles::default();
    let mut bytes = 0;
    walk(dir, "", limits, &mut out, &mut bytes)?;
    finish(out, dir.display().to_string())
}

fn walk(
    dir: &Path,
    prefix: &str,
    limits: Limits,
    out: &mut AppFiles,
    bytes: &mut u64,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!("`{}` is not a UTF-8 name", entry.path().display()));
        };
        if is_hidden(name) {
            continue;
        }
        let relative = format!("{prefix}{name}");
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read `{}`: {e}", entry.path().display()))?;
        if kind.is_symlink() {
            return Err(format!(
                "`{relative}` is a symbolic link; an app is deployed as plain files"
            ));
        } else if kind.is_dir() {
            walk(&entry.path(), &format!("{relative}/"), limits, out, bytes)?;
        } else if !kind.is_file() {
            return Err(format!("`{relative}` is not a regular file"));
        } else if is_app_file(&relative) {
            let data = std::fs::read(entry.path())
                .map_err(|e| format!("cannot read `{}`: {e}", entry.path().display()))?;
            *bytes += data.len() as u64;
            out.files.push((relative, data));
            within(out.files.len(), *bytes, limits)?;
        } else {
            out.skipped.push(relative);
        }
    }
    Ok(())
}

fn within(files: usize, bytes: u64, limits: Limits) -> Result<(), String> {
    if files > limits.max_files {
        return Err(format!(
            "the app has more than {} files, the most a deploy takes",
            limits.max_files
        ));
    }
    if bytes > limits.max_bytes {
        return Err(format!(
            "the app is more than {} bytes, the most a deploy takes",
            limits.max_bytes
        ));
    }
    Ok(())
}

/// What a collection must have: an `app.toml` at its root.
fn finish(mut out: AppFiles, what: String) -> Result<AppFiles, String> {
    if !out.files.iter().any(|(path, _)| path == "app.toml") {
        return Err(format!("{what} has no `app.toml`: it is not an app"));
    }
    out.files.sort();
    Ok(out)
}

/// The files as a tar archive.
pub fn pack(app: &AppFiles) -> Result<Vec<u8>, String> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in &app.files {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mtime(0);
        builder
            .append_data(&mut header, path, data.as_slice())
            .map_err(|e| format!("cannot pack `{path}`: {e}"))?;
    }
    builder
        .into_inner()
        .map_err(|e| format!("cannot pack the app: {e}"))
}

/// Reads an archive back into an app's files, refusing what [the module
/// documentation](self) lists. Directories are taken as they come; other
/// files are skipped.
pub fn unpack(archive: &[u8], limits: Limits) -> Result<AppFiles, String> {
    if archive.len() as u64 > limits.max_archive_bytes() {
        return Err(format!(
            "the archive is more than {} bytes, the most a deploy takes",
            limits.max_archive_bytes()
        ));
    }
    let mut out = AppFiles::default();
    let mut bytes = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut tar = tar::Archive::new(archive);
    let entries = tar
        .entries()
        .map_err(|e| format!("not a tar archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("not a tar archive: {e}"))?;
        let raw = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&raw).into_owned();
        let relative = relative_path(&raw).map_err(|why| format!("`{shown}` {why}"))?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(format!(
                "`{shown}` is a link; an app is deployed as plain files"
            ));
        }
        let Some(relative) = relative else {
            // `.` or `./`: the app's directory itself.
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        if !(kind.is_file() || kind == tar::EntryType::Continuous) {
            return Err(format!("`{shown}` is not a regular file"));
        }
        if relative.split('/').any(is_hidden) {
            continue;
        }
        if !is_app_file(&relative) {
            out.skipped.push(relative);
            continue;
        }
        if !seen.insert(relative.clone()) {
            return Err(format!("`{relative}` is in the archive twice"));
        }
        let size = entry.header().size().map_err(|e| e.to_string())?;
        within(out.files.len() + 1, bytes + size, limits)?;
        let mut data = Vec::with_capacity(size as usize);
        entry
            .by_ref()
            .take(size)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read `{relative}` from the archive: {e}"))?;
        bytes += data.len() as u64;
        out.files.push((relative, data));
    }
    finish(out, "the archive".to_string())
}

/// A tar entry's path as a relative, `/`-separated path inside the app, or
/// `None` for the app's directory itself; an error for one that is absolute
/// or leaves it.
fn relative_path(raw: &[u8]) -> Result<Option<String>, &'static str> {
    let text = std::str::from_utf8(raw).map_err(|_| "is not a UTF-8 path")?;
    if text.contains('\\') || text.contains('\0') {
        return Err("is not a plain relative path");
    }
    let mut parts = Vec::new();
    for component in Path::new(text).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or("is not UTF-8")?),
            Component::CurDir => {}
            Component::ParentDir => return Err("leaves the app's directory (`..`)"),
            Component::RootDir | Component::Prefix(_) => {
                return Err("is absolute; an archive's paths are relative to the app")
            }
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// Writes the files into `dir`, which is created afresh.
pub fn write(app: &AppFiles, dir: &Path) -> Result<(), String> {
    remove(dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
    for (path, data) in &app.files {
        let to = dir.join(path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
        }
        std::fs::write(&to, data).map_err(|e| format!("cannot write `{}`: {e}", to.display()))?;
    }
    Ok(())
}

fn remove(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove `{}`: {e}", dir.display())),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| {
        format!(
            "cannot move `{}` to `{}`: {e}",
            from.display(),
            to.display()
        )
    })
}

/// The directories one app's deploy works with.
#[derive(Clone, Debug)]
pub struct Places {
    /// `<apps>/<app>`: what the host loads.
    pub current: PathBuf,
    /// `<apps>/.deploy/<app>`: the copy being checked.
    pub staged: PathBuf,
    /// `<apps>/.previous/<app>`: the version the last deploy replaced.
    pub previous: PathBuf,
}

impl Places {
    pub fn new(apps: &Path, name: &str) -> Places {
        Places {
            current: apps.join(name),
            staged: apps.join(STAGING).join(name),
            previous: apps.join(PREVIOUS).join(name),
        }
    }

    /// Writes `app` to the staged copy.
    pub fn stage(&self, app: &AppFiles) -> Result<(), String> {
        write(app, &self.staged)
    }

    /// Drops the staged copy.
    pub fn unstage(&self) {
        let _ = remove(&self.staged);
    }

    /// Whether a previous version is kept.
    pub fn has_previous(&self) -> bool {
        self.previous.join("app.toml").is_file()
    }

    /// Moves the current copy (if any) to the kept previous one, and the
    /// staged copy into its place. A new app leaves no previous version, and
    /// drops one a removed app of the same name left behind. If the second
    /// move fails, the first is undone.
    pub fn switch_in(&self) -> Result<(), String> {
        remove(&self.previous)?;
        let had_current = self.current.exists();
        if had_current {
            if let Some(parent) = self.previous.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
            }
            rename(&self.current, &self.previous)?;
        }
        if let Err(why) = rename(&self.staged, &self.current) {
            if had_current {
                let _ = rename(&self.previous, &self.current);
            }
            return Err(why);
        }
        Ok(())
    }

    /// Puts back what [`Places::switch_in`] replaced: the copy it switched in
    /// is dropped.
    pub fn switch_back(&self) -> Result<(), String> {
        remove(&self.current)?;
        if self.previous.exists() {
            rename(&self.previous, &self.current)?;
        }
        Ok(())
    }

    /// Swaps the current copy with the kept previous one (through the
    /// staging place): a rollback, and a rollback's undo.
    pub fn swap(&self) -> Result<(), String> {
        remove(&self.staged)?;
        if let Some(parent) = self.staged.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
        }
        let had_current = self.current.exists();
        if had_current {
            rename(&self.current, &self.staged)?;
        }
        if let Err(why) = rename(&self.previous, &self.current) {
            if had_current {
                let _ = rename(&self.staged, &self.current);
            }
            return Err(why);
        }
        if had_current {
            rename(&self.staged, &self.previous)?;
        }
        Ok(())
    }
}

/// `minicloud deploy --into <apps>`: stages `files` as the app `name`,
/// checks it as `minicloud check` does (with this process's env), and only
/// if it passes switches it in, keeping the version it replaces. What the
/// check printed is the report; a refusal changes no file. `store` secrets
/// are looked up in `data`'s secret store, as `minicloud check --data` does.
pub fn deploy_into(
    apps: &Path,
    name: &str,
    files: &AppFiles,
    modules: &crate::hosts::HostModules,
    data: Option<&Path>,
) -> Result<crate::toolchain::Report, String> {
    crate::apps::valid_name(name)?;
    let places = Places::new(apps, name);
    let checked = places.stage(files).and_then(|()| {
        crate::toolchain::check_with(&apps.join(STAGING), &[name.to_string()], modules, data)
    });
    let mut report = match checked {
        Ok(report) => report,
        Err(why) => {
            places.unstage();
            return Err(why);
        }
    };
    if !report.ok {
        places.unstage();
        report
            .err
            .push_str(&format!("deploy of `{name}` refused; nothing changed\n"));
        return Ok(report);
    }
    if let Err(why) = places.switch_in() {
        places.unstage();
        return Err(why);
    }
    report.out.push_str(&format!(
        "deployed `{name}` to `{}`{}; a running host loads it with `minicloud update {name}` \
         or at its next start\n",
        places.current.display(),
        if places.has_previous() {
            format!(
                " (the version it replaced is kept in `{}`)",
                places.previous.display()
            )
        } else {
            String::new()
        }
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("minicloud-deploy-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Not UTF-8: a `String` could not hold it.
    const FAVICON: [u8; 6] = [0x00, 0x00, 0x01, 0x00, 0xff, 0xfe];
    const LOGO: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0xff];

    fn app_dir(label: &str) -> PathBuf {
        let dir = scratch(label).join("hello");
        std::fs::create_dir_all(dir.join("text")).unwrap();
        std::fs::write(dir.join("app.toml"), "grant = []\n").unwrap();
        std::fs::write(dir.join("hello.cove"), "// main\n").unwrap();
        std::fs::write(dir.join("text/text.cove"), "// a module\n").unwrap();
        std::fs::write(dir.join("README.md"), "not part of the app\n").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD.cove"), "hidden\n").unwrap();
        std::fs::write(dir.join("._hello.cove"), "a resource fork\n").unwrap();
        std::fs::create_dir_all(dir.join("assets/img")).unwrap();
        std::fs::write(dir.join("assets/style.css"), "body { margin: 0 }\n").unwrap();
        std::fs::write(dir.join("assets/favicon.ico"), FAVICON).unwrap();
        std::fs::write(dir.join("assets/img/logo.png"), LOGO).unwrap();
        std::fs::write(dir.join("assets/.DS_Store"), "hidden\n").unwrap();
        dir
    }

    /// An archive with one entry whose name is written as given, past the
    /// checks the tar crate makes when building.
    fn raw_archive(name: &str, kind: tar::EntryType, data: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut ok = tar::Header::new_ustar();
        ok.set_size(11);
        ok.set_entry_type(tar::EntryType::Regular);
        ok.set_cksum();
        builder
            .append_data(&mut ok, "app.toml", b"grant = []\n".as_slice())
            .unwrap();
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(data.len() as u64);
        header.set_entry_type(kind);
        header.set_cksum();
        builder.append(&header, data).unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn a_directory_packs_its_app_files_only_and_unpacks_the_same() {
        let dir = app_dir("pack");
        let app = collect(&dir, Limits::standard()).unwrap();
        let paths: Vec<&str> = app.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "app.toml",
                "assets/favicon.ico",
                "assets/img/logo.png",
                "assets/style.css",
                "hello.cove",
                "text/text.cove"
            ]
        );
        assert_eq!(app.skipped, ["README.md"]);
        let back = unpack(&pack(&app).unwrap(), Limits::standard()).unwrap();
        assert_eq!(back.files, app.files);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn everything_under_assets_is_packed_but_hidden_files_and_lookalikes_are_not() {
        let dir = app_dir("assets-packed");
        std::fs::create_dir_all(dir.join("text/assets")).unwrap();
        std::fs::write(dir.join("text/assets/nested.css"), "nested\n").unwrap();
        let app = collect(&dir, Limits::standard()).unwrap();
        let paths: Vec<&str> = app.files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"assets/style.css"));
        assert!(paths.contains(&"assets/favicon.ico"));
        assert!(paths.contains(&"assets/img/logo.png"));
        assert!(!paths.iter().any(|p| p.contains(".DS_Store")));
        assert!(!paths.contains(&"text/assets/nested.css"));
        assert!(app.skipped.contains(&"README.md".to_string()));
        assert!(app.skipped.contains(&"text/assets/nested.css".to_string()));
        assert!(!app.skipped.iter().any(|p| p.contains(".DS_Store")));
        let back = unpack(&pack(&app).unwrap(), Limits::standard()).unwrap();
        assert!(!back.files.iter().any(|(p, _)| p.contains(".DS_Store")));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn bytes_that_are_not_utf8_survive_pack_unpack_and_write_unchanged() {
        let dir = app_dir("assets-bytes");
        let app = collect(&dir, Limits::standard()).unwrap();
        let back = unpack(&pack(&app).unwrap(), Limits::standard()).unwrap();
        let out = scratch("assets-bytes-out").join("hello");
        write(&back, &out).unwrap();
        assert_eq!(
            std::fs::read(out.join("assets/favicon.ico")).unwrap(),
            FAVICON
        );
        assert_eq!(
            std::fs::read(out.join("assets/img/logo.png")).unwrap(),
            LOGO
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn a_file_two_directories_deep_under_assets_is_written_with_its_directories() {
        let app = AppFiles {
            files: vec![
                ("app.toml".to_string(), b"grant = []\n".to_vec()),
                ("assets/img/logo.png".to_string(), LOGO.to_vec()),
            ],
            skipped: vec![],
        };
        let out = scratch("assets-nested").join("hello");
        write(&app, &out).unwrap();
        assert!(out.join("assets/img").is_dir());
        assert_eq!(
            std::fs::read(out.join("assets/img/logo.png")).unwrap(),
            LOGO
        );
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_refused_when_packing() {
        let dir = app_dir("symlink");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("secret.cove")).unwrap();
        let refused = collect(&dir, Limits::standard()).unwrap_err();
        assert!(
            refused.contains("`secret.cove` is a symbolic link"),
            "{refused}"
        );
        // A link to a directory is refused too, not followed.
        std::fs::remove_file(dir.join("secret.cove")).unwrap();
        std::os::unix::fs::symlink("/tmp", dir.join("elsewhere")).unwrap();
        let refused = collect(&dir, Limits::standard()).unwrap_err();
        assert!(
            refused.contains("`elsewhere` is a symbolic link"),
            "{refused}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn links_and_escapes_are_refused_when_unpacking() {
        let cases = [
            (
                "../outside.cove",
                tar::EntryType::Regular,
                "leaves the app's directory",
            ),
            (
                "text/../../outside.cove",
                tar::EntryType::Regular,
                "leaves the app's directory",
            ),
            ("/etc/x.cove", tar::EntryType::Regular, "is absolute"),
            ("link.cove", tar::EntryType::Symlink, "is a link"),
            ("hard.cove", tar::EntryType::Link, "is a link"),
            ("pipe.cove", tar::EntryType::Fifo, "is not a regular file"),
            (
                "app.toml",
                tar::EntryType::Regular,
                "is in the archive twice",
            ),
        ];
        for (name, kind, why) in cases {
            let refused = unpack(&raw_archive(name, kind, b"x"), Limits::standard()).unwrap_err();
            assert!(refused.contains(why), "{name}: {refused}");
        }
        // A file that is not part of an app is skipped, not refused.
        let fine = unpack(
            &raw_archive("notes.txt", tar::EntryType::Regular, b"x"),
            Limits::standard(),
        )
        .unwrap();
        assert_eq!(fine.skipped, ["notes.txt"]);
        assert!(unpack(
            b"not a tar archive at all, but long enough",
            Limits::standard()
        )
        .is_err());
    }

    #[test]
    fn an_app_over_the_limits_is_refused_both_ways() {
        let dir = app_dir("size");
        std::fs::write(dir.join("big.cove"), vec![b'/'; 4000]).unwrap();
        let small = Limits {
            max_bytes: 1000,
            max_files: 100,
        };
        let refused = collect(&dir, small).unwrap_err();
        assert!(refused.contains("more than 1000 bytes"), "{refused}");
        let few = Limits {
            max_bytes: 1 << 20,
            max_files: 2,
        };
        assert!(collect(&dir, few)
            .unwrap_err()
            .contains("more than 2 files"));

        let archive = pack(&collect(&dir, Limits::standard()).unwrap()).unwrap();
        let refused = unpack(&archive, small).unwrap_err();
        assert!(refused.contains("bytes"), "{refused}");
        assert!(unpack(&archive, few)
            .unwrap_err()
            .contains("more than 2 files"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_directory_without_app_toml_is_not_an_app() {
        let dir = scratch("noapp");
        std::fs::write(dir.join("x.cove"), "").unwrap();
        assert!(collect(&dir, Limits::standard())
            .unwrap_err()
            .contains("has no `app.toml`"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn switching_in_keeps_the_previous_copy_and_a_swap_restores_it() {
        let apps = scratch("places");
        let places = Places::new(&apps, "hello");
        let version = |label: &str| AppFiles {
            files: vec![("app.toml".to_string(), label.as_bytes().to_vec())],
            skipped: Vec::new(),
        };
        let current = || std::fs::read_to_string(apps.join("hello/app.toml")).unwrap();
        places.stage(&version("one")).unwrap();
        places.switch_in().unwrap();
        assert_eq!(current(), "one");
        assert!(!places.has_previous());
        places.stage(&version("two")).unwrap();
        places.switch_in().unwrap();
        assert_eq!(current(), "two");
        assert!(!places.staged.exists());
        assert!(places.has_previous());
        places.swap().unwrap();
        assert_eq!(current(), "one");
        places.swap().unwrap();
        assert_eq!(current(), "two");
        places.switch_back().unwrap();
        assert_eq!(current(), "one");
        let _ = std::fs::remove_dir_all(apps);
    }
}
