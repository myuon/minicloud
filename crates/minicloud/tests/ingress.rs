//! The host's own front door (#52): TLS it terminates itself, HTTP/2 over
//! it, gzip, and a hostname with a `*` in it.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use common::*;
use minicloud::Host;

/// A certificate and key for `localhost`, written as a renewal would write
/// them, in a directory that removes itself.
struct Pair {
    dir: std::path::PathBuf,
    pair: minicloud::tls::Pair,
}

impl Pair {
    fn written(label: &str) -> Pair {
        let dir =
            std::env::temp_dir().join(format!("minicloud-ingress-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();
        Pair {
            dir,
            pair: minicloud::tls::Pair { cert, key },
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A client that takes any certificate: these are self-signed, and what is
/// being tested is that the host speaks TLS at all.
fn trusting() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("a client")
}

fn ask(client: &reqwest::Client, url: &str) -> reqwest::Response {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { client.get(url).send().await.expect("an answer") })
}

#[test]
fn the_host_terminates_tls_itself_and_speaks_http_2_over_it() {
    let apps = apps(&[sample("hello")]);
    let certificates = Pair::written("tls");
    let mut options = options(&apps, 2);
    options.tls = Some(certificates.pair.clone());
    let host = Host::start(options).expect("the host starts");
    let url = format!("https://localhost:{}/hello/?name=tls", host.addr.port());

    let answer = ask(&trusting(), &url);
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer.version(),
        reqwest::Version::HTTP_2,
        "ALPN offers h2 first"
    );
    let body = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { answer.text().await.unwrap() });
    assert_eq!(body, "Hello, tls! (GET /)\n");

    // A plain HTTP request to the TLS port is not an answer, and does not
    // take the host down.
    let mut plain = TcpStream::connect(host.addr).unwrap();
    plain
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    plain
        .write_all(b"GET /hello/ HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut said = Vec::new();
    let _ = plain.read_to_end(&mut said);
    assert!(
        !said.starts_with(b"HTTP/1.1 200"),
        "a plain request is not served over the TLS port"
    );
    assert_eq!(ask(&trusting(), &url).status(), 200, "and the host is fine");
}

#[test]
fn a_certificate_renewed_on_disk_is_read_again_without_a_restart() {
    let apps = apps(&[sample("hello")]);
    let certificates = Pair::written("renew");
    let mut options = options(&apps, 1);
    options.tls = Some(certificates.pair.clone());
    let host = Host::start(options).expect("the host starts");
    let url = format!("https://localhost:{}/hello/", host.addr.port());
    assert_eq!(ask(&trusting(), &url).status(), 200);

    // Half a renewal: the certificate is replaced by something that is not
    // one. The pair that is serving is kept.
    std::fs::write(&certificates.pair.cert, "part of a file\n").unwrap();
    assert!(host.reload_certificates().is_err());
    assert_eq!(ask(&trusting(), &url).status(), 200);

    // A whole one is taken.
    let fresh = Pair::written("renew-2");
    std::fs::copy(&fresh.pair.cert, &certificates.pair.cert).unwrap();
    std::fs::copy(&fresh.pair.key, &certificates.pair.key).unwrap();
    host.reload_certificates().expect("the new pair reads");
    assert_eq!(ask(&trusting(), &url).status(), 200);
}

#[test]
fn a_host_whose_certificate_does_not_read_does_not_start() {
    let apps = apps(&[sample("hello")]);
    let certificates = Pair::written("broken");
    std::fs::write(&certificates.pair.key, "not a key\n").unwrap();
    let mut options = options(&apps, 1);
    options.tls = Some(certificates.pair.clone());
    let why = match Host::start(options) {
        Ok(_) => panic!("a host whose key does not read started"),
        Err(why) => why,
    };
    assert!(why.contains("private key"), "{why}");
}

#[test]
fn a_body_worth_compressing_is_gzipped_for_a_client_that_takes_it() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 2);
    let gzipped = send_raw(
        host.addr,
        b"GET /hello/big?bytes=5000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          Accept-Encoding: gzip\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(gzipped.status, 200);
    assert_eq!(gzipped.header("content-encoding"), Some("gzip"));
    assert_eq!(gzipped.header("vary"), Some("accept-encoding"));
    assert!(
        gzipped.bytes.len() < 5000,
        "and it is smaller: {}",
        gzipped.bytes.len()
    );

    // Not asked for, not done.
    let plain = get(host.addr, "/hello/big?bytes=5000");
    assert_eq!(plain.header("content-encoding"), None);
    assert_eq!(plain.bytes.len(), 5000);

    // Too small to be worth it.
    let small = send_raw(
        host.addr,
        b"GET /hello/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          Accept-Encoding: gzip\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(small.header("content-encoding"), None);

    // `gzip;q=0` is a client saying it does not want it.
    let refused = send_raw(
        host.addr,
        b"GET /hello/big?bytes=5000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          Accept-Encoding: gzip;q=0\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(refused.header("content-encoding"), None);
}

#[test]
fn a_hostname_with_a_star_reaches_its_app_one_label_below() {
    let apps = apps(&[
        sample_with(
            "hello",
            "grant = []\n[route]\nhosts = [\"*.apps.example\"]\n",
        ),
        sample("crunch"),
    ]);
    let host = start(&apps, 2);
    // A path and not `/`, which on a hostname that reaches no app is the
    // host's own index rather than a 404.
    let at = |name: &str| {
        send_raw(
            host.addr,
            format!("GET /echo HTTP/1.1\r\nHost: {name}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
    };
    assert_eq!(at("a.apps.example").status, 200);
    assert_eq!(at("b.apps.example").status, 200);
    // One label, never more: a name two below is not it.
    assert_eq!(at("a.b.apps.example").status, 404);
    // And the name itself is not the wildcard.
    assert_eq!(at("apps.example").status, 404);
    // An app with a hostname is not reached by its prefix on another name.
    assert_eq!(at("elsewhere.example").status, 404);
    assert_eq!(
        send_raw(
            host.addr,
            b"GET /crunch/?n=10 HTTP/1.1\r\nHost: elsewhere.example\r\nConnection: close\r\n\r\n"
                .as_slice()
        )
        .status,
        200
    );
}
