use penoxy::utils::{self, provide_service, Service, StreamContext};
use quinn::{
    crypto::rustls::QuicClientConfig,
    rustls::{pki_types::CertificateDer, CertificateError, SignatureScheme},
    ClientConfig, Endpoint,
};
use serde_yml::Value;
use std::{collections::HashMap, error::Error, fs, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{self, select, sync::Mutex, time::sleep};
use tracing::{error, info, warn};

#[derive(Debug)]
struct ServerVerification<'a> {
    certificate: CertificateDer<'a>,
}

impl<'a> ServerVerification<'a> {
    fn new(certificate: CertificateDer<'a>) -> Arc<ServerVerification<'a>> {
        Arc::new(Self { certificate })
    }
}

impl quinn::rustls::client::danger::ServerCertVerifier for ServerVerification<'_> {
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        cert: &CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        if self.certificate == *cert {
            Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
        } else {
            Err(quinn::rustls::Error::InvalidCertificate(
                CertificateError::BadSignature,
            ))
        }
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        cert: &CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        if self.certificate == *cert {
            Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
        } else {
            Err(quinn::rustls::Error::InvalidCertificate(
                CertificateError::BadSignature,
            ))
        }
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }

    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
        if self.certificate == *end_entity {
            Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(quinn::rustls::Error::InvalidCertificate(
                CertificateError::BadSignature,
            ))
        }
    }
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(_) => {}
        Err(e) => error!("Failed: {err}", err = e.to_string()),
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let (config, _guard) = utils::init(
        "./penoxy_client.yml",
        vec![
            "server",
            "server_cert",
            "passkey",
            "host_name",
        ],
        "client",
    )?;

    let client_config = quinn::rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(ServerVerification::new(CertificateDer::from(fs::read(
            config["server_cert"].as_str().unwrap(),
        )?)))
        .with_no_client_auth();

    let client_addr = "0.0.0.0:0".parse::<SocketAddr>()?;
    let mut endpoint = Endpoint::client(client_addr)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        client_config,
    )?)));
    let mut tasks = vec![];
    let services = Arc::new(Mutex::new(vec![]));

    if config.contains_key("services") {
        'service_iter: for service_config in config["services"].as_sequence().unwrap() {
            for key in ["name", "bind", "remote"] {
                if service_config.as_mapping() == None
                    || !service_config.as_mapping().unwrap().contains_key(key)
                {
                    warn!(
                        "Invalid service: Field '{field}' must be included. The service will be ingnored.",
                        field = key
                    );
                    continue 'service_iter;
                }
            }

            let service = Service::new(service_config);
            services.lock().await.push(service.clone());
            tokio::spawn(async move {
                if let Err(e) = service.run().await {
                    error!("Failed: {err}", err = e.to_string())
                }
            });
        }
    }

    for i in 0..config["max_connection"].as_u64().unwrap_or(1) {
        let config = config.clone();
        let endpoint = endpoint.clone();
        let services = services.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match new_connection(config.clone(), endpoint.clone(), services.clone(), i + 1)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => error!("Failed: {err}", err = e.to_string()),
                };
                warn!(
                    "Connection lost, try to reconnect in 5s (ID:{id}).",
                    id = i + 1
                );
                sleep(Duration::from_secs(5)).await;
            }
        }));
    }
    for task in tasks {
        task.await?;
    }
    Ok(())
}

async fn new_connection(
    config: HashMap<String, Value>,
    endpoint: Endpoint,
    services: Arc<Mutex<Vec<Arc<Service>>>>,
    id: u64,
) -> Result<(), Box<dyn Error>> {
    let server_addr = config["server"].as_str().unwrap().parse::<SocketAddr>()?;
    let connection = endpoint
        .connect(server_addr, "server")?
        .await?;
    let mut context = StreamContext::new(connection.open_bi().await?);
    info!(
        "Connected server on {addr} (ID:{id}).",
        addr = server_addr.to_string()
    );
    context
        .send_str(config["host_name"].as_str().unwrap())
        .await?;
    context
        .send_str(config["passkey"].as_str().unwrap())
        .await?;
    for service in &services.lock().await[..] {
        service.providers.lock().await.push(connection.clone());
    }
    loop {
        select! {
            stat = context.read_u16() => {
                match stat {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed: {err}", err = e.to_string());
                        break Err(e);
                    },
                }
            }
            streams = connection.accept_bi() => {
                let stream = StreamContext::new(match streams {
                    Ok(s) => s,
                    Err(e) => break Err(Box::new(e)),
                });
                let handler = provide_service(stream,id);
                tokio::spawn(async move {
                    match handler.await {
                        Ok(_) => {}
                        Err(e) => {error!("Failed: {err}", err = e.to_string())},
                    }
                });
            }
        }
    }
}
