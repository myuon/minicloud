//! What an app's deploy carries, read back (issue 89): the `host` module's
//! `code` and `codeFile` ([`minicloud::code`]), and the admin app's page
//! over them — as a tree, and coloured ([`minicloud::highlight`], issue
//! 100).
//!
//! The module's own window is the `control` fixture as the app `admin`, as
//! in tests/admin.rs: one path an operation, the answer as text. The pages
//! are `apps/admin` itself, over the apps beside it.

mod common;

use common::*;
use minicloud::Host;

/// The admin app's hostname in the module's own tests.
const ADMIN_HOST: &str = "admin.example";

const CONTROL_TOML: &str = "grant = [\"admin\"]\n\n[limits]\ndeadline = \"60s\"\n\n\
[route]\nhosts = [\"admin.example\"]\n";

/// Bytes that are not UTF-8: a PNG's signature, which is what an asset the
/// page cannot show looks like.
const PNG: [u8; 6] = [0x89, b'P', b'N', b'G', 0x00, 0xff];

/// The control fixture as the app `admin`.
fn control() -> AppSpec<'static> {
    AppSpec {
        name: "admin",
        from: fixtures().join("control"),
        config: Some(CONTROL_TOML),
    }
}

/// One operation of the control fixture.
fn op(host: &Host, path: &str) -> Answer {
    send_raw(
        host.addr,
        format!("GET {path} HTTP/1.1\r\nHost: {ADMIN_HOST}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
}

/// `host.code(app, 100)` as the fixture prints it: one line a row,
/// `name | kind | bytes | isText | onDisk | depth | label`.
fn code(host: &Host, app: &str) -> Vec<String> {
    let answer = op(host, &format!("/code?app={app}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
        .body
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Each file with its size left out, which is the part an edit to a shipped
/// file would change: `name | kind | isText | onDisk`. The directory rows
/// are left out too; [`tree`] is what reads those.
fn shape(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.split(" | ").collect();
            assert_eq!(parts.len(), 7, "{line}");
            format!("{} | {} | {} | {}", parts[0], parts[1], parts[3], parts[4])
        })
        .filter(|line| !line.contains(" | dir | "))
        .collect()
}

/// Each row as the page is given it: `depth label`, a directory's label
/// with a `/` after it, which is the tree as a reader sees it.
fn tree(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.split(" | ").collect();
            assert_eq!(parts.len(), 7, "{line}");
            let slash = if parts[1] == "dir" { "/" } else { "" };
            format!("{} {}{slash}", parts[5], parts[6])
        })
        .collect()
}

/// The size `host.code` gave for `name`.
fn bytes_of(lines: &[String], name: &str) -> u64 {
    let line = lines
        .iter()
        .find(|line| line.starts_with(&format!("{name} | ")))
        .unwrap_or_else(|| panic!("no `{name}` in\n{}", lines.join("\n")));
    line.split(" | ").nth(2).unwrap().parse().unwrap()
}

/// `host.codeFile(app, file)`: `(the first line, the text)`, or the error
/// for a name the deploy has no file under.
fn code_file(host: &Host, app: &str, file: &str) -> Result<(String, String), String> {
    let answer = op(
        host,
        &format!("/codeFile?app={app}&file={}", urlencode(file)),
    );
    if answer.status != 200 {
        return Err(answer.body.trim().to_string());
    }
    let (head, text) = answer.body.split_once('\n').expect("a header and a text");
    // The fixture's `answer` adds a newline of its own.
    let text = text.strip_suffix('\n').unwrap_or(text);
    Ok((head.to_string(), text.to_string()))
}

/// `host.codeFile(app, file)`'s colouring as the fixture prints it: why it
/// is shown plain — `""` where it is not — and the pieces, each a kind and
/// a run of the file's own text.
fn code_pieces(host: &Host, app: &str, file: &str) -> (String, Vec<(String, String)>) {
    let answer = op(
        host,
        &format!("/codePieces?app={app}&file={}", urlencode(file)),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let (head, rest) = answer
        .body
        .split_once('\n')
        .expect("a header and the pieces");
    let plain = head.split(" | ").next().unwrap_or_default().to_string();
    // The fixture's `answer` adds a newline of its own; every piece before
    // it ends with the NUL the fixture wrote.
    let rest = rest.strip_suffix('\n').unwrap_or(rest);
    let mut parts = rest.split('\0');
    let mut pieces = Vec::new();
    while let (Some(kind), Some(text)) = (parts.next(), parts.next()) {
        pieces.push((kind.to_string(), text.to_string()));
    }
    (plain, pieces)
}

/// A query value, percent-encoded: the names here hold `/`, `..` and worse.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// `notes` with an `assets/` written into it: it ships none, and what the
/// host serves for an app is half of what a deploy carries.
fn with_assets(apps: &Apps) {
    let assets = apps.root.join("notes").join(minicloud::assets::DIR);
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("style.css"), "body{}").unwrap();
    std::fs::write(assets.join("logo.png"), PNG).unwrap();
}

#[test]
fn the_listing_is_the_app_toml_the_cove_files_and_the_assets() {
    let apps = apps(&[control(), sample("notes"), sample("algo")]);
    with_assets(&apps);
    let host = start(&apps, 2);

    // `app.toml` first — it is what decides what the app may do — then the
    // `.cove` files by path, then the assets by path. An asset that is not
    // UTF-8 says so rather than carrying a text it has not got.
    let listed = code(&host, "notes");
    assert_eq!(
        shape(&listed),
        [
            "app.toml | config | true | same",
            "notes.cove | cove | true | same",
            "notes_test.cove | cove | true | same",
            "assets/logo.png | asset | false | same",
            "assets/style.css | asset | true | same",
        ]
    );
    // The size is the whole file's, whatever is carried of it.
    for (name, path) in [
        ("app.toml", "notes/app.toml"),
        ("notes.cove", "notes/notes.cove"),
        ("assets/logo.png", "notes/assets/logo.png"),
    ] {
        let on_disk = std::fs::metadata(apps.root.join(path)).unwrap().len();
        assert_eq!(bytes_of(&listed, name), on_disk, "{name}");
    }

    // A module of the app's own is named by its path within the app, at
    // whatever depth.
    let algo = shape(&code(&host, "algo"));
    assert_eq!(algo[0], "app.toml | config | true | same");
    for name in [
        "algo.cove",
        "pages.cove",
        "text/text.cove",
        "sat/sat_test.cove",
    ] {
        assert!(
            algo.contains(&format!("{name} | cove | true | same")),
            "{name} in\n{}",
            algo.join("\n")
        );
    }
    // Nothing of the standard library, which is in the same source map.
    assert!(!algo.iter().any(|line| line.contains("std")), "{algo:?}");
}

#[test]
fn one_files_text_is_the_text_the_host_is_running() {
    let apps = apps(&[control(), sample("notes")]);
    with_assets(&apps);
    let host = start(&apps, 2);

    let (head, text) = code_file(&host, "notes", "notes.cove").unwrap();
    assert_eq!(
        head,
        format!("notes.cove | {} | true | false | same", text.len())
    );
    assert_eq!(
        text,
        std::fs::read_to_string(apps.root.join("notes/notes.cove")).unwrap()
    );

    // `app.toml` is not kept as text by the load, so it is read from the
    // app's directory — which is why it is the one file that cannot
    // disagree with what is on disk.
    let (head, text) = code_file(&host, "notes", "app.toml").unwrap();
    assert!(head.starts_with("app.toml | "), "{head}");
    assert!(text.contains("grant = [\"kv\", \"log\"]"), "{text}");

    // An asset that is text is its text; one that is not has none, and its
    // size is still the whole file's.
    let (head, text) = code_file(&host, "notes", "assets/style.css").unwrap();
    assert_eq!(head, "assets/style.css | 6 | true | false | same");
    assert_eq!(text, "body{}");
    let (head, text) = code_file(&host, "notes", "assets/logo.png").unwrap();
    assert_eq!(head, "assets/logo.png | 6 | false | false | same");
    assert_eq!(text, "");
}

/// A file the deploy does not hold is an `Err`, and so is every way of
/// asking for one that is not in it: the names a listing gave are the only
/// names there are, so a name is never joined to a path.
#[test]
fn a_name_the_deploy_has_no_file_under_is_an_error() {
    let apps = apps(&[control(), sample("notes"), sample("hello")]);
    let host = start(&apps, 2);

    for name in [
        "nothing.cove",
        // Another app's file, by name and by path.
        "hello.cove",
        "../hello/hello.cove",
        "../../etc/passwd",
        "notes/../hello/hello.cove",
        "/etc/passwd",
        "/notes/notes.cove",
        "./notes.cove",
        "..",
        "",
        // The directories themselves, and a name that only looks like one.
        "assets",
        "assets/",
        "app.toml/",
        "APP.TOML",
    ] {
        let why = code_file(&host, "notes", name).expect_err(name);
        assert!(
            why.contains("has no file") && why.contains("notes"),
            "{name}: {why}"
        );
    }
    // What `hello` holds is still readable as `hello`: the refusal above is
    // about the app asked for, not about the file.
    assert!(code_file(&host, "hello", "hello.cove").is_ok());

    // An app the host does not have reads nothing rather than reaching into
    // a directory, the same guard the store's reads rest on.
    for app in ["..", "../admin", "nothing", ""] {
        assert!(code(&host, app).is_empty(), "{app}");
        assert!(code_file(&host, app, "app.toml").is_err(), "{app}");
    }
}

/// What is in memory is what is running, and the directory can hold
/// something else: a deploy staged and not switched to, an edit made on the
/// machine. The listing says which files those are.
#[test]
fn a_file_on_disk_that_is_not_the_one_running_is_said_to_differ() {
    let apps = apps(&[control(), sample("notes")]);
    with_assets(&apps);
    let host = start(&apps, 2);
    assert!(shape(&code(&host, "notes"))
        .iter()
        .all(|line| line.ends_with("| same")));

    // An edit that keeps the length, which no size or time would catch.
    let source = apps.root.join("notes/notes.cove");
    let text = std::fs::read_to_string(&source).unwrap();
    std::fs::write(&source, text.replacen("//", "/*", 1)).unwrap();
    // A file taken away, and an asset's bytes changed.
    std::fs::remove_file(apps.root.join("notes/notes_test.cove")).unwrap();
    std::fs::write(apps.root.join("notes/assets/style.css"), "body{ }").unwrap();

    let listed = shape(&code(&host, "notes"));
    assert_eq!(
        listed,
        [
            "app.toml | config | true | same",
            "notes.cove | cove | true | differs",
            "notes_test.cove | cove | true | missing",
            "assets/logo.png | asset | false | same",
            "assets/style.css | asset | true | differs",
        ]
    );
    // And what is answered is still what is running, not the edit.
    let (head, running) = code_file(&host, "notes", "notes.cove").unwrap();
    assert!(head.ends_with("| differs"), "{head}");
    assert_eq!(running, text);

    // `app.toml` is the exception, read from the directory every time.
    std::fs::write(
        apps.root.join("notes/app.toml"),
        "grant = [\"kv\", \"log\"]\n# edited on the machine\n",
    )
    .unwrap();
    let (head, config) = code_file(&host, "notes", "app.toml").unwrap();
    assert!(head.ends_with("| same"), "{head}");
    assert!(config.contains("edited on the machine"), "{config}");
}

/// A refused app has no source map — it was never compiled — so what is
/// deployed is its `app.toml` and whatever the host read of its assets. Its
/// `app.toml` is exactly the file somebody looking at a refusal wants.
#[test]
fn a_refused_app_still_says_what_its_app_toml_is() {
    let apps = apps(&[control(), fixture("greedy")]);
    let host = start(&apps, 2);
    assert_eq!(
        shape(&code(&host, "greedy")),
        ["app.toml | config | true | same"]
    );
    let (_, text) = code_file(&host, "greedy", "app.toml").unwrap();
    assert!(text.contains("grant"), "{text}");
    assert!(code_file(&host, "greedy", "greedy.cove").is_err());
}

/// One file's text is cut at what a page can carry, and says it was cut
/// rather than ending quietly.
#[test]
fn a_file_longer_than_a_page_carries_says_it_was_cut() {
    let apps = apps(&[control(), sample("notes")]);
    let limit = minicloud::code::READ_TEXT_BYTES;
    let assets = apps.root.join("notes").join(minicloud::assets::DIR);
    std::fs::create_dir_all(&assets).unwrap();
    // One character over the limit, and the character is three bytes, so a
    // cut at the limit falls inside it: the text has to stop before it.
    let long = format!("{}あ", "x".repeat(limit));
    std::fs::write(assets.join("long.txt"), &long).unwrap();
    let host = start(&apps, 2);

    let (head, text) = code_file(&host, "notes", "assets/long.txt").unwrap();
    assert_eq!(
        head,
        format!("assets/long.txt | {} | true | true | same", long.len())
    );
    assert_eq!(text.len(), limit);
    assert!(!text.ends_with('あ'), "cut inside a character");

    // A file of exactly the limit is not cut.
    std::fs::write(assets.join("long.txt"), "x".repeat(limit)).unwrap();
    let apps2 = apps;
    let host = start(&apps2, 2);
    let (head, text) = code_file(&host, "notes", "assets/long.txt").unwrap();
    assert_eq!(
        head,
        format!("assets/long.txt | {limit} | true | false | same")
    );
    assert_eq!(text.len(), limit);
}

/// The listing is bounded: a deploy with more files than one page carries
/// gets the first of them, and `limit` is clamped to the module's ceiling.
#[test]
fn the_listing_carries_at_most_what_was_asked_for() {
    let apps = apps(&[control(), sample("notes")]);
    with_assets(&apps);
    let host = start(&apps, 2);
    let whole = code(&host, "notes");
    // Five files and the `assets` directory they are under one of.
    assert_eq!(whole.len(), 6);
    assert_eq!(shape(&whole).len(), 5);
    // `limit` counts files, so the ceiling means what it did before there
    // were directory rows; the directory comes with the first file under
    // it.
    for (asked, want) in [
        ("0", 0),
        ("1", 1),
        ("3", 3),
        ("4", 5),
        ("500", 6),
        ("9999", 6),
    ] {
        let answer = op(&host, &format!("/code?app=notes&n={asked}"));
        let got: Vec<&str> = answer.body.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(got.len(), want, "n={asked}: {}", answer.body);
    }
    // A negative limit is none, not every file.
    let answer = op(&host, "/code?app=notes&n=-1");
    assert_eq!(answer.body.trim(), "");
}

/// The names carry the nesting and the host does it: a row says how deep it
/// is, and a directory is a row of its own where one begins.
///
/// The order the listing had survives it — `app.toml` first because it
/// decides what the app may do, then the code, then the assets — which is
/// why one pass over the names is enough to group them.
#[test]
fn the_listing_is_a_tree_the_host_computed_the_depth_of() {
    let apps = apps(&[control(), sample("notes"), sample("algo")]);
    with_assets(&apps);
    let deeper = apps
        .root
        .join("notes")
        .join(minicloud::assets::DIR)
        .join("img");
    std::fs::create_dir_all(&deeper).unwrap();
    std::fs::write(deeper.join("icon.svg"), "<svg/>").unwrap();
    let host = start(&apps, 2);

    assert_eq!(
        tree(&code(&host, "notes")),
        [
            "0 app.toml",
            "0 notes.cove",
            "0 notes_test.cove",
            "0 assets/",
            "1 img/",
            "2 icon.svg",
            "1 logo.png",
            "1 style.css",
        ]
    );
    // A module of the app's own is a directory under the code, with the
    // app's own files before all of them: the order is the source map's,
    // which is the order the app was loaded in.
    let algo = tree(&code(&host, "algo"));
    assert_eq!(&algo[..2], ["0 app.toml", "0 algo.cove"]);
    for row in [
        "0 sat/",
        "1 sat.cove",
        "1 sat_test.cove",
        "0 text/",
        "1 text.cove",
    ] {
        assert!(algo.contains(&row.to_string()), "{row} in {algo:?}");
    }
    // No row is deeper than the directory that holds it.
    assert!(
        algo.iter()
            .all(|row| row.starts_with("0 ") || row.starts_with("1 ")),
        "{algo:?}"
    );
    // A directory says what the files under it come to, however deep they
    // are: the question is "how large is `assets/`".
    let listed = code(&host, "notes");
    let under: u64 = ["assets/img/icon.svg", "assets/logo.png", "assets/style.css"]
        .iter()
        .map(|name| bytes_of(&listed, name))
        .sum();
    assert_eq!(bytes_of(&listed, "assets"), under);
    assert_eq!(bytes_of(&listed, "assets/img"), 6);
    // A directory is a row and not a file: it has no text, and the name of
    // one is not a name the deploy has a file under.
    assert!(code_file(&host, "notes", "assets").is_err());
    assert!(code_file(&host, "notes", "assets/img").is_err());
}

/// The point of the colouring, and the one thing it must not get wrong:
/// **the pieces concatenated are the file**, so showing them shows the file.
///
/// Every language a deploy carries is here, because each has a reader of its
/// own: `.cove` the lexer the host compiled it with, and `app.toml`, CSS and
/// XML a scanner each.
#[test]
fn the_pieces_of_a_file_are_the_file_byte_for_byte() {
    let apps = apps(&[control(), sample("notes")]);
    with_assets(&apps);
    let assets = apps.root.join("notes").join(minicloud::assets::DIR);
    std::fs::write(
        assets.join("icon.svg"),
        "<!-- an icon -->\n<svg viewBox=\"0 0 8 8\"><circle r=\"4\"/></svg>\n",
    )
    .unwrap();
    std::fs::write(
        assets.join("style.css"),
        "/* a */\n:root{--bg:#fff}\na:hover{color:var(--bg)}\n",
    )
    .unwrap();
    // `notes.cove` ships `///` comments and no `//` ones; both have to come
    // out as comments, and the ordinary one is the whole reason the gaps
    // between the spans are walked.
    let source = apps.root.join("notes/notes.cove");
    let text = std::fs::read_to_string(&source).unwrap();
    std::fs::write(&source, format!("// why it is here\n{text}")).unwrap();
    let host = start(&apps, 2);

    for name in [
        "app.toml",
        "notes.cove",
        "notes_test.cove",
        "assets/style.css",
        "assets/icon.svg",
    ] {
        let (_, text) = code_file(&host, "notes", name).unwrap();
        let (plain, pieces) = code_pieces(&host, "notes", name);
        assert_eq!(plain, "", "{name} was not coloured");
        let whole: String = pieces.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(whole, text, "{name}");
        // And something in it is coloured, rather than one plain run.
        assert!(
            pieces.iter().any(|(kind, _)| kind != "plain"),
            "{name} came out with no colour at all"
        );
    }

    // An ordinary `//` is not a token to the lexer at all — it keeps the
    // `///` and drops the rest — so it is the gaps between the spans that
    // are walked to find one. Both come out as comments.
    let (_, pieces) = code_pieces(&host, "notes", "notes.cove");
    assert!(
        pieces.contains(&("comment".to_string(), "// why it is here".to_string())),
        "no ordinary comment in {pieces:?}"
    );
    assert!(
        pieces
            .iter()
            .any(|(kind, text)| kind == "doc" && text.starts_with("/// A note per path")),
        "no doc comment in {pieces:?}"
    );
    assert!(
        pieces
            .iter()
            .any(|(kind, text)| kind == "keyword" && text == "export"),
        "no keyword in {pieces:?}"
    );

    // A file that is not UTF-8 has no text, so there is nothing of it to
    // colour and nothing to say about colouring it: the page says it is not
    // text at all, which is more than a word about its colours would.
    let (plain, pieces) = code_pieces(&host, "notes", "assets/logo.png");
    assert!(pieces.is_empty(), "{pieces:?}");
    assert_eq!(plain, "");
}

/// Both limits are said rather than quietly applied, and a file the host
/// has no reader for is shown plain rather than shown wrongly.
#[test]
fn a_file_the_host_will_not_colour_says_why() {
    let apps = apps(&[control(), sample("notes")]);
    let assets = apps.root.join("notes").join(minicloud::assets::DIR);
    std::fs::create_dir_all(&assets).unwrap();
    // No reader for it: a `.txt` is text and nothing else is known of it.
    std::fs::write(assets.join("notes.txt"), "a = 1\n").unwrap();
    // Longer than the window carries, so the text stops short of the file.
    std::fs::write(
        assets.join("long.css"),
        format!("a{{b:c}}{}", "x".repeat(minicloud::code::READ_TEXT_BYTES)),
    )
    .unwrap();
    // Shorter than that and still more pieces than one page carries.
    let dense = "a{b:c}".repeat(minicloud::code::HIGHLIGHT_PIECES / 6 + 10);
    std::fs::write(assets.join("dense.css"), &dense).unwrap();
    let host = start(&apps, 2);

    for (name, why) in [
        ("assets/notes.txt", "unknown"),
        ("assets/long.css", "cut"),
        ("assets/dense.css", "dense"),
    ] {
        let (plain, pieces) = code_pieces(&host, "notes", name);
        assert_eq!(plain, why, "{name}");
        assert!(pieces.is_empty(), "{name}: {pieces:?}");
        // The text is still the file's, cut or not: only the colour is gone.
        assert!(code_file(&host, "notes", name).is_ok(), "{name}");
    }
    // The same file under the limit is coloured, so the limit is what is
    // being tested and not the scanner.
    std::fs::write(assets.join("dense.css"), &dense[..60]).unwrap();
    let host = start(&apps, 2);
    assert_eq!(code_pieces(&host, "notes", "assets/dense.css").0, "");
}

/// Only the app named `admin` may hold the capability these reads are on,
/// which is what keeps them out of every other app (`config::ADMIN_APP`).
#[test]
fn no_app_but_admin_can_be_granted_the_capability_that_reads_code() {
    let apps = apps(&[
        control(),
        sample_with("hello", "grant = [\"admin\"]\n"),
        sample("notes"),
    ]);
    let host = start(&apps, 2);
    let refused = get(host.addr, "/hello/");
    assert_eq!(refused.status, 503);
    assert!(
        refused.body.contains("only the app `admin` may be granted"),
        "{}",
        refused.body
    );
    // And no change makes it so.
    let answer = op(&host, "/configure?app=notes&grant=kv,log,admin");
    assert_eq!(answer.status, 422, "{}", answer.body);
    assert!(
        answer.body.contains("only the app `admin` may be granted"),
        "{}",
        answer.body
    );
}

// ------------------------------------------------- the admin app's pages

const UI_HOST: &str = "covtools-admin.ramda.io";
/// `Basic base64("owner:ui-secret")`.
const UI_LOGIN: &str = "Basic b3duZXI6dWktc2VjcmV0";

/// `apps/admin`'s own config, with the secret as a value rather than from
/// the environment.
fn admin_app_toml() -> String {
    std::fs::read_to_string(bundled().join("admin/app.toml"))
        .unwrap()
        .replace("{ env = \"ADMIN_UI_TOKEN\" }", "{ value = \"ui-secret\" }")
}

fn ui_apps() -> Apps {
    let config: &'static str = Box::leak(admin_app_toml().into_boxed_str());
    apps(&[sample_with("admin", config), sample("notes")])
}

/// A page of the admin app.
fn page(host: &Host, path: &str) -> Answer {
    send_raw(
        host.addr,
        format!(
            "GET {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\n\r\n"
        )
        .as_bytes(),
    )
}

#[test]
fn the_page_shows_what_the_version_it_is_running_is_made_of() {
    let apps = ui_apps();
    with_assets(&apps);
    let host = start(&apps, 2);

    let listing = page(&host, "/apps/notes/code");
    assert_eq!(listing.status, 200, "{}", listing.body);
    // Every file, with the way to each one's text.
    for name in ["app.toml", "notes.cove", "notes_test.cove"] {
        assert!(listing.body.contains(&format!(">{name}</code>")), "{name}");
    }
    assert!(
        listing.body.contains("assets/style.css"),
        "{}",
        listing.body
    );
    assert!(listing.body.contains("asset, not text"), "{}", listing.body);
    assert!(
        listing.body.contains("code?file=assets%2Flogo.png"),
        "{}",
        listing.body
    );
    // `app.toml` is read from the directory, so it is not compared.
    assert!(listing.body.contains("read from there"), "{}", listing.body);
    // Nothing differs, so nothing is said of a difference.
    assert!(!listing.body.contains("class=problem"), "{}", listing.body);

    // The app's own page is the way here, beside the version it is about.
    let app = page(&host, "/apps/notes");
    assert!(
        app.body.contains("/apps/notes/code\">what it is"),
        "{}",
        app.body
    );

    // One file: its text, and the facts about it.
    let one = page(&host, "/apps/notes/code?file=notes.cove");
    assert_eq!(one.status, 200, "{}", one.body);
    assert!(one.body.contains("<pre class=code>"), "{}", one.body);
    // The text is coloured, which is what every word of it being in a span
    // of its own looks like.
    assert!(
        one.body
            .contains("<span class=k>export</span> <span class=k>fn</span> handle"),
        "{}",
        one.body
    );

    // The admin app's own code, at a depth: its own module as a directory,
    // the file under it, and the whole path where the link points. The
    // depth is the markup — the module's files are a list inside the item
    // that names the module — and not a padding on a cell of a table.
    let own = page(&host, "/apps/admin/code");
    assert!(
        own.body.contains("<li class=dir><code>text/</code>"),
        "{}",
        own.body
    );
    assert!(
        own.body.contains("title=\"text/text.cove\""),
        "{}",
        own.body
    );
    assert!(!own.body.contains("<table"), "{}", own.body);
    let text = page(&host, "/apps/admin/code?file=text%2Ftext.cove");
    assert!(
        text.body.contains("<span class=k>export</span>"),
        "{}",
        text.body
    );
    assert!(text.body.contains("escapeHtml"), "{}", text.body);

    // An asset that is not text: where the host answers it, not a page
    // pretending to render it. `notes` is reached by path, so the path
    // carries its name.
    let logo = page(&host, "/apps/notes/code?file=assets%2Flogo.png");
    assert_eq!(logo.status, 200, "{}", logo.body);
    assert!(logo.body.contains("Not text"), "{}", logo.body);
    assert!(!logo.body.contains("<pre class=code>"), "{}", logo.body);
    assert!(
        logo.body.contains("<code>/notes/logo.png</code>"),
        "{}",
        logo.body
    );
    // The admin app is reached by hostname, which carries the whole path.
    let icon = page(&host, "/apps/admin/code?file=assets%2Ffavicon.ico");
    assert!(
        icon.body.contains("<code>/favicon.ico</code>")
            && icon.body.contains("covtools-admin.ramda.io"),
        "{}",
        icon.body
    );

    // A file the deploy has not got says so, as a missing key does.
    let missing = page(&host, "/apps/notes/code?file=..%2Fadmin%2Fadmin.cove");
    assert_eq!(missing.status, 200, "{}", missing.body);
    assert!(missing.body.contains("Not read."), "{}", missing.body);
    assert!(
        !missing.body.contains("<pre class=code>"),
        "{}",
        missing.body
    );

    // The page reads, whatever is posted to it.
    let posted = send_raw(
        host.addr,
        format!(
            "POST /apps/notes/code HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\nOrigin: https://{UI_HOST}\r\n\
             Sec-Fetch-Site: same-origin\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(posted.status, 404, "{}", posted.body);
}

/// Every `<ul>`, `</ul>`, `<li>` and `</li>` of the page's tree, in the
/// order they are written, from the list that opens it to the one that
/// closes it. The tree is the only list on the page.
fn tree_tags(body: &str) -> Vec<&str> {
    let from = body.find("<ul class=tree>").expect("a tree on the page");
    let mut tags = Vec::new();
    let mut lists = 0;
    for piece in body[from..].split('<').skip(1) {
        let tag = piece.split(['>', ' ']).next().unwrap_or("");
        match tag {
            "ul" => lists += 1,
            "/ul" => lists -= 1,
            "li" | "/li" => {}
            _ => continue,
        }
        tags.push(tag);
        if lists == 0 {
            break;
        }
    }
    tags
}

/// **The tree closes every list it opens**, whatever the depths do. The
/// nesting is the markup now rather than a padding a cell is given, so a
/// list left open is a page that reads as the wrong shape from there on,
/// and the depths the page walks are the host's and not its own.
#[test]
fn the_tree_closes_every_list_it_opens() {
    let apps = ui_apps();
    with_assets(&apps);
    // An asset two directories down, so the row after the last of them is
    // two levels shallower and two lists have to close at once.
    let icons = apps
        .root
        .join("notes")
        .join(minicloud::assets::DIR)
        .join("img")
        .join("icons");
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::write(icons.join("deep.svg"), "<svg/>").unwrap();
    let host = start(&apps, 2);

    let listing = page(&host, "/apps/notes/code");
    assert_eq!(listing.status, 200, "{}", listing.body);
    // A list closes inside the item that opened it, an item closes inside
    // the list that holds it, and nothing is left open at the end.
    let tags = tree_tags(&listing.body);
    let mut open: Vec<&str> = Vec::new();
    for tag in &tags {
        match *tag {
            "ul" | "li" => open.push(tag),
            "/ul" => assert_eq!(open.pop(), Some("ul"), "{tags:?}"),
            "/li" => assert_eq!(open.pop(), Some("li"), "{tags:?}"),
            _ => unreachable!("{tag}"),
        }
    }
    assert!(open.is_empty(), "{tags:?}");
    // `assets/img/icons/deep.svg` to `assets/logo.png` is two levels: the
    // file's item, then `icons/` and `img/`, each with the item that named
    // it, and the next file is an item of the list that is left.
    assert!(
        listing.body.contains(
            "</li></ul></li></ul></li><li><a href=\"/apps/notes/code?file=assets%2Flogo.png\""
        ),
        "{}",
        listing.body
    );
}

/// The page shows code, which is markup more often than not: every value
/// the host gives it — a file's name as well as its text — goes through
/// `escapeHtml`, or this page would be the way into the admin app.
#[test]
fn the_page_escapes_the_code_and_the_names_it_shows() {
    let apps = ui_apps();
    let assets = apps.root.join("notes").join(minicloud::assets::DIR);
    std::fs::create_dir_all(&assets).unwrap();
    // An asset may be named anything a request path can carry, `<` and `&`
    // included ([`minicloud::assets`]).
    std::fs::write(assets.join("x<img src=y>&.png"), PNG).unwrap();
    // A comment is part of the file the host compiled, so it is part of
    // what the page shows.
    let source = apps.root.join("notes/notes.cove");
    let text = std::fs::read_to_string(&source).unwrap();
    std::fs::write(
        &source,
        format!(
            "// <script>alert('x')</script> & \"quoted\"\n\
             /// <img src=y> in a doc comment\n\
             fn markup() -> String {{ \"<img src=y onerror=alert(1)>\" }}\n\
             {text}"
        ),
    )
    .unwrap();
    let host = start(&apps, 2);

    let listing = page(&host, "/apps/notes/code");
    assert_eq!(listing.status, 200, "{}", listing.body);
    assert!(!listing.body.contains("<img src=y>"), "{}", listing.body);
    assert!(
        listing.body.contains("x&lt;img src=y&gt;&amp;.png"),
        "{}",
        listing.body
    );

    let one = page(&host, "/apps/notes/code?file=notes.cove");
    assert_eq!(one.status, 200, "{}", one.body);
    assert!(!one.body.contains("<script>"), "{}", one.body);
    assert!(!one.body.contains("<img src=y"), "{}", one.body);
    // Every kind of piece is escaped, because the escaping is one call in
    // one place and not a property of the kind: a comment, a doc comment
    // and a string literal are each the file's own bytes.
    for coloured in [
        "<span class=c>// &lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quoted&quot;</span>",
        "<span class=d>/// &lt;img src=y&gt; in a doc comment</span>",
        "<span class=s>&quot;&lt;img src=y onerror=alert(1)&gt;&quot;</span>",
    ] {
        assert!(one.body.contains(coloured), "{coloured}\nin\n{}", one.body);
    }
    // The name a request made up is escaped too, where the page says it.
    let sneaky = page(&host, "/apps/notes/code?file=%3Cimg%20src%3Dx%3E");
    assert_eq!(sneaky.status, 200, "{}", sneaky.body);
    assert!(!sneaky.body.contains("<img"), "{}", sneaky.body);
    assert!(sneaky.body.contains("&lt;img src=x&gt;"), "{}", sneaky.body);
}

/// A difference between what is running and what the directory holds is
/// said above the listing, because it is what a deploy that did not switch
/// over looks like from here.
#[test]
fn the_page_says_when_the_directory_is_not_what_is_running() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    let before = page(&host, "/apps/notes/code");
    assert!(!before.body.contains("class=problem"), "{}", before.body);

    std::fs::write(
        apps.root.join("notes/notes.cove"),
        "// not what is running\n",
    )
    .unwrap();
    let after = page(&host, "/apps/notes/code");
    assert_eq!(after.status, 200, "{}", after.body);
    assert!(
        after
            .body
            .contains("One file in the app&#39;s directory is not the one that is running."),
        "{}",
        after.body
    );
    assert!(after.body.contains(">differs<"), "{}", after.body);
    // And the file answered is still the one that is running.
    let one = page(&host, "/apps/notes/code?file=notes.cove");
    assert!(
        one.body
            .contains("<span class=k>export</span> <span class=k>fn</span> handle"),
        "{}",
        one.body
    );
    assert!(!one.body.contains("not what is running"), "{}", one.body);
}
