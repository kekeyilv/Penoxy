use quinn::{Connection, RecvStream, SendStream};
use serde_yml::Value;
use std::{
    collections::HashMap,
    error::Error,
    fmt::{Display, Formatter},
    fs::File,
    io::Read,
    net::SocketAddr,
    sync::Arc,
};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    select,
    sync::Mutex,
};
use tracing::{error, info, level_filters::LevelFilter, trace, warn};
use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{self, layer::SubscriberExt, util::SubscriberInitExt};
#[derive(Debug)]
pub struct UtilError {
    pub message: String,
}

impl Display for UtilError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for UtilError {}

pub fn init(
    config_path: &str,
    key_check: Vec<&str>,
    name: &str,
) -> Result<(HashMap<String, Value>, WorkerGuard), Box<dyn Error>> {
    let stdout_log = tracing_subscriber::fmt::layer().pretty();
    let appender = rolling::daily(format!("penoxy_logs_{}", name), "penoxy.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let file_log = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking);
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(file_log)
        .with(stdout_log)
        .init();
    let mut config_file = File::open(config_path)?;
    let mut buffer = String::new();
    let _ = config_file.read_to_string(&mut buffer);
    let config: HashMap<String, Value> = serde_yml::from_str(&buffer)?;
    for key in key_check {
        if !config.contains_key(key) {
            return Err(Box::new(UtilError {
                message: format!("The config file must contain key '{}'.", key).to_string(),
            }));
        }
    }
    Ok((config, guard))
}

const BUF_SIZE: usize = 1024 * 16;

pub struct StreamContext {
    recv_stream: RecvStream,
    send_stream: SendStream,
    buf: [u8; BUF_SIZE],
    offset: usize,
    buf_len: usize,
}

impl StreamContext {
    pub fn new(streams: (SendStream, RecvStream)) -> StreamContext {
        StreamContext {
            recv_stream: streams.1,
            send_stream: streams.0,
            buf: [0; BUF_SIZE],
            offset: 0,
            buf_len: 0,
        }
    }

    pub async fn read(self: &mut Self, mut size: usize) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        let mut res = vec![];
        while size > self.buf_len - self.offset {
            if self.buf_len > 0 {
                let slice = &self.buf[self.offset..self.buf_len];
                size -= self.buf_len - self.offset;
                res.extend_from_slice(slice);
            }
            self.buf_len = match self.recv_stream.read(&mut self.buf).await? {
                Some(n) => n,
                None => return Ok(None),
            };
            self.offset = 0;
        }
        let slice = &self.buf[self.offset..self.offset + size];
        self.offset += size;
        res.extend_from_slice(slice);
        Ok(Some(res))
    }

    pub async fn read_u64(self: &mut Self) -> Result<Option<u64>, Box<dyn Error>> {
        let mut buf: [u8; 8] = [0; 8];
        let data = match self.read(8).await? {
            Some(v) => v,
            None => return Ok(None),
        };
        buf.copy_from_slice(&data[0..8]);
        Ok(Some(u64::from_be_bytes(buf)))
    }

    pub async fn read_u16(self: &mut Self) -> Result<Option<u16>, Box<dyn Error>> {
        let mut buf: [u8; 2] = [0; 2];
        let data = match self.read(2).await? {
            Some(v) => v,
            None => return Ok(None),
        };
        buf.copy_from_slice(&data[0..2]);
        Ok(Some(u16::from_be_bytes(buf)))
    }

    pub async fn read_chunk(self: &mut Self) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        let len = self.read_u64().await?;
        Ok(self
            .read(match len {
                Some(n) => n.try_into()?,
                None => return Ok(None),
            })
            .await?)
    }

    pub async fn read_str(self: &mut Self) -> Result<Option<String>, Box<dyn Error>> {
        Ok(Some(String::from_utf8(match self.read_chunk().await? {
            Some(chunk) => chunk,
            None => return Ok(None),
        })?))
    }

    pub async fn send_u64(self: &mut Self, data: u64) -> Result<(), Box<dyn Error>> {
        self.send_stream.write_all(&data.to_be_bytes()).await?;
        Ok(())
    }

    pub async fn send_u16(self: &mut Self, data: u16) -> Result<(), Box<dyn Error>> {
        self.send_stream.write_all(&data.to_be_bytes()).await?;
        Ok(())
    }

    pub async fn send_chunk(self: &mut Self, data: &[u8]) -> Result<(), Box<dyn Error>> {
        self.send_u64(data.len().try_into()?).await?;
        self.send_stream.write_all(&data).await?;
        Ok(())
    }

    pub async fn send_str(self: &mut Self, data: &str) -> Result<(), Box<dyn Error>> {
        self.send_chunk(data.as_bytes()).await?;
        Ok(())
    }
}

pub struct StreamPair {
    stream_context: StreamContext,
    tcp_stream: TcpStream,
    addr: String,
}

impl StreamPair {
    pub fn new(stream_context: StreamContext, tcp_stream: TcpStream, addr: String) -> Self {
        StreamPair {
            stream_context,
            tcp_stream,
            addr,
        }
    }

    pub async fn run(self: &mut Self) -> Result<(), Box<dyn Error>> {
        loop {
            let context = &mut self.stream_context;
            let mut buf = [0; BUF_SIZE];
            let mut buflen = 0;
            select! {
                _ = self.tcp_stream.readable() => {
                    match self.tcp_stream.try_read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            trace!(
                                "Read data of {n} bytes from {remote}.",
                                n = n,
                                remote = self.addr.to_string()
                            );
                            buflen = n;
                        }
                        Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => {
                            continue;
                        }
                        Err(e) => return Err::<(), Box<dyn Error>>(Box::new(e)),
                    }
                }
                code = context.read_u16() => {
                    match code? {
                        Some(0) => { break; } // Code 0: Close
                        Some(1) => { }// Code 1: Data frame
                        _ => { break; }
                    }
                }
            };
            if buflen != 0 {
                context.send_u16(1).await?;
                context.send_chunk(&buf[0..buflen]).await?;
            } else {
                let chunk = match context.read_chunk().await? {
                    Some(chunk) => chunk,
                    None => break,
                };
                while buflen < chunk.len() {
                    self.tcp_stream.writable().await?;
                    match self.tcp_stream.try_write(&chunk[buflen..chunk.len()]) {
                        Ok(n) => buflen += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            continue;
                        }
                        Err(e) => {
                            return Err(Box::new(e));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}


pub struct Service {
    pub name: String,
    pub bind: String,
    pub host_name: String,
    pub remote: String,
    pub providers: Arc<Mutex<Vec<Connection>>>,
    counter: Mutex<usize>,
}

impl Service {
    pub fn new(service_config: &Value) -> Arc<Self> {
        Arc::new(Service {
            name: service_config["name"].as_str().unwrap().to_string(),
            bind: service_config["bind"].as_str().unwrap().to_string(),
            host_name: match service_config["host_name"].as_str() {
                Some(s) => s.to_string(),
                None => String::new(),
            },
            remote: service_config["remote"].as_str().unwrap().to_string(),
            providers: Arc::new(Mutex::new(vec![])),
            counter: Mutex::new(0),
        })
    }

    pub async fn run(self: Arc<Self>) -> Result<(), Box<dyn Error>> {
        let listener = TcpListener::bind(&self.bind).await?;
        info!(
            "Tcp service '{name}' started on {addr}.",
            name = self.name,
            addr = listener.local_addr()?.to_string()
        );
        while let Ok((stream, addr)) = listener.accept().await {
            if self.providers.lock().await.len() == 0 {
                warn!(
                    "Service '{name}' has no provider. Connection refused.",
                    name = self.name
                );
                continue;
            }
            let handler = self.clone().handle_connection(stream, addr);
            tokio::spawn(async move {
                if let Err(e) = handler.await {
                    error!("Failed: {err}", err = e.to_string())
                }
            });
        }
        Ok(())
    }

    pub async fn handle_connection(
        self: Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<(), Box<dyn Error>> {
        info!(
            "Tcp connection of service '{name}' from {remote} established.",
            name = self.name,
            remote = addr.to_string()
        );
        let mut context = {
            let mut providers = self.providers.lock().await;
            loop {
                let mut counter = self.counter.lock().await;
                let index = *counter % providers.len();
                *counter += 1;
                match providers[index].open_bi().await {
                    Ok(streams) => break Ok(StreamContext::new(streams)),
                    Err(e) => {
                        error!("Service provider error: {e}", e = e);
                        providers.remove(index);
                        *counter -= 1;
                    }
                }
                if providers.len() == 0 {
                    break Err(UtilError {
                        message: format!(
                            "Service '{}' has no provider. Connection refused.",
                            self.name
                        ),
                    });
                }
            }
        }?;
        context.send_str(&self.remote).await?;
        let mut pair = StreamPair::new(context, stream, addr.to_string());
        pair.run().await?;
        info!(
            "Tcp connection of service '{name}' from {remote} closed.",
            name = self.name,
            remote = addr.to_string()
        );
        Ok(())
    }
}

pub async fn provide_service(mut context: StreamContext, id: u64) -> Result<(), Box<dyn Error>> {
    let addr = match context.read_str().await? {
        Some(s) => s,
        None => {
            error!("Read failed (ID:{id}).");
            return Ok(());
        }
    };
    let stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Failed to establish tcp stream to {addr}: {err} (ID:{id}).",
                addr = addr,
                err = e.to_string()
            );
            context.send_u16(0).await?; // Code 0: Close the stream.
            return Err(Box::new(e));
        }
    };
    info!("Tcp stream to {addr} established (ID:{id}).", addr = addr);
    let mut pair = StreamPair::new(context, stream, addr.clone());
    pair.run().await?;
    info!("Tcp stream to {addr} closed (ID:{id}).", addr = addr);
    Ok(())
}