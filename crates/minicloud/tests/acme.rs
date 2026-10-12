//! ACME (#52): what the host owns of getting its own certificate.
//!
//! The exchange with an authority is not here — it needs an authority, a
//! public name and port 80 — and neither is anything that would need one.
//! What is here is the door the authority knocks on, and the host starting
//! with no certificate at all without falling over.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::*;
use minicloud::acme::{Challenges, Order, CHALLENGE_PREFIX};
use minicloud::Host;

/// A directory that removes itself.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("minicloud-acme-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Asks the plain listener for `path` and reads the whole answer.
fn ask(port: u16, path: &str, host: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut said = String::new();
    let _ = stream.read_to_string(&mut said);
    said
}

/// A free port, for the plain listener.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn the_plain_listener_answers_a_challenge_and_redirects_everything_else() {
    let apps = apps(&[sample("hello")]);
    let scratch = Scratch::new("challenge");
    let port = free_port();
    let mut options = options(&apps, 1);
    options.acme = Some(Order {
        domains: vec!["tools.example".to_string()],
        email: None,
        staging: true,
        dir: scratch.0.clone(),
    });
    options.acme_http = Some(format!("127.0.0.1:{port}"));
    // No certificate on disk, and the host starts all the same: ordering
    // one is what the first minutes are for.
    let host = Host::start(options).expect("the host starts without a certificate");
    assert!(host.banner().contains("ACME"), "{}", host.banner());

    // Everything but a challenge is sent to https, keeping the path and
    // the query.
    let redirected = ask(port, "/hello/?name=a", "tools.example");
    assert!(redirected.starts_with("HTTP/1.1 308"), "{redirected}");
    assert!(
        redirected.contains("location: https://tools.example/hello/?name=a"),
        "{redirected}"
    );

    // A challenge the host is not offering is not an answer either.
    let unknown = ask(port, &format!("{CHALLENGE_PREFIX}nobody"), "tools.example");
    assert!(unknown.starts_with("HTTP/1.1 308"), "{unknown}");
}

#[test]
fn a_challenge_is_answered_with_the_key_authorization_and_only_while_it_is_offered() {
    // The listener reads the same `Challenges` the order writes into, so
    // this is what the authority would see at each moment.
    let challenges = Challenges::default();
    assert!(challenges.is_empty());
    challenges.offer("the-token", "the-token.the-thumbprint");
    assert_eq!(
        challenges
            .answer(&format!("{CHALLENGE_PREFIX}the-token"))
            .as_deref(),
        Some("the-token.the-thumbprint")
    );
    // Only under the well-known path, and only that token.
    assert_eq!(challenges.answer("/the-token"), None);
    assert_eq!(challenges.answer(&format!("{CHALLENGE_PREFIX}other")), None);
    challenges.withdraw("the-token");
    assert!(challenges.is_empty());
}

#[test]
fn a_certificate_already_on_disk_is_served_and_nothing_is_ordered() {
    let apps = apps(&[sample("hello")]);
    let scratch = Scratch::new("kept");
    // A certificate with years left, as a kept one would be.
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let key = rcgen::KeyPair::generate().unwrap();
    let certified = params.self_signed(&key).unwrap();
    std::fs::write(scratch.0.join("cert.pem"), certified.pem()).unwrap();
    std::fs::write(scratch.0.join("key.pem"), key.serialize_pem()).unwrap();

    let port = free_port();
    let mut options = options(&apps, 1);
    options.acme = Some(Order {
        domains: vec!["localhost".to_string()],
        email: None,
        staging: true,
        dir: scratch.0.clone(),
    });
    options.acme_http = Some(format!("127.0.0.1:{port}"));
    let host = Host::start(options).expect("the host starts");

    // It is serving that certificate: a TLS client gets an answer.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let answer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            client
                .get(format!("https://localhost:{}/hello/", host.addr.port()))
                .send()
                .await
        })
        .expect("an answer");
    assert_eq!(answer.status(), 200);

    // And the account was never made, because nothing was ordered.
    assert!(!scratch.0.join("account.json").exists());
}
