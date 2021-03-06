//! UDP relay local server

use std::{
    io::{self, Cursor, Read},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use bytes::BytesMut;
use futures::{future, FutureExt};
use log::{debug, error, info, trace};
use lru_time_cache::{Entry, LruCache};
use tokio::{
    self,
    net::udp::{RecvHalf, SendHalf},
    sync::{mpsc, oneshot, Mutex},
    time,
};

use crate::{
    config::{ServerAddr, ServerConfig},
    context::{Context, SharedContext},
    relay::{
        loadbalancing::server::{PlainPingBalancer, ServerType, SharedPlainServerStatistic},
        socks5::Address,
        sys::create_udp_socket,
        utils::try_timeout,
    },
};

use super::{
    crypto_io::{decrypt_payload, encrypt_payload},
    tproxy_socket::TProxyUdpSocket,
    DEFAULT_TIMEOUT,
    MAXIMUM_UDP_PAYLOAD_SIZE,
};

fn cache_key(src: &SocketAddr, dst: &SocketAddr) -> String {
    format!("{}-{}", src, dst)
}

// Drop the oneshot::Sender<()> will trigger local <- remote task to finish
struct UdpAssociationWatcher(oneshot::Sender<()>);

// Represent a UDP association
#[derive(Clone)]
struct UdpAssociation {
    // local -> remote Queue
    // Drops tx, will close local -> remote task
    tx: mpsc::Sender<Vec<u8>>,

    // local <- remote task life watcher
    watcher: Arc<UdpAssociationWatcher>,
}

impl UdpAssociation {
    /// Create an association with addr
    async fn associate(
        server: SharedPlainServerStatistic,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        assoc_map: SharedAssocMap,
    ) -> io::Result<UdpAssociation> {
        // Create a socket for receiving packets
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
        let remote_udp = create_udp_socket(&local_addr).await?;

        let local_addr = remote_udp.local_addr().expect("Could not determine port bound to");
        debug!(
            "Created UDP Association for {} from {} -> {}",
            src_addr, local_addr, dst_addr
        );

        // Create a channel for sending packets to remote
        // FIXME: Channel size 1024?
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);

        // Create a watcher for local <- remote task
        let (watcher_tx, watcher_rx) = oneshot::channel::<()>();

        let close_flag = Arc::new(UdpAssociationWatcher(watcher_tx));

        // Splits socket into sender and receiver
        let (mut receiver, mut sender) = remote_udp.split();

        // Create a socket for sending packets back
        let mut local_udp = TProxyUdpSocket::bind(&dst_addr)?;

        let timeout = server.config().udp_timeout.unwrap_or(DEFAULT_TIMEOUT);

        {
            // local -> remote

            let server = server.clone();
            tokio::spawn(async move {
                let svr_cfg = server.server_config();
                let context = server.context();
                let dst_addr = Address::from(dst_addr);

                while let Some(pkt) = rx.recv().await {
                    // pkt is already a raw packet, so just send it
                    let res = UdpAssociation::relay_l2r(
                        context,
                        &src_addr,
                        &dst_addr,
                        &mut sender,
                        &pkt[..],
                        timeout,
                        svr_cfg,
                    )
                    .await;

                    if let Err(err) = res {
                        error!("failed to send packet {} -> {}, error: {}", src_addr, dst_addr, err);

                        // FIXME: Ignore? Or how to deal with it?
                    }
                }

                debug!("UDP REDIR {} -> {} finished", src_addr, dst_addr);
            });
        }

        // local <- remote
        tokio::spawn(async move {
            let svr_cfg = server.server_config();
            let context = server.context();

            let transfer_fut = async move {
                loop {
                    // Read and send back to source
                    let res =
                        UdpAssociation::relay_r2l(context, &src_addr, &mut receiver, &mut local_udp, svr_cfg).await;

                    if let Err(err) = res {
                        error!("failed to receive packet, {} <- {}, error: {}", src_addr, dst_addr, err);

                        // FIXME: Don't break, or if you can find a way to drop the UdpAssociation
                        // break;
                    }

                    let cache_key = cache_key(&src_addr, &dst_addr);
                    {
                        let mut amap = assoc_map.lock().await;

                        // Check or update expire time
                        let _ = amap.get(&cache_key);
                    }
                }
            };

            // Resolved only if watcher_rx resolved
            let _ = future::select(transfer_fut.boxed(), watcher_rx.boxed()).await;

            debug!("UDP REDIR {} <- {} finished", src_addr, dst_addr);
        });

        Ok(UdpAssociation {
            tx,
            watcher: close_flag,
        })
    }

    /// Relay packets from local to remote
    async fn relay_l2r(
        context: &Context,
        src: &SocketAddr,
        dst: &Address,
        remote_udp: &mut SendHalf,
        payload: &[u8],
        timeout: Duration,
        svr_cfg: &ServerConfig,
    ) -> io::Result<()> {
        debug!("UDP REDIR {} -> {}, payload length {} bytes", src, dst, payload.len());

        // CLIENT -> SERVER protocol: ADDRESS + PAYLOAD
        let mut send_buf = Vec::new();
        dst.write_to_buf(&mut send_buf);
        send_buf.extend_from_slice(payload);

        let mut encrypt_buf = BytesMut::new();
        encrypt_payload(context, svr_cfg.method(), svr_cfg.key(), &send_buf, &mut encrypt_buf)?;

        let send_len = match svr_cfg.addr() {
            ServerAddr::SocketAddr(ref remote_addr) => {
                try_timeout(remote_udp.send_to(&encrypt_buf[..], remote_addr), Some(timeout)).await?
            }
            ServerAddr::DomainName(ref dname, port) => lookup_then!(context, dname, *port, false, |addr| {
                try_timeout(remote_udp.send_to(&encrypt_buf[..], &addr), Some(timeout)).await
            })
            .map(|(_, l)| l)?,
        };

        assert_eq!(encrypt_buf.len(), send_len);

        Ok(())
    }

    /// Relay packets from remote to local
    async fn relay_r2l(
        context: &Context,
        src_addr: &SocketAddr,
        remote_udp: &mut RecvHalf,
        local_udp: &mut TProxyUdpSocket,
        svr_cfg: &ServerConfig,
    ) -> io::Result<()> {
        // Waiting for response from server SERVER -> CLIENT
        // Packet length is limited by MAXIMUM_UDP_PAYLOAD_SIZE, excess bytes will be discarded.
        let mut recv_buf = [0u8; MAXIMUM_UDP_PAYLOAD_SIZE];

        let (recv_n, remote_addr) = remote_udp.recv_from(&mut recv_buf).await?;

        let decrypt_buf = match decrypt_payload(context, svr_cfg.method(), svr_cfg.key(), &recv_buf[..recv_n])? {
            None => {
                error!("UDP packet too short, received length {}", recv_n);
                let err = io::Error::new(io::ErrorKind::InvalidData, "packet too short");
                return Err(err);
            }
            Some(b) => b,
        };
        // SERVER -> CLIENT protocol: ADDRESS + PAYLOAD
        let mut cur = Cursor::new(decrypt_buf);
        // FIXME: Address is ignored. Maybe useful in the future if we uses one common UdpSocket for communicate with remote server
        let _ = Address::read_from(&mut cur).await?;

        let mut payload = Vec::new();
        cur.read_to_end(&mut payload)?;

        debug!(
            "UDP REDIR {} <- {}, payload length {} bytes",
            src_addr,
            remote_addr,
            payload.len()
        );

        // Send back to src_addr
        local_udp.send_to(&payload, src_addr).await.map(|_| ())
    }

    // Send packet to remote
    //
    // Return `Err` if receiver have been closed
    async fn send(&mut self, pkt: Vec<u8>) {
        if let Err(..) = self.tx.send(pkt).await {
            // SHOULDn't HAPPEN
            unreachable!("UDP Association local -> remote Queue closed unexpectly");
        }
    }
}

type AssocMap = LruCache<String, UdpAssociation>;
type SharedAssocMap = Arc<Mutex<AssocMap>>;

/// Starts a UDP local server
pub async fn run(context: SharedContext) -> io::Result<()> {
    if let Err(err) = super::sys::check_support_tproxy() {
        panic!("{}", err);
    }

    let local_addr = context.config().local.as_ref().expect("Missing local config");
    let bind_addr = local_addr.bind_addr(&*context).await?;

    // let l = create_socket(&bind_addr).await?;
    let mut l = TProxyUdpSocket::bind(&bind_addr)?;
    let local_addr = l.local_addr().expect("Could not determine port bound to");

    let balancer = PlainPingBalancer::new(context.clone(), ServerType::Udp).await;

    info!("ShadowSocks UDP Redir listening on {}", local_addr);

    // NOTE: Associations are only eliminated by expire time
    // So it may exhaust all available file descriptors
    let timeout = context.config().udp_timeout.unwrap_or(DEFAULT_TIMEOUT);
    let assoc_map: SharedAssocMap = Arc::new(Mutex::new(LruCache::with_expiry_duration(timeout)));

    let mut pkt_buf = [0u8; MAXIMUM_UDP_PAYLOAD_SIZE];

    loop {
        let (recv_len, src, dst) = match time::timeout(timeout, l.recv_from(&mut pkt_buf)).await {
            Ok(r) => r?,
            Err(..) => {
                // Cleanup expired association
                // Do not consume this iterator, it will updates expire time of items that traversed
                let mut assoc_map = assoc_map.lock().await;
                let _ = assoc_map.iter();
                continue;
            }
        };

        // Packet length is limited by MAXIMUM_UDP_PAYLOAD_SIZE, excess bytes will be discarded.
        // Copy bytes, because udp_associate runs in another tokio Task
        let pkt = &pkt_buf[..recv_len];

        trace!(
            "received UDP packet from {}, destination {}, length {} bytes",
            src,
            dst,
            recv_len
        );

        if recv_len == 0 {
            // For windows, it will generate a ICMP Port Unreachable Message
            // https://docs.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-recvfrom
            // Which will result in recv_from return 0.
            //
            // It cannot be solved here, because `WSAGetLastError` is already set.
            //
            // See `relay::udprelay::utils::create_socket` for more detail.
            continue;
        }

        // Check or (re)create an association
        let mut assoc = {
            // Locks the whole association map
            let mut ref_assoc_map = assoc_map.lock().await;

            // Get or create an association
            let assoc = match ref_assoc_map.entry(cache_key(&src, &dst)) {
                Entry::Occupied(oc) => oc.into_mut(),
                Entry::Vacant(vc) => {
                    // Pick a server
                    let server = balancer.pick_server();

                    vc.insert(
                        UdpAssociation::associate(server, src, dst, assoc_map.clone())
                            .await
                            .expect("Failed to create udp association"),
                    )
                }
            };

            // Clone the handle and release the lock.
            // Make sure we keep the critical section small
            assoc.clone()
        };

        // Send to local -> remote task
        assoc.send(pkt.to_vec()).await;
    }
}
