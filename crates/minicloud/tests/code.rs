//! What an app's deploy carries, read back (issue 89): the `host` module's
//! `code` and `codeFile` ([`minicloud::code`]), and the admin app's page
//! over them.
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

/// `host.code(app, 100)` as the fixture prints it: one line a file,
/// `name | kind | bytes | isText | onDisk`.
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

/// Each line with its size left out, which is the part an edit to a shipped
/// file would change: `name | kind | isText | onDisk`.
fn shape(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.split(" | ").collect();
            assert_eq!(parts.len(), 5, "{line}");
            format!("{} | {} | {} | {}", parts[0], parts[1], parts[3], parts[4])
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
    assert_eq!(whole.len(), 5);
    for (asked, want) in [("0", 0), ("1", 1), ("3", 3), ("500", 5), ("9999", 5)] {
        let answer = op(&host, &format!("/code?app=notes&n={asked}"));
        let got: Vec<&str> = answer.body.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(got.len(), want, "n={asked}: {}", answer.body);
    }
    // A negative limit is none, not every file.
    let answer = op(&host, "/code?app=notes&n=-1");
    assert_eq!(answer.body.trim(), "");
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
    assert!(one.body.contains("export fn handle"), "{}", one.body);

    // The admin app's own code, at a depth: its own module beside it.
    let own = page(&host, "/apps/admin/code");
    assert!(own.body.contains("text/text.cove"), "{}", own.body);
    let text = page(&host, "/apps/admin/code?file=text%2Ftext.cove");
    assert!(text.body.contains("export fn escapeHtml"), "{}", text.body);

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
        format!("// <script>alert('x')</script> & \"quoted\"\n{text}"),
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
    assert!(
        one.body
            .contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quoted&quot;"),
        "{}",
        one.body
    );
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
    assert!(one.body.contains("export fn handle"), "{}", one.body);
    assert!(!one.body.contains("not what is running"), "{}", one.body);
}
