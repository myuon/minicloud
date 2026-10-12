//! An app's static files (#57): what the host answers from `assets/`.
//!
//! The point of the feature is the byte: an app's answer is a Cove `String`,
//! which is UTF-8, so an icon could not be an app's answer at all. These
//! tests hold the host to answering the bytes that are on disk, exactly, and
//! to leaving everything else — the queue, the isolate, the app's own
//! routing — alone.

mod common;

use std::net::SocketAddr;

use common::*;
use minicloud::Host;

/// An icon's first bytes: an ICO header, which is not UTF-8 and so could
/// never be a Cove `String`.
const ICON: &[u8] = &[
    0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10, 0x10, 0x00, 0xff, 0xfe,
];
const SHEET: &str = "body{color:#191c21}\n";

/// `hello`, with an `assets/` directory written into the test's copy of it.
fn hello_with_assets(config: Option<&'static str>) -> Apps {
    let apps = match config {
        Some(config) => apps(&[sample_with("hello", config), sample("crunch")]),
        None => apps(&[sample("hello"), sample("crunch")]),
    };
    write_asset(&apps, "style.css", SHEET.as_bytes());
    write_asset(&apps, "favicon.ico", ICON);
    write_asset(&apps, "img/logo.png", &[0x89, b'P', b'N', b'G', 0x00]);
    apps
}

fn write_asset(apps: &Apps, path: &str, bytes: &[u8]) {
    let file = apps.root.join("hello").join("assets").join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, bytes).unwrap();
}

/// `GET path` with the header lines `extra` (each ending in `\r\n`).
fn get_with(addr: SocketAddr, path: &str, extra: &str) -> Answer {
    send_raw(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\r\n")
            .as_bytes(),
    )
}

#[test]
fn a_file_in_assets_is_answered_with_its_type_and_the_hosts_headers() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    let answer = get(host.addr, "/hello/style.css");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, SHEET);
    assert_eq!(
        answer.header("content-type"),
        Some("text/css; charset=utf-8")
    );
    // The host's headers, not the app's: a strong tag, the `[assets]`
    // default, and no guessing at the type.
    assert!(answer
        .header("etag")
        .is_some_and(|tag| tag.starts_with('"')));
    assert_eq!(answer.header("cache-control"), Some("no-cache"));
    assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        answer.header("x-cove-app-version"),
        Some(app_stats(&host, "hello")["version"].as_str().unwrap())
    );
    // Nothing ran: no run headers, and the app's own counters are untouched.
    assert_eq!(answer.header("x-cove-run-wall-us"), None);
    assert_eq!(count(&host, "hello", "served"), 0);
    assert_eq!(count(&host, "hello", "assets_served"), 1);
    assert_eq!(count(&host, "hello", "assets_not_modified"), 0);

    let nested = get(host.addr, "/hello/img/logo.png");
    assert_eq!(nested.status, 200);
    assert_eq!(nested.header("content-type"), Some("image/png"));
}

#[test]
fn bytes_that_are_not_utf8_are_answered_as_they_are() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    let answer = get(host.addr, "/hello/favicon.ico");
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.header("content-type"),
        Some("image/vnd.microsoft.icon")
    );
    assert_eq!(answer.bytes, ICON, "the icon, byte for byte");
    assert_eq!(answer.header("content-length"), Some("11"));
}

#[test]
fn the_etag_answers_304_the_second_time_and_a_changed_file_answers_200() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    let first = get(host.addr, "/hello/style.css");
    let etag = first.header("etag").expect("a tag").to_string();

    let again = get_with(
        host.addr,
        "/hello/style.css",
        &format!("If-None-Match: {etag}\r\n"),
    );
    assert_eq!(again.status, 304);
    assert!(again.bytes.is_empty(), "a 304 carries no body");
    assert_eq!(again.header("etag").unwrap(), etag);
    assert_eq!(again.header("content-type"), None);
    assert_eq!(count(&host, "hello", "assets_served"), 2);
    assert_eq!(count(&host, "hello", "assets_not_modified"), 1);

    // A tag that is not the file's, and `*` which always matches.
    let stale = get_with(
        host.addr,
        "/hello/style.css",
        "If-None-Match: \"0000000000000000\"\r\n",
    );
    assert_eq!(stale.status, 200);
    let any = get_with(host.addr, "/hello/style.css", "If-None-Match: *\r\n");
    assert_eq!(any.status, 304);
}

#[test]
fn a_path_that_names_no_file_and_a_method_that_is_not_a_get_reach_the_app() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    // No such file: the app answers, as it did before there were files.
    let greeting = get(host.addr, "/hello/?name=Cove");
    assert_eq!(greeting.status, 200);
    assert_eq!(greeting.body, "Hello, Cove! (GET /)\n");
    // A file's path, but a `POST`: the app's, so that a form may be posted
    // to a path a file also has.
    let posted = post(host.addr, "/hello/style.css", "");
    assert_eq!(posted.status, 200);
    assert!(
        posted.body.starts_with("Hello, world!"),
        "the app answered, not the file: {:?}",
        posted.body
    );
    assert_eq!(count(&host, "hello", "assets_served"), 0);
    assert_eq!(count(&host, "hello", "served"), 2);
}

#[test]
fn a_changed_file_is_a_new_version_and_an_update_switches_to_it() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    let before = get(host.addr, "/hello/style.css");
    let first = app_stats(&host, "hello")["version"]
        .as_str()
        .unwrap()
        .to_string();

    write_asset(&apps, "style.css", b"body{color:red}\n");
    host.update("hello").expect("the app updates");

    let after = get(host.addr, "/hello/style.css");
    assert_eq!(after.body, "body{color:red}\n");
    let second = app_stats(&host, "hello")["version"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        first, second,
        "a stylesheet is part of the app, so it is part of its version"
    );
    assert_ne!(
        before.header("etag"),
        after.header("etag"),
        "other bytes, another tag"
    );
    assert_eq!(after.header("x-cove-app-version"), Some(second.as_str()));
}

#[test]
fn an_assets_directory_over_max_bytes_refuses_that_app_alone() {
    let apps = hello_with_assets(Some("grant = []\n[assets]\nmax_bytes = 16\n"));
    let host = start(&apps, 2);
    let answer = get(host.addr, "/hello/style.css");
    assert_eq!(answer.status, 503);
    assert!(
        answer.body.contains("max_bytes") && answer.body.contains("assets/"),
        "the reason names the limit and the file: {:?}",
        answer.body
    );
    assert_eq!(app_stats(&host, "hello")["state"], "refused");
    // Its neighbour serves.
    assert_eq!(get(host.addr, "/crunch/?n=10").status, 200);
}

#[test]
fn the_files_in_an_apps_assets_directory_are_what_it_has_in_its_stats() {
    let apps = hello_with_assets(None);
    let host = start(&apps, 2);
    let stats = app_stats(&host, "hello");
    assert_eq!(stats["assets"]["files"], 3);
    assert_eq!(
        stats["assets"]["bytes"],
        (SHEET.len() + ICON.len() + 5) as u64
    );
    assert_eq!(stats["assets"]["max_bytes"], 8 << 20);
    // An app with no files says nothing about them.
    assert!(app_stats(&host, "crunch")["assets"].is_null());
}

/// The admin app's own icon and stylesheet, as they ship: the reason the
/// feature exists (#57). The app is reached by its hostname, so the files
/// are at the hostname's root — which is also where a browser asks for
/// `/favicon.ico` by itself.
#[test]
fn the_admin_app_answers_the_icon_and_the_sheet_it_ships() {
    let apps = apps(&[sample_with(
        "admin",
        "grant = [\"admin\", \"auth\"]\n\
         [limits]\ndeadline = \"60s\"\n\
         [route]\nhosts = [\"admin.localhost\"]\n\
         [access]\ntoken = \"admin\"\n\
         [secrets]\nadmin = { value = \"ui-secret\" }\n",
    )]);
    let host = Host::start(options(&apps, 2)).expect("the host starts");
    let shipped = |name: &str| {
        std::fs::read(bundled().join("admin").join("assets").join(name)).expect("the file ships")
    };
    let ask = |path: &str| {
        send_raw(
            host.addr,
            format!("GET {path} HTTP/1.1\r\nHost: admin.localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
    };

    let icon = ask("/favicon.ico");
    assert_eq!(icon.status, 200);
    assert_eq!(
        icon.header("content-type"),
        Some("image/vnd.microsoft.icon")
    );
    assert_eq!(icon.bytes, shipped("favicon.ico"));
    assert_eq!(&icon.bytes[..4], &[0x00, 0x00, 0x01, 0x00], "an ICO header");

    let svg = ask("/favicon.svg");
    assert_eq!(svg.header("content-type"), Some("image/svg+xml"));
    assert_eq!(svg.bytes, shipped("favicon.svg"));

    let sheet = ask("/style.css");
    assert_eq!(sheet.status, 200);
    assert_eq!(
        sheet.header("content-type"),
        Some("text/css; charset=utf-8")
    );
    assert_eq!(sheet.bytes, shipped("style.css"));
    assert!(
        sheet.body.contains(":root{"),
        "the sheet is CSS, with its braces unescaped"
    );

    // The pages link them, and no longer carry the sheet or allow an inline
    // one. `Basic owner:ui-secret`.
    let page = send_raw(
        host.addr,
        b"GET / HTTP/1.1\r\nHost: admin.localhost\r\nConnection: close\r\n\
          Authorization: Basic b3duZXI6dWktc2VjcmV0\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page
        .body
        .contains("<link rel=stylesheet href=\"/style.css\">"));
    assert!(page.body.contains("href=\"/favicon.svg\""));
    assert!(!page.body.contains("<style>"), "the sheet is a file now");
    let csp = page.header("content-security-policy").expect("a policy");
    assert!(csp.contains("style-src 'self'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "{csp}");

    // The files are answered before the app runs, so they are not behind
    // its login: on the server, Cloudflare Access in front of the hostname
    // is what covers them (docs/design.md).
    assert_eq!(ask("/style.css").status, 200);
}
