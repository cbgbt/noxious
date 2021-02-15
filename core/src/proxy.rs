use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpSocket, TcpStream},
};
use tokio_stream::wrappers::SplitStream;
use tokio_stream::StreamExt;

use tokio_util::codec::{BytesCodec, FramedRead, FramedWrite};

use crate::link::Link;
use crate::signal::Stop;
use crate::toxic::Toxic;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProxyConfig {
    /// An arbitrary name
    pub name: String,
    /// The host name and the port the proxy listens on, like 127.0.0.1:5431
    pub listen: String,
    /// The host name and the port the proxy connects to, like 127.0.0:5432
    pub upstream: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToxicEvent {
    ToxicAdd { addr: SocketAddr, toxic: Toxic },
    ToxicUpdate { addr: SocketAddr, toxic: Toxic },
    ToxicRemove { addr: SocketAddr, toxic_name: String },
}

#[derive(Debug)]
pub struct Links {
    upstream: Link,
    client: Link,
}

pub struct ProxyState {
    // Socket address --> (Upstream, Downstream)
    clients: HashMap<SocketAddr, Links>,
}

pub(crate) async fn run_proxy(
    config: ProxyConfig,
    receiver: mpsc::Receiver<ToxicEvent>,
    mut stop: Stop,
) -> io::Result<()> {
    let listener = TcpListener::bind(&config.listen).await?;

    let state = Arc::new(Mutex::new(ProxyState::new()));

    tokio::spawn(listen_toxic_events(state.clone(), receiver, stop.clone()));

    while !stop.is_shutdown() {
        let maybe_connection = tokio::select! {
            res = listener.accept() => Ok::<Option<(TcpStream, SocketAddr)>, io::Error>(Some(res?)),
            _ = stop.recv() => {
                Ok(None)
            },
        }?;

        if let Some((client_stream, addr)) = maybe_connection {
            // TODO: wrap this error? (could not connect to upstream)
            let upstream = TcpStream::connect(&config.upstream).await?;

            let (client_read, client_write) = client_stream.into_split();
            let (upstream_read, upstream_write) = upstream.into_split();

            let client_read = FramedRead::new(client_read, BytesCodec::new());
            let client_write = FramedWrite::new(client_write, BytesCodec::new());
            let upstream_read = FramedRead::new(upstream_read, BytesCodec::new());
            let upstream_write = FramedWrite::new(upstream_write, BytesCodec::new());

            let res = create_links(
                state.clone(),
                addr,
                &config,
                &mut stop,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
            );
            match res {
                Err(err) => {
                    // TODO: trace
                    println!("{}", err);
                    continue;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn create_links(
    state: Arc<Mutex<ProxyState>>,
    addr: SocketAddr,
    config: &ProxyConfig,
    stop: &mut Stop,
    client_read: FramedRead<OwnedReadHalf, BytesCodec>,
    client_write: FramedWrite<OwnedWriteHalf, BytesCodec>,
    upstream_read: FramedRead<OwnedReadHalf, BytesCodec>,
    upstream_write: FramedWrite<OwnedWriteHalf, BytesCodec>,
) -> io::Result<()> {
    let config = config.clone();
    let config_clone = config.clone();

    let stop_upstream = stop.clone();
    let stop_client = stop.clone();

    // TODO: when there is an update in the list of toxics, drop the current link and
    // start a new one?

    let mut state = state.lock().expect(&format!(
        "ProxyState poisoned for upstream {}",
        addr.to_string()
    ));

    if state.clients.contains_key(&addr) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "State error: there is already a client connected with this address, {}",
                addr.to_string()
            ),
        ));
    }

    let mut upstream_link = Link::new(client_read, upstream_write, addr, config);
    let mut client_link = Link::new(upstream_read, client_write, addr, config_clone);

    upstream_link.establish(stop_upstream);
    client_link.establish(stop_client);

    state.clients.insert(
        addr,
        Links {
            upstream: upstream_link,
            client: client_link,
        },
    );
    Ok(())
}

impl ProxyState {
    fn new() -> Self {
        ProxyState {
            clients: HashMap::new(),
        }
    }
}

async fn listen_toxic_events(
    state: Arc<Mutex<ProxyState>>,
    mut receiver: mpsc::Receiver<ToxicEvent>,
    mut stop: Stop,
) {
    while !stop.is_shutdown() {
        let maybe_event = tokio::select! {
            res = receiver.recv() => Some(res),
            _ = stop.recv() => None,
        };

        if let Some(event) = maybe_event {
            let mut state = state.lock().expect("ProxyState poisoned for upstream {}");
            // let links = state
            // Rebuild the links

        }
    }
}
