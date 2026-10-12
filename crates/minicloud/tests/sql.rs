//! The app's SQL store (#50): what `kv` cannot do, and what it refuses.
//!
//! The app under them is `tests/apps/catalog`, a table of books: a schema,
//! an index, an aggregate, and a parameter bound rather than pasted in.

mod common;

use common::*;

fn catalog(config: Option<&'static str>) -> Apps {
    apps(&[AppSpec {
        name: "catalog",
        from: fixtures().join("catalog"),
        config,
    }])
}

/// `/setup`, `/index` and the books, in order.
fn stocked(host: &minicloud::Host) {
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    assert_eq!(get(host.addr, "/catalog/index").status, 200);
    for (title, pages) in [("Dune", "412"), ("Ubik", "224"), ("Solaris", "204")] {
        let added = get(
            host.addr,
            &format!("/catalog/add?title={title}&pages={pages}"),
        );
        assert_eq!(added.status, 200, "{}", added.body);
    }
}

#[test]
fn a_table_an_index_and_an_aggregate_are_what_the_store_is_for() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    stocked(&host);
    // An order an index can serve, which a prefix scan of `kv` could not.
    assert_eq!(
        get(host.addr, "/catalog/titles").body,
        "title|pages\nDune|412\nUbik|224\nSolaris|204\n"
    );
    // An aggregate over the rows, which `kv` has no way to ask for.
    assert_eq!(
        get(host.addr, "/catalog/longest").body,
        "title|max(pages)\nDune|412\n"
    );
    // Its own file, beside where a `kv` store would be and not it.
    assert!(apps.data.join("catalog/sql.sqlite3").is_file());
    assert!(!apps.data.join("catalog/kv.sqlite3").exists());
}

#[test]
fn a_parameter_is_data_and_never_sql() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    stocked(&host);
    let injection = "';%20DROP%20TABLE%20books;%20--";
    assert_eq!(
        get(
            host.addr,
            &format!("/catalog/add?title={injection}&pages=1")
        )
        .status,
        200
    );
    // The table is still there, with the attempt in it as a title.
    let every = get(host.addr, "/catalog/every").body;
    assert_eq!(every.lines().count(), 5, "a header and four books: {every}");
    let found = get(host.addr, &format!("/catalog/find?title={injection}"));
    assert_eq!(found.body, "title\n'; DROP TABLE books; --\n");
}

#[test]
fn a_null_is_not_an_empty_string() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    // `pages` is not `NOT NULL`, and the insert binds the empty string,
    // which an INTEGER column keeps as text rather than as a null — so the
    // null here is the aggregate's over no rows.
    assert_eq!(
        get(host.addr, "/catalog/longest").body,
        "title|max(pages)\nNULL|NULL\n"
    );
}

#[test]
fn a_write_through_query_is_refused_and_points_at_exec() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    stocked(&host);
    let refused = get(host.addr, "/catalog/drop-through-query");
    assert_eq!(refused.status, 422);
    assert!(refused.body.contains("sql.exec"), "{}", refused.body);
    // Nothing was deleted.
    assert_eq!(get(host.addr, "/catalog/every").body.lines().count(), 4);
}

#[test]
fn a_statement_that_does_not_parse_is_the_apps_error_and_names_no_file() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    let refused = get(host.addr, "/catalog/nonsense");
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(
        !refused.body.contains("sqlite3") && !refused.body.contains('/'),
        "the app is told what is wrong, not where the host keeps it: {}",
        refused.body
    );
}

#[test]
fn another_database_and_a_transaction_of_its_own_are_refused() {
    let apps = catalog(None);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    for path in ["/catalog/attach", "/catalog/begin"] {
        let refused = get(host.addr, path);
        assert_eq!(refused.status, 422, "{path}: {}", refused.body);
    }
    // The app is still working after both.
    assert_eq!(
        get(host.addr, "/catalog/add?title=After&pages=1").status,
        200
    );
}

#[test]
fn more_rows_than_max_rows_is_an_error_naming_the_limit() {
    let apps = catalog(Some("grant = [\"sql\"]\n[sql]\nmax_rows = 2\n"));
    let host = start(&apps, 2);
    stocked(&host);
    let refused = get(host.addr, "/catalog/every");
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(refused.body.contains('2'), "{}", refused.body);
    // Two rows is not more than two.
    assert_eq!(
        get(host.addr, "/catalog/find?title=Dune").body,
        "title\nDune\n"
    );
}

#[test]
fn the_store_survives_a_restart() {
    let apps = catalog(None);
    {
        let host = start(&apps, 1);
        stocked(&host);
    }
    let host = start(&apps, 1);
    assert_eq!(
        get(host.addr, "/catalog/titles").body,
        "title|pages\nDune|412\nUbik|224\nSolaris|204\n"
    );
}

#[test]
fn an_app_that_is_not_granted_sql_is_refused_at_load() {
    let apps = catalog(Some("grant = []\n"));
    let host = start(&apps, 1);
    let answer = get(host.addr, "/catalog/setup");
    assert_eq!(answer.status, 503);
    assert!(
        answer.body.contains("requires `sql`"),
        "the reason names the capability: {}",
        answer.body
    );
    assert!(!apps.data.join("catalog/sql.sqlite3").exists());
}
