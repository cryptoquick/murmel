//
// Copyright 2018-2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Construct the Murmel stack
//!
//! Assembles modules of this library to a complete service
//!

use crate::chaindb::SharedChainDB;
use crate::dispatcher::Dispatcher;
use crate::dns::dns_seed;
use crate::downstream::{DownStreamDummy, SharedDownstream};
use crate::error::Error;
use crate::hammersbald::Hammersbald;
use crate::headerdownload::HeaderDownload;
use crate::p2p::BitcoinP2PConfig;
use crate::p2p::{P2PControl, PeerMessageSender, PeerSource, P2P};
use crate::ping::Ping;
use crate::timeout::Timeout;
use bitcoin::network::constants::{Network, ServiceFlags};
use bitcoin::network::message::NetworkMessage;
use bitcoin::network::message::RawNetworkMessage;
use futures::{
    executor::{ThreadPool, ThreadPoolBuilder},
    future,
    task::{Context, SpawnExt},
    Future, FutureExt, Poll as Async, StreamExt,
};
use futures_timer::Interval;
use rand::{thread_rng, RngCore};
use std::pin::Pin;
use std::time::Duration;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::Path,
    sync::{atomic::AtomicUsize, mpsc, Arc, Mutex, RwLock},
};

const MAX_PROTOCOL_VERSION: u32 = 70001;
const USER_AGENT: &'static str = concat!("/Murmel:", env!("CARGO_PKG_VERSION"), '/');

/// The complete stack
pub struct Constructor {
    p2p: Arc<P2P<NetworkMessage, RawNetworkMessage, BitcoinP2PConfig>>,
    /// this should be accessed by Lightning
    pub downstream: SharedDownstream,
}

impl Constructor {
    /// open DBs
    pub fn open_db(
        path: Option<&Path>,
        network: Network,
        _birth: u64,
    ) -> Result<SharedChainDB, Error> {
        let mut chaindb = if let Some(path) = path {
            #[cfg(feature = "default")]
            Hammersbald::new(path, network)?
        } else {
            #[cfg(feature = "default")]
            Hammersbald::mem(network)?
        };
        chaindb.init()?;
        Ok(Arc::new(RwLock::new(chaindb)))
    }

    /// Construct the stack
    pub fn new(
        network: Network,
        listen: Vec<SocketAddr>,
        chaindb: SharedChainDB,
    ) -> Result<Constructor, Error> {
        const BACK_PRESSURE: usize = 10;

        let (to_dispatcher, from_p2p) = mpsc::sync_channel(BACK_PRESSURE);

        let p2pconfig = BitcoinP2PConfig {
            network,
            nonce: thread_rng().next_u64(),
            max_protocol_version: MAX_PROTOCOL_VERSION,
            user_agent: USER_AGENT.to_owned(),
            height: AtomicUsize::new(0),
            server: !listen.is_empty(),
        };

        let (p2p, p2p_control) = P2P::new(
            p2pconfig,
            PeerMessageSender::new(to_dispatcher),
            BACK_PRESSURE,
        );

        let downstream = Arc::new(Mutex::new(DownStreamDummy {}));

        let timeout = Arc::new(Mutex::new(Timeout::new(p2p_control.clone())));

        let mut dispatcher = Dispatcher::new(from_p2p);

        dispatcher.add_listener(HeaderDownload::new(
            chaindb.clone(),
            p2p_control.clone(),
            timeout.clone(),
            downstream.clone(),
        ));
        dispatcher.add_listener(Ping::new(p2p_control.clone(), timeout.clone()));

        for addr in &listen {
            p2p_control.send(P2PControl::Bind(addr.clone()));
        }

        Ok(Constructor { p2p, downstream })
    }

    /// Run the stack. This should be called AFTER registering listener of the ChainWatchInterface,
    /// so they are called as the stack catches up with the blockchain
    /// * peers - connect to these peers at startup (might be empty)
    /// * min_connections - keep connections with at least this number of peers. Peers will be randomly chosen
    /// from those discovered in earlier runs
    pub fn run(
        &mut self,
        network: Network,
        peers: Vec<SocketAddr>,
        min_connections: usize,
    ) -> Result<(), Error> {
        let mut executor = ThreadPoolBuilder::new()
            .name_prefix("bitcoin-connect")
            .pool_size(2)
            .create()
            .expect("can not start futures thread pool");

        let p2p = self.p2p.clone();
        for addr in &peers {
            executor
                .spawn(
                    p2p.add_peer("bitcoin", PeerSource::Outgoing(addr.clone()))
                        .map(|_| ()),
                )
                .expect("can not spawn task for peers");
        }

        let keep_connected = KeepConnected {
            min_connections,
            p2p: self.p2p.clone(),
            earlier: HashSet::new(),
            dns: dns_seed(network),
            cex: executor.clone(),
        };
        executor
            .spawn(Interval::new(Duration::new(10, 0)).for_each(move |_| keep_connected.clone()))
            .expect("can not keep connected");

        let p2p = self.p2p.clone();
        let mut cex = executor.clone();
        executor.run(future::poll_fn(move |_| {
            let needed_services = ServiceFlags::NONE;
            p2p.poll_events("bitcoin", needed_services, &mut cex);
            Async::Ready(())
        }));
        Ok(())
    }
}

#[derive(Clone)]
struct KeepConnected {
    cex: ThreadPool,
    dns: Vec<SocketAddr>,
    earlier: HashSet<SocketAddr>,
    p2p: Arc<P2P<NetworkMessage, RawNetworkMessage, BitcoinP2PConfig>>,
    min_connections: usize,
}

impl Future for KeepConnected {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Async<Self::Output> {
        if self.p2p.n_connected_peers() < self.min_connections {
            let eligible = self
                .dns
                .iter()
                .cloned()
                .filter(|a| !self.earlier.contains(a))
                .collect::<Vec<_>>();
            if eligible.len() > 0 {
                let mut rng = thread_rng();
                let choice = eligible[(rng.next_u32() as usize) % eligible.len()];
                self.earlier.insert(choice.clone());
                let add = self
                    .p2p
                    .add_peer("bitcoin", PeerSource::Outgoing(choice))
                    .map(|_| ());
                self.cex
                    .spawn(add)
                    .expect("can not add peer for outgoing connection");
            }
        }
        Async::Ready(())
    }
}
