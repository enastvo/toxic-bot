// Regression test for the rustls 0.23 CryptoProvider panic.
//
// In production the dashboard's TLS task panicked at startup with
// "Could not automatically determine the process-level CryptoProvider" because
// both aws-lc-rs (via axum-server) and ring (via rcgen) are in the dependency
// tree, so rustls cannot auto-select one. `web::load_tls_config` must install a
// provider before building the config; without that install this test panics.
use signal_bot::web::load_tls_config;
use std::io::Write;

#[tokio::test]
async fn tls_config_builds_with_crypto_provider() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::File::create(&cert_path)
        .unwrap()
        .write_all(cert.cert.pem().as_bytes())
        .unwrap();
    std::fs::File::create(&key_path)
        .unwrap()
        .write_all(cert.key_pair.serialize_pem().as_bytes())
        .unwrap();

    let res = load_tls_config(&cert_path, &key_path).await;
    assert!(res.is_ok(), "TLS config should build: {res:?}");
}
