use crate::{
    error::NotFoundError,
    link::Link,
    signal::Stop,
    state::{ProxyState, SharedProxyInfo, ToxicStateHolder},
    stream::{Read, Write},
    toxic::{update_toxic_list_in_place, StreamDirection, Toxic, ToxicEvent, ToxicEventResult},
};
use bmrng::{Payload, RequestReceiver};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{io, mem};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{BytesCodec, FramedRead, FramedWrite};
use tracing::{debug, error, info, instrument};

/// The default Go io.Copy buffer size is 32K, so also use 32K buffers here to imitate Toxiproxy.
const READ_BUFFER_SIZE: usize = 32768;

/// The immutable configuration for a proxy
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// An arbitrary name
    pub name: String,
    /// The host name and the port the proxy listens on, like 127.0.0.1:5431
    pub listen: String,
    /// The host name and the port the proxy connects to, like 127.0.0:5432
    pub upstream: String,
    /// The client can set the enabled field to false to stop this proxy.
    /// Proxies are enabled by default
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// A random seed. Not exposed in the API
    #[serde(skip)]
    pub rand_seed: Option<u64>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug)]
pub struct Links {
    upstream: Link,
    client: Link,
    /// Optional, connection-wide state for toxics that need such state (like LimitData)
    /// Toxic Name -> State
    state_holder: Option<Arc<ToxicStateHolder>>,
}

/// Toxics applied on a proxy connection
#[derive(Debug, Clone)]
pub struct Toxics {
    /// The toxics applied on the upstream link
    pub upstream: Vec<Toxic>,
    /// The toxics applied on the downstream link
    pub downstream: Vec<Toxic>,
}

impl ProxyConfig {
    /// Validate the proxy config, return `ProxyValidateError` if invalid
    pub fn validate(&self) -> Result<(), ProxyValidateError> {
        if self.name.is_empty() {
            Err(ProxyValidateError::MissingName)
        } else if self.upstream.is_empty() {
            Err(ProxyValidateError::MissingUpstream)
        } else if self.listen.is_empty() {
            Err(ProxyValidateError::MissingListen)
        } else {
            Ok(())
        }
    }
}

impl Toxics {
    /// Initialize an empty set up toxics
    pub fn noop() -> Self {
        Toxics {
            upstream: Vec::new(),
            downstream: Vec::new(),
        }
    }

    /// Consume this Toxics struct to combine upstream and downstream toxics in a flat unordered vec
    pub fn into_vec(mut self) -> Vec<Toxic> {
        self.upstream.append(&mut self.downstream);
        self.upstream
    }

    pub fn find_by_name(&self, toxic_name: &str) -> Option<Toxic> {
        self.upstream
            .iter()
            .find(|toxic| toxic.name == toxic_name)
            .or_else(|| {
                self.downstream
                    .iter()
                    .find(|toxic| toxic.name == toxic_name)
            })
            .map(|toxic| toxic.to_owned())
    }
}

/// TODO
pub async fn initialize_proxy(
    config: ProxyConfig,
    initial_toxics: Toxics,
) -> io::Result<(TcpListener, SharedProxyInfo)> {
    let listener = TcpListener::bind(&config.listen).await?;

    info!(name = ?config.name, proxy = ?config.listen, upstream = ?config.upstream, "Started proxy");

    let state = Arc::new(ProxyState::new(initial_toxics));

    let proxy_info = SharedProxyInfo {
        state,
        config: Arc::new(config),
    };

    Ok((listener, proxy_info))
}

/// TODO
#[instrument(level = "trace")]
pub async fn run_proxy(
    listener: TcpListener,
    proxy_info: SharedProxyInfo,
    receiver: RequestReceiver<ToxicEvent, ToxicEventResult>,
    mut stop: Stop,
) -> io::Result<()> {
    let state = proxy_info.state;
    let config = proxy_info.config;

    tokio::spawn(listen_toxic_events(
        state.clone(),
        receiver,
        stop.clone(),
        config.clone(),
    ));

    while !stop.stop_received() {
        let maybe_connection = tokio::select! {
            res = listener.accept() => Ok::<Option<(TcpStream, SocketAddr)>, io::Error>(Some(res?)),
            _ = stop.recv() => {
                Ok(None)
            },
        }?;

        if let Some((client_stream, addr)) = maybe_connection {
            // TODO: start a new span
            info!("Accepted client {}", addr);
            let upstream = match TcpStream::connect(&config.upstream).await {
                Ok(upstream) => upstream,
                Err(err) => {
                    // TODO: log details
                    error!("Unable to open connection to upstream {:?}", err);
                    // This is not a fatal error, can retry next time another client connects
                    continue;
                }
            };

            let (client_read, client_write) = client_stream.into_split();
            let (upstream_read, upstream_write) = upstream.into_split();

            let client_read =
                FramedRead::with_capacity(client_read, BytesCodec::new(), READ_BUFFER_SIZE);
            let client_write = FramedWrite::new(client_write, BytesCodec::new());
            let upstream_read =
                FramedRead::with_capacity(upstream_read, BytesCodec::new(), READ_BUFFER_SIZE);
            let upstream_write = FramedWrite::new(upstream_write, BytesCodec::new());

            let toxics = state.lock().toxics.clone();

            let res = create_links(
                state.clone(),
                addr,
                &config,
                &mut stop,
                toxics,
                client_read,
                client_write,
                upstream_read,
                upstream_write,
                None,
            );
            match res {
                Err(err) => {
                    // TODO: log arguments
                    error!("Unable to establish link for proxy {:?}", err);
                    continue;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn create_links(
    state: Arc<ProxyState>,
    addr: SocketAddr,
    config: &ProxyConfig,
    stop: &mut Stop,
    toxics: Toxics,
    client_read: Read,
    client_write: Write,
    upstream_read: Read,
    upstream_write: Write,
    previous_toxic_state_holder: Option<Arc<ToxicStateHolder>>,
) -> io::Result<()> {
    let mut current_state = state.lock();

    if current_state.clients.contains_key(&addr) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "State error: there is already a client connected with this address: {}",
                addr.to_string()
            ),
        ));
    }

    let (links_stop, links_stopper) = stop.fork();

    let toxics_state_holder =
        previous_toxic_state_holder.or_else(|| ToxicStateHolder::for_toxics(&toxics));

    let mut upstream_link = Link::new(
        addr,
        StreamDirection::Upstream,
        config.clone(),
        links_stop.clone(),
    );
    let mut client_link = Link::new(
        addr,
        StreamDirection::Downstream,
        config.clone(),
        links_stop.clone(),
    );

    let upstream_handle = upstream_link.establish(
        client_read,
        upstream_write,
        toxics.upstream,
        toxics_state_holder.clone(),
    );
    let downstream_handle = client_link.establish(
        upstream_read,
        client_write,
        toxics.downstream,
        toxics_state_holder.clone(),
    );

    let addr = addr.clone();
    let state = state.clone();
    tokio::spawn(async move {
        // No need to listen for the stop signal here, we're ending as soon as one of the tasks have stopped.
        let _ = tokio::select! {
            up = upstream_handle => up,
            down = downstream_handle => down
        };
        links_stopper.stop();
        let mut state = state.lock();
        state.clients.remove(&addr);
        debug!("Removed {}", addr);
    });

    current_state.clients.insert(
        addr,
        Links {
            upstream: upstream_link,
            client: client_link,
            state_holder: toxics_state_holder,
        },
    );
    Ok(())
}

async fn listen_toxic_events(
    state: Arc<ProxyState>,
    mut receiver: RequestReceiver<ToxicEvent, ToxicEventResult>,
    mut stop: Stop,
    config: Arc<ProxyConfig>,
) {
    while !stop.stop_received() {
        let maybe_payload: Option<Payload<ToxicEvent, ToxicEventResult>> = tokio::select! {
            res = receiver.recv() => {
                if let Ok(payload) = res {
                    Some(payload)
                } else {
                    None
                }
            },
            _ = stop.recv() => None,
        };
        if let Some(payload) = maybe_payload {
            process_toxic_event(state.clone(), config.clone(), stop.clone(), payload).await;
        } else {
            break;
        }
    }
}

async fn process_toxic_event(
    state: Arc<ProxyState>,
    config: Arc<ProxyConfig>,
    stop: Stop,
    (request, mut responder): Payload<ToxicEvent, ToxicEventResult>,
) {
    let new_toxics = {
        let mut current_state = state.lock();
        if let Err(err) = update_toxics(request, &mut current_state.toxics) {
            let _ = responder.respond(Err(err.into()));
            return;
        }
        current_state.toxics.clone()
    };

    let old_map = {
        let mut current_state = state.lock();
        mem::replace(&mut current_state.clients, HashMap::new())
    };

    let mut elements = stream::iter(old_map);
    while let Some((addr, links)) = elements.next().await {
        if let Err(err) = recreate_links(
            state.clone(),
            &config,
            stop.clone(),
            addr,
            links,
            new_toxics.clone(),
        )
        .await
        {
            error!(err = ?err, addr = ?addr, proxy = ?&config.name, "Failed to recreate links for client");
        }
    }
}

async fn recreate_links(
    state: Arc<ProxyState>,
    config: &ProxyConfig,
    stop: Stop,
    addr: SocketAddr,
    links: Links,
    new_toxics: Toxics,
) -> io::Result<()> {
    let (client_read, upstream_write) = links.client.disband().await?;
    let (upstream_read, client_write) = links.upstream.disband().await?;
    create_links(
        state.clone(),
        addr,
        config,
        &mut stop.clone(),
        new_toxics,
        client_read,
        client_write,
        upstream_read,
        upstream_write,
        links.state_holder,
    )
}

/// Update the toxics collection in place
fn update_toxics(event: ToxicEvent, toxics: &mut Toxics) -> Result<(), NotFoundError> {
    update_toxic_list_in_place(&mut toxics.upstream, event.kind)
        .or_else(|kind| update_toxic_list_in_place(&mut toxics.downstream, kind))
        .or(Err(NotFoundError))
}

#[derive(Debug, Clone, Error, PartialEq)]
pub enum ProxyValidateError {
    #[error("name missing")]
    MissingName,
    #[error("upstream missing")]
    MissingUpstream,
    #[error("listen address missing")]
    MissingListen,
}

#[cfg(test)]
mod serde_tests {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn test_ser_and_de() {
        let config = ProxyConfig {
            name: "foo".to_owned(),
            listen: "127.0.0.1:5431".to_owned(),
            upstream: "127.0.0.1:5432".to_owned(),
            enabled: false,
            rand_seed: Some(3),
        };
        let serialized = to_string(&config).unwrap();
        let expected = "{\"name\":\"foo\",\"listen\":\"127.0.0.1:5431\",\"upstream\":\"127.0.0.1:5432\",\"enabled\":false}";
        assert_eq!(expected, serialized);

        let expected = ProxyConfig {
            name: "foo".to_owned(),
            listen: "127.0.0.1:5431".to_owned(),
            upstream: "127.0.0.1:5432".to_owned(),
            enabled: false,
            rand_seed: None,
        };

        let deserialized = from_str(&serialized).unwrap();
        assert_eq!(expected, deserialized);
    }

    #[test]
    fn test_optional_enabled() {
        let expected = ProxyConfig {
            name: "foo".to_owned(),
            listen: "127.0.0.1:5431".to_owned(),
            upstream: "127.0.0.1:5432".to_owned(),
            enabled: true,
            rand_seed: None,
        };
        let input =
            "{\"name\":\"foo\",\"listen\":\"127.0.0.1:5431\",\"upstream\":\"127.0.0.1:5432\"}";
        let deserialized = from_str(&input).unwrap();
        assert_eq!(expected, deserialized);
    }
}
