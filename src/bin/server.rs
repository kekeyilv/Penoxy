use penoxy::utils::{self, provide_service, Service, StreamContext};
use quinn::{
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    Endpoint, Incoming, ServerConfig,
};
use std::{error::Error, fs, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{self, select, sync::Mutex, time::sleep};
use tracing::{debug, error, info, warn};

#[tokio::main]
async fn main() {
    match run().await {
        Ok(_) => {}
        Err(e) => error!("Failed: {err}", err = e.to_string()),
    }
}

struct Host {
    host_name: String,
    passkey: String,
}

async fn run() -> Result<(), Box<dyn Error>> {
    let (config, _guard) = utils::init(
        "./penoxy.yml",
        vec!["cert", "key", "bind", "hosts"],
        "server",
    )?;
    let certs = vec![CertificateDer::from(fs::read(
        config["cert"].as_str().unwrap(),
    )?)];
    let key = PrivateKeyDer::try_from(fs::read(config["key"].as_str().unwrap())?)?;
    let addr = config["bind"].as_str().unwrap().parse::<SocketAddr>()?;
    let server_config = ServerConfig::with_single_cert(certs, key).unwrap();
    let endpoint = Endpoint::server(server_config, addr)?;
    let services = Arc::new(Mutex::new(vec![]));
    let hosts = Arc::new(Mutex::new(vec![]));
    info!("Server started on {addr}.", addr = addr.to_string());

    'hosts_iter: for host in config["hosts"].as_sequence().unwrap() {
        for key in ["passkey", "host_name"] {
            if host.as_mapping() == None || !host.as_mapping().unwrap().contains_key(key) {
                warn!(
                    "Invalid host: Field '{field}' must be included. The host will be ingnored.",
                    field = key
                );
                continue 'hosts_iter;
            }
        }
        hosts.lock().await.push(Arc::new(Host {
            host_name: host["host_name"].as_str().unwrap().to_string(),
            passkey: host["passkey"].as_str().unwrap().to_string(),
        }));
    }

    if config.contains_key("services") {
        'service_iter: for service_config in config["services"].as_sequence().unwrap() {
            for key in ["name", "bind", "remote", "host_name"] {
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

    let mut counter = 0;
    while let Some(conn) = endpoint.accept().await {
        let proc = connect(conn, services.clone(), hosts.clone(), counter);
        tokio::spawn(async move {
            if let Err(e) = proc.await {
                error!("Failed: {err}", err = e.to_string())
            }
        });
        counter += 1;
    }
    Ok(())
}

async fn connect(
    conn: Incoming,
    services: Arc<Mutex<Vec<Arc<Service>>>>,
    hosts: Arc<Mutex<Vec<Arc<Host>>>>,
    id: u64,
) -> Result<(), Box<dyn Error>> {
    let connection = conn.await?;
    info!(
        "Connection from {remote}.",
        remote = connection.remote_address()
    );
    let mut context = StreamContext::new(connection.accept_bi().await?);
    let host_name = match context.read_str().await? {
        Some(s) => s,
        None => {
            error!(
                "Read failed from {remote} (ID:{id}).",
                remote = connection.remote_address()
            );
            return Ok(());
        }
    };
    let passkey = match context.read_str().await? {
        Some(s) => s,
        None => {
            error!(
                "Read failed from {remote} (ID:{id}).",
                remote = connection.remote_address()
            );
            return Ok(());
        }
    };
    for host in &hosts.lock().await[..] {
        if host_name == host.host_name {
            if passkey != host.passkey {
                warn!(
                    "Incorrect passkey from {remote} (Hostname:{host}). The service provider will be ignore (ID:{id}).",
                    remote = connection.remote_address(),
                    host = host.host_name
                );
                return Ok(());
            }
        }
    }
    for service in &services.lock().await[..] {
        if host_name == service.host_name {
            service.providers.lock().await.push(connection.clone());
            debug!(
                "Service provider of '{service}' registered. Hostname {name} ({remote}) (ID:{id}).",
                service = service.name,
                name = host_name,
                remote = connection.remote_address()
            );
        }
    }
    loop {
        select! {
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
                match context.send_u16(0).await {
                    Ok(_) => {},
                    Err(e) => break Err(e),
                }
            }
            _ = sleep(Duration::from_millis(5000)) =>{
                match context.send_u16(0).await {
                    Ok(_) => {},
                    Err(e) => break Err(e),
                }
            }
        }
    }
}
