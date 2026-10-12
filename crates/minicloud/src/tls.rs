//! TLS the host terminates itself, so that a certificate and a key are all
//! it needs to be reached directly.
//!
//! Until this, `--addr` was plain HTTP and something else — cloudflared,
//! nginx, Caddy — had to sit in front and hold the certificate. With
//! `--tls-cert` and `--tls-key` the host holds it: one process, one port,
//! nothing between it and the client.
//!
//! **The files are read at start and at every reload, never per
//! connection.** A certificate that is renewed on disk (certbot, lego, a
//! copy from somewhere) reaches the host when it is told to reload it —
//! `SIGHUP`, which is what a renewal hook runs — and not before, so a
//! half-written pair is never served. A reload that does not read keeps the
//! pair that is serving, and says why.
//!
//! **ACME is not here.** Getting a certificate is a protocol with an
//! account, a challenge and a renewal clock, and the host would have to
//! answer `/.well-known/acme-challenge/` before it has a certificate at
//! all. Reading a pair that something else renews is the smaller half, and
//! it is the half that lets the host stand on its own port today (#52).
//!
//! **HTTP/2 comes with it.** ALPN offers `h2` and `http/1.1`, and the
//! connection is served by whichever the client chose; a plain connection
//! stays HTTP/1.1, because a browser will not speak h2 without TLS anyway
//! and h2c would be a second protocol for nobody.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

/// Where the host's certificate and key are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// The certificate chain, leaf first, PEM.
    pub cert: PathBuf,
    /// The private key, PEM (PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
}

/// What the listener accepts with, swapped as a whole when the files are
/// read again.
#[derive(Debug)]
pub struct Certificates {
    config: std::sync::RwLock<Arc<ServerConfig>>,
    pair: Pair,
}

impl Certificates {
    /// Reads the pair and builds what a connection is accepted with.
    ///
    /// The `Err` says which file and what about it, and never its
    /// contents: a key is a secret even when it does not parse.
    pub fn load(pair: Pair) -> Result<Certificates, String> {
        let config = read(&pair)?;
        Ok(Certificates {
            config: std::sync::RwLock::new(Arc::new(config)),
            pair,
        })
    }

    /// A listener with no certificate yet: every connection is refused
    /// until [`Certificates::reload`] reads one.
    ///
    /// This is what `--acme-domain` starts with on a host that has never
    /// had a certificate. Refusing a handshake is the honest answer — the
    /// host has nothing to prove who it is with — and the first order is
    /// usually seconds away.
    pub fn none(pair: Pair) -> Certificates {
        let empty = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(NoCertificate));
        Certificates {
            config: std::sync::RwLock::new(Arc::new(empty)),
            pair,
        }
    }

    /// Reads the files again and serves the new pair from the next
    /// connection on. The pair that is serving is kept if the new one does
    /// not read, because half a renewal is worse than an old certificate.
    pub fn reload(&self) -> Result<(), String> {
        let config = read(&self.pair)?;
        *self.config.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(config);
        Ok(())
    }

    /// What the next connection is accepted with.
    pub fn now(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.config.read().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn pair(&self) -> &Pair {
        &self.pair
    }
}

/// Reads one pair into a server configuration, with ALPN for h2 and
/// HTTP/1.1.
fn read(pair: &Pair) -> Result<ServerConfig, String> {
    let certs = certificates(&pair.cert)?;
    let key = private_key(&pair.key)?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|why| {
            format!(
                "`{}` and `{}` are not a certificate and its key: {why}",
                pair.cert.display(),
                pair.key.display()
            )
        })?;
    // The order is the host's preference: h2 where the client can.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// A resolver with nothing to offer, for a host whose certificate has not
/// been ordered yet.
#[derive(Debug)]
struct NoCertificate;

impl rustls::server::ResolvesServerCert for NoCertificate {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        None
    }
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let pem = std::fs::read(path)
        .map_err(|e| format!("cannot read the certificate `{}`: {e}", path.display()))?;
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut pem.as_slice()).collect();
    let certs = certs.map_err(|e| format!("`{}` is not a PEM certificate: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "`{}` holds no certificate; it is the chain, leaf first, in PEM",
            path.display()
        ));
    }
    Ok(certs)
}

fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let pem = std::fs::read(path)
        .map_err(|e| format!("cannot read the key `{}`: {e}", path.display()))?;
    // Nothing of the file is in any message from here on: a key that does
    // not parse is still a key.
    rustls_pemfile::private_key(&mut pem.as_slice())
        .map_err(|_| format!("`{}` is not a PEM private key", path.display()))?
        .ok_or_else(|| {
            format!(
                "`{}` holds no private key; PKCS#8, PKCS#1 or SEC1 in PEM",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate and key written by `rcgen`, as a deployment's would be
    /// by its renewal.
    fn written(label: &str) -> (Pair, tempdir::TempDir) {
        let dir = tempdir::TempDir::new(label);
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();
        (Pair { cert, key }, dir)
    }

    #[test]
    fn a_certificate_and_its_key_are_read_and_offer_h2_and_http_1_1() {
        let (pair, _dir) = written("read");
        let certificates = Certificates::load(pair).expect("the pair reads");
        let config = certificates.now();
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn a_reload_that_does_not_read_keeps_the_pair_that_is_serving() {
        let (pair, _dir) = written("reload");
        let certificates = Certificates::load(pair.clone()).expect("the pair reads");
        let before = Arc::as_ptr(&certificates.now());
        std::fs::write(&pair.cert, "not a certificate\n").unwrap();
        let why = certificates.reload().expect_err("it does not read");
        assert!(why.contains("holds no certificate"), "{why}");
        assert_eq!(
            Arc::as_ptr(&certificates.now()),
            before,
            "the old pair is still what a connection is accepted with"
        );
        // And a pair that reads again is taken.
        let (fresh, _fresh_dir) = written("reload-2");
        std::fs::copy(&fresh.cert, &pair.cert).unwrap();
        std::fs::copy(&fresh.key, &pair.key).unwrap();
        certificates.reload().expect("the new pair reads");
        assert_ne!(Arc::as_ptr(&certificates.now()), before);
    }

    #[test]
    fn a_file_that_is_not_there_says_which_one_and_no_key_is_quoted() {
        let (pair, _dir) = written("missing");
        let gone = Pair {
            cert: pair.cert.clone(),
            key: pair.key.with_extension("gone"),
        };
        let why = Certificates::load(gone).expect_err("there is no key");
        assert!(why.contains("cannot read the key"), "{why}");

        std::fs::write(&pair.key, "-----BEGIN PRIVATE KEY-----\nnot base64\n").unwrap();
        let why = Certificates::load(pair).expect_err("it does not parse");
        assert!(why.contains("private key"), "{why}");
        assert!(
            !why.contains("not base64"),
            "no part of it is quoted: {why}"
        );
    }

    /// A directory that removes itself, since the tests write files.
    mod tempdir {
        pub struct TempDir(std::path::PathBuf);

        impl TempDir {
            pub fn new(label: &str) -> TempDir {
                let dir = std::env::temp_dir()
                    .join(format!("minicloud-tls-{label}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                TempDir(dir)
            }

            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
