//! Real HTTPS fixture: stock transport and manifest registration, no fake body client.
use orbisync_extensions::{DeliveryError, DnsResolver, SecretProvider};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Service {
    pub task: tokio::task::JoinHandle<()>,
    pub calls: Arc<AtomicUsize>,
    address: SocketAddr,
    ca: Vec<u8>,
}

struct Resolver(SocketAddr);
#[async_trait::async_trait]
impl DnsResolver for Resolver {
    async fn resolve(&self, _: &str) -> Result<Vec<SocketAddr>, std::io::Error> {
        Ok(vec![self.0])
    }
}
struct Secret;
#[async_trait::async_trait]
impl SecretProvider for Secret {
    async fn resolve(&self, reference: &str) -> Result<Vec<u8>, DeliveryError> {
        assert_eq!(reference, "TEST_RULE_SECRET");
        Ok(b"fixture-only-rule-secret".to_vec())
    }
}

impl Service {
    pub async fn register(
        &self,
        world: orbisync_domain::WorldId,
    ) -> orbisync_server::input::InputRules {
        use orbisync_server::external_input::{ExternalInputTransport, InputRuleManifest};
        let client = orbisync_extensions::ReqwestPreCommitClient::try_new_with_additional_ca(
            Arc::new(Resolver(self.address)),
            true,
            &self.ca,
        )
        .unwrap();
        let transport = Arc::new(ExternalInputTransport::new(
            Arc::new(client),
            Arc::new(Secret),
            orbisync_extensions::PreCommitValidationPolicy::from_config(
                orbisync_config::Config::default().extensions,
            )
            .unwrap(),
        ));
        let path =
            std::env::temp_dir().join(format!("orbisync-input-{}.json", uuid::Uuid::now_v7()));
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1, "rules": [{"world_id": world.to_string(), "rule": "example.move",
                    "component_key": "example.position", "endpoint": "https://rules.test/compute",
                    "signing_secret_ref": "TEST_RULE_SECRET"}]
            })
            .to_string(),
        )
        .unwrap();
        let manifest = InputRuleManifest::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        manifest.register(transport).await.unwrap()
    }

    pub async fn start() -> Self {
        // Another concurrent test may have already installed this provider.
        drop(rustls::crypto::ring::default_provider().install_default());
        let cert = rcgen::generate_simple_self_signed(vec!["rules.test".into()]).unwrap();
        let ca = cert.cert.pem().into_bytes();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    cert.cert.der().to_vec(),
                )],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
                ),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4096];
                let (header_end, length) = loop {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body = &bytes[header_end..header_end + length];
                let wire: serde_json::Value = serde_json::from_slice(body).unwrap();
                // Verify the real raw-body signature, then inspect authenticated context.
                let expected = orbisync_extensions::build_signed_webhook(
                    "https://rules.test/compute",
                    wire["event_id"].as_str().unwrap().parse().unwrap(),
                    "input.compute",
                    wire["timestamp"].as_i64().unwrap(),
                    &wire["payload"],
                    b"fixture-only-rule-secret",
                )
                .unwrap();
                assert_eq!(expected.body, body);
                let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
                for (name, value) in expected.headers {
                    assert!(headers.contains(&format!("{}: {}", name.to_lowercase(), value)));
                }
                let p = &wire["payload"];
                assert_eq!(p["version"], 1);
                assert_eq!(p["requester"], p["current_entity"]["owner_id"]);
                assert_eq!(p["component_key"], "example.position");
                let dx = p["intent"]["dx"].as_f64().unwrap();
                let x = p["current_entity"]["components"]["example.position"]["value"]["x"]
                    .as_f64()
                    .unwrap_or(0.0);
                count.fetch_add(1, Ordering::SeqCst);
                let response = if dx == 3.0 {
                    "malformed".to_owned()
                } else if dx == 4.0 {
                    serde_json::json!({"version": 1, "request_id": "wrong", "decision": "accept", "update": {"x": 999}}).to_string()
                } else if dx.abs() > 1.0 {
                    serde_json::json!({"version": 1, "request_id": p["request_id"], "decision": "reject", "reason": "step out of range"}).to_string()
                } else {
                    serde_json::json!({"version": 1, "request_id": p["request_id"], "decision": "accept", "update": {"x": x + dx}}).to_string()
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response.len(),
                            response
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        Self {
            task,
            calls,
            address,
            ca,
        }
    }
}
