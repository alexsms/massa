use std::cmp::Reverse;
use std::net::IpAddr;
use std::{collections::HashMap, net::SocketAddr, thread::JoinHandle, time::Duration};

use crossbeam::channel::tick;
use crossbeam::{
    channel::{Receiver, Sender},
    select,
};
use massa_models::version::{VersionDeserializer, VersionSerializer};
use massa_protocol_exports::{BootstrapPeers, ProtocolConfig};
use massa_serialization::{DeserializeError, Deserializer, Serializer};
use peernet::types::PUBLIC_KEY_SIZE_BYTES;
use rand::{rngs::StdRng, RngCore, SeedableRng};

use peernet::messages::MessagesSerializer;
use peernet::{
    error::{PeerNetError, PeerNetResult},
    messages::MessagesHandler as PeerNetMessagesHandler,
    peer::InitConnectionHandler,
    peer_id::PeerId,
    transports::{endpoint::Endpoint, TransportType},
    types::Hash,
    types::{KeyPair, Signature},
};
use tracing::log::{debug, error, info, warn};

use crate::handlers::peer_handler::models::PeerState;
use crate::messages::MessagesHandler;
use crate::wrap_network::ActiveConnectionsTrait;

use self::models::PeerInfo;
use self::{
    models::{
        InitialPeers, PeerManagementChannel, PeerManagementCmd, PeerMessageTuple, SharedPeerDB,
    },
    tester::Tester,
};

use self::{
    announcement::{
        Announcement, AnnouncementDeserializer, AnnouncementDeserializerArgs,
        AnnouncementSerializer,
    },
    messages::{PeerManagementMessageDeserializer, PeerManagementMessageDeserializerArgs},
};

/// This file contains the definition of the peer management handler
/// This handler is here to check that announcements we receive are valid and
/// that all the endpoints we received are active.
mod announcement;
mod messages;
pub mod models;
mod tester;

pub(crate) use messages::{PeerManagementMessage, PeerManagementMessageSerializer};

pub struct PeerManagementHandler {
    pub peer_db: SharedPeerDB,
    pub thread_join: Option<JoinHandle<()>>,
    pub sender: PeerManagementChannel,
    testers: Vec<Tester>,
}

impl PeerManagementHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_peers: InitialPeers,
        peer_id: PeerId,
        peer_db: SharedPeerDB,
        (sender_msg, receiver_msg): (Sender<PeerMessageTuple>, Receiver<PeerMessageTuple>),
        (sender_cmd, receiver_cmd): (Sender<PeerManagementCmd>, Receiver<PeerManagementCmd>),
        messages_handler: MessagesHandler,
        mut active_connections: Box<dyn ActiveConnectionsTrait>,
        target_out_connections: HashMap<String, (Vec<IpAddr>, usize)>,
        default_target_out_connections: usize,
        config: &ProtocolConfig,
    ) -> Self {
        let message_serializer = PeerManagementMessageSerializer::new();

        let ((test_sender, test_receiver), testers) = Tester::run(
            config,
            active_connections.clone(),
            peer_db.clone(),
            messages_handler,
            target_out_connections,
            default_target_out_connections,
        );

        let thread_join = std::thread::Builder::new()
        .name("protocol-peer-handler".to_string())
        .spawn({
            let peer_db = peer_db.clone();
            let ticker = tick(Duration::from_secs(10));
            let config = config.clone();
            let message_serializer = crate::messages::MessagesSerializer::new()
                .with_peer_management_message_serializer(PeerManagementMessageSerializer::new());
            let mut message_deserializer =
                PeerManagementMessageDeserializer::new(PeerManagementMessageDeserializerArgs {
                    max_peers_per_announcement: config.max_size_peers_announcement,
                    max_listeners_per_peer: config.max_size_listeners_per_peer,
                });
            move || {
                loop {
                    select! {
                        recv(ticker) -> _ => {
                            let peers_to_send = peer_db.read().get_rand_peers_to_send(100);
                            if peers_to_send.is_empty() {
                                continue;
                            }

                            let msg = PeerManagementMessage::ListPeers(peers_to_send);

                            for peer_id in &active_connections.get_peer_ids_connected() {
                                if let Err(e) = active_connections
                                    .send_to_peer(peer_id, &message_serializer, msg.clone().into(), false) {
                                    error!("error sending ListPeers message to peer: {:?}", e);
                               }
                            }
                        }
                        recv(receiver_cmd) -> cmd => {
                            // internal command
                           match cmd {
                             Ok(PeerManagementCmd::Ban(peer_ids)) => {
                                // remove running handshake ?
                                for peer_id in peer_ids {
                                    active_connections.shutdown_connection(&peer_id);

                                    // update peer_db
                                    peer_db.write().ban_peer(&peer_id);
                                }
                            },
                             Ok(PeerManagementCmd::Unban(peer_ids)) => {
                                for peer_id in peer_ids {
                                    peer_db.write().unban_peer(&peer_id);
                                }
                            },
                             Ok(PeerManagementCmd::GetBootstrapPeers { responder }) => {
                                let mut peers = peer_db.read().get_rand_peers_to_send(100);
                                // Add myself
                                if let Some(routable_ip) = config.routable_ip {
                                    let listeners = config.listeners.iter().map(|(addr, ty)| {
                                        (SocketAddr::new(routable_ip, addr.port()), *ty)
                                    }).collect();
                                    peers.push((peer_id.clone(), listeners));
                                }
                                if let Err(err) = responder.send(BootstrapPeers(peers)) {
                                    warn!("error sending bootstrap peers: {:?}", err);
                                }
                             },
                             Ok(PeerManagementCmd::Stop) => {
                                while let Ok(_msg) = test_receiver.try_recv() {
                                    // nothing to do just clean the channel
                                }
                                return;
                             },
                            Err(e) => {
                                error!("error receiving command: {:?}", e);
                            }
                           }
                        },
                        recv(receiver_msg) -> msg => {
                            let (peer_id, message_id, message) = match msg {
                                Ok((peer_id, message_id, message)) => (peer_id, message_id, message),
                                Err(_) => {
                                    return;
                                }
                            };
                            // check if peer is banned
                            if let Some(peer) = peer_db.read().peers.get(&peer_id) {
                                if peer.state == PeerState::Banned {
                                    warn!("Banned peer sent us a message: {:?}", peer_id);
                                    continue;
                                }
                            }
                            message_deserializer.set_message(message_id);
                            let (rest, message) = match message_deserializer
                                .deserialize::<DeserializeError>(&message) {
                                Ok((rest, message)) => (rest, message),
                                Err(e) => {
                                    warn!("error when deserializing message: {:?}", e);
                                    continue;
                                }
                            };
                            if !rest.is_empty() {
                                warn!("message not fully deserialized");
                                continue;
                            }
                            match message {
                                PeerManagementMessage::NewPeerConnected((peer_id, listeners)) => {
                                    debug!("Received peer message: NewPeerConnected from {}", peer_id);
                                    if let Err(e) = test_sender.try_send((peer_id, listeners)) {
                                        debug!("error when sending msg to peer tester : {}", e);
                                    }
                                }
                                PeerManagementMessage::ListPeers(peers) => {
                                    debug!("Received peer message: List peers from {}", peer_id);
                                    for (peer_id, listeners) in peers.into_iter() {
                                        if let Err(e) = test_sender.try_send((peer_id, listeners)) {
                                            debug!("error when sending msg to peer tester : {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }).expect("OS failed to start peer management thread");

        for (peer_id, listeners) in &initial_peers {
            let mut message = Vec::new();
            message_serializer
                .serialize(
                    &PeerManagementMessage::NewPeerConnected((peer_id.clone(), listeners.clone())),
                    &mut message,
                )
                .unwrap();
            sender_msg.send((peer_id.clone(), 0, message)).unwrap();
        }

        Self {
            peer_db,
            thread_join: Some(thread_join),
            sender: PeerManagementChannel {
                msg_sender: sender_msg,
                command_sender: sender_cmd,
            },
            testers,
        }
    }

    pub fn stop(&mut self) {
        self.sender
            .command_sender
            .send(PeerManagementCmd::Stop)
            .unwrap();

        // waiting for all threads to finish
        self.testers.iter_mut().for_each(|tester| {
            if let Some(join_handle) = tester.handler.take() {
                join_handle.join().expect("Failed to join tester thread");
            }
        });
    }
}

#[derive(Clone)]
pub struct MassaHandshake {
    pub announcement_serializer: AnnouncementSerializer,
    pub announcement_deserializer: AnnouncementDeserializer,
    pub version_serializer: VersionSerializer,
    pub version_deserializer: VersionDeserializer,
    pub config: ProtocolConfig,
    pub peer_db: SharedPeerDB,
    peer_mngt_msg_serializer: crate::messages::MessagesSerializer,
    message_handlers: MessagesHandler,
}

impl MassaHandshake {
    pub fn new(
        peer_db: SharedPeerDB,
        config: ProtocolConfig,
        message_handlers: MessagesHandler,
    ) -> Self {
        Self {
            peer_db,
            announcement_serializer: AnnouncementSerializer::new(),
            announcement_deserializer: AnnouncementDeserializer::new(
                AnnouncementDeserializerArgs {
                    max_listeners: config.max_size_listeners_per_peer,
                },
            ),
            version_serializer: VersionSerializer::new(),
            version_deserializer: VersionDeserializer::new(),
            config,
            peer_mngt_msg_serializer: crate::messages::MessagesSerializer::new()
                .with_peer_management_message_serializer(PeerManagementMessageSerializer::new()),
            message_handlers,
        }
    }
}

impl InitConnectionHandler for MassaHandshake {
    fn perform_handshake<MassaMessagesHandler: PeerNetMessagesHandler>(
        &mut self,
        keypair: &KeyPair,
        endpoint: &mut Endpoint,
        listeners: &HashMap<SocketAddr, TransportType>,
        messages_handler: MassaMessagesHandler,
    ) -> PeerNetResult<PeerId> {
        let mut bytes = PeerId::from_public_key(keypair.get_public_key()).to_bytes();
        self.version_serializer
            .serialize(&self.config.version, &mut bytes)
            .map_err(|err| {
                PeerNetError::HandshakeError.error(
                    "Massa Handshake",
                    Some(format!("Failed to serialize version: {}", err)),
                )
            })?;
        bytes.push(0);
        let listeners_announcement =
            Announcement::new(listeners.clone(), self.config.routable_ip, keypair).unwrap();
        self.announcement_serializer
            .serialize(&listeners_announcement, &mut bytes)
            .map_err(|err| {
                PeerNetError::HandshakeError.error(
                    "Massa Handshake",
                    Some(format!("Failed to serialize announcement: {}", err)),
                )
            })?;
        endpoint.send(&bytes)?;
        let received = endpoint.receive()?;
        if received.len() < 32 {
            return Err(PeerNetError::HandshakeError.error(
                "Massa Handshake",
                Some(format!("Received too short message len:{}", received.len())),
            ));
        }
        let mut offset = 0;
        let peer_id =
            PeerId::from_bytes(&received[offset..offset + 32].try_into().map_err(|_| {
                PeerNetError::HandshakeError.error(
                    "Massa Handshake",
                    Some("Failed to deserialize PeerId".to_string()),
                )
            })?)?;
        offset += PUBLIC_KEY_SIZE_BYTES;
        {
            let peer_db_read = self.peer_db.read();
            if let Some(info) = peer_db_read.peers.get(&peer_id) {
                if info.state == PeerState::Banned {
                    debug!("Banned peer tried to connect: {:?}", peer_id);
                }
            }
        }

        let res = {
            {
                let mut peer_db_write = self.peer_db.write();
                peer_db_write
                    .peers
                    .entry(peer_id.clone())
                    .and_modify(|info| {
                        info.state = PeerState::InHandshake;
                    });
            }

            let (received, version) = self
                .version_deserializer
                .deserialize::<DeserializeError>(&received[offset..])
                .map_err(|err| {
                    PeerNetError::HandshakeError.error(
                        "Massa Handshake",
                        Some(format!("Failed to deserialize version: {}", err)),
                    )
                })?;
            if !self.config.version.is_compatible(&version) {
                return Err(PeerNetError::HandshakeError.error(
                    "Massa Handshake",
                    Some(format!("Received version incompatible: {}", version)),
                ));
            }
            offset = 0;
            let id = received.get(offset).ok_or(
                PeerNetError::HandshakeError
                    .error("Massa Handshake", Some("Failed to get id".to_string())),
            )?;
            offset += 1;
            match id {
                0 => {
                    let (_, announcement) = self
                        .announcement_deserializer
                        .deserialize::<DeserializeError>(&received[offset..])
                        .map_err(|err| {
                            PeerNetError::HandshakeError.error(
                                "Massa Handshake",
                                Some(format!("Failed to deserialize announcement: {}", err)),
                            )
                        })?;
                    if peer_id
                        .verify_signature(&announcement.hash, &announcement.signature)
                        .is_err()
                    {
                        return Err(PeerNetError::HandshakeError
                            .error("Massa Handshake", Some("Invalid signature".to_string())));
                    }
                    let message = PeerManagementMessage::NewPeerConnected((
                        peer_id.clone(),
                        announcement.clone().listeners,
                    ));
                    let mut bytes = Vec::new();
                    let peer_management_message_serializer = PeerManagementMessageSerializer::new();
                    peer_management_message_serializer
                        .serialize(&message, &mut bytes)
                        .map_err(|err| {
                            PeerNetError::HandshakeError.error(
                                "Massa Handshake",
                                Some(format!("Failed to serialize announcement: {}", err)),
                            )
                        })?;
                    messages_handler.handle(7, &bytes, &peer_id)?;
                    let mut self_random_bytes = [0u8; 32];
                    StdRng::from_entropy().fill_bytes(&mut self_random_bytes);
                    let self_random_hash = Hash::compute_from(&self_random_bytes);
                    let mut bytes = [0u8; 32];
                    bytes[..32].copy_from_slice(&self_random_bytes);

                    endpoint.send(&bytes)?;
                    let received = endpoint.receive()?;
                    let other_random_bytes: &[u8; 32] =
                        received.as_slice().try_into().map_err(|_| {
                            PeerNetError::HandshakeError.error(
                                "Massa Handshake",
                                Some("Failed to deserialize random bytes".to_string()),
                            )
                        })?;

                    // sign their random bytes
                    let other_random_hash = Hash::compute_from(other_random_bytes);
                    let self_signature = keypair.sign(&other_random_hash).map_err(|_| {
                        PeerNetError::HandshakeError.error(
                            "Massa Handshake",
                            Some("Failed to sign random bytes".to_string()),
                        )
                    })?;

                    let mut bytes = [0u8; 64];
                    bytes.copy_from_slice(&self_signature.to_bytes());

                    endpoint.send(&bytes)?;
                    let received = endpoint.receive()?;

                    let other_signature =
                        Signature::from_bytes(received.as_slice().try_into().map_err(|_| {
                            PeerNetError::HandshakeError.error(
                                "Massa Handshake",
                                Some("Failed to get random bytes".to_string()),
                            )
                        })?)
                        .map_err(|_| {
                            PeerNetError::HandshakeError.error(
                                "Massa Handshake",
                                Some("Failed to sign 2 random bytes".to_string()),
                            )
                        })?;

                    // check their signature
                    peer_id.verify_signature(&self_random_hash, &other_signature)?;
                    Ok((peer_id.clone(), Some(announcement)))
                }
                1 => {
                    let (received, id) = self
                        .message_handlers
                        .deserialize_id(&received[offset..], &peer_id)?;
                    self.message_handlers.handle(id, received, &peer_id)?;
                    Ok((peer_id.clone(), None))
                }
                _ => Err(PeerNetError::HandshakeError
                    .error("Massa Handshake", Some("Invalid message id".to_string()))),
            }
        };
        {
            let mut peer_db_write = self.peer_db.write();
            // if handshake failed, we set the peer state to HandshakeFailed
            match &res {
                Ok((peer_id, Some(announcement))) => {
                    info!("Peer connected: {:?}", peer_id);
                    //TODO: Hacky organize better when multiple ip/listeners
                    if !announcement.listeners.is_empty() {
                        peer_db_write
                            .index_by_newest
                            .retain(|(_, peer_id_stored)| peer_id_stored != peer_id);
                        peer_db_write
                            .index_by_newest
                            .insert((Reverse(announcement.timestamp), peer_id.clone()));
                    }
                    peer_db_write
                        .peers
                        .entry(peer_id.clone())
                        .and_modify(|info| {
                            info.last_announce = announcement.clone();
                            info.state = PeerState::Trusted;
                        })
                        .or_insert(PeerInfo {
                            last_announce: announcement.clone(),
                            state: PeerState::Trusted,
                        });
                }
                Ok((_peer_id, None)) => {
                    peer_db_write.peers.entry(peer_id).and_modify(|info| {
                        //TODO: Add the peerdb but for now impossible as we don't have announcement and we need one to place in peerdb
                        info.state = PeerState::HandshakeFailed;
                    });
                    return Err(PeerNetError::HandshakeError.error(
                        "Massa Handshake",
                        Some("Distant peer don't have slot for us.".to_string()),
                    ));
                }
                Err(_) => {
                    peer_db_write.peers.entry(peer_id).and_modify(|info| {
                        //TODO: Add the peerdb but for now impossible as we don't have announcement and we need one to place in peerdb
                        info.state = PeerState::HandshakeFailed;
                    });
                }
            }
        }

        // Send 100 peers to the other peer
        let peers_to_send = {
            let peer_db_read = self.peer_db.read();
            peer_db_read.get_rand_peers_to_send(100)
        };
        let mut buf = Vec::new();
        let msg = PeerManagementMessage::ListPeers(peers_to_send).into();

        self.peer_mngt_msg_serializer.serialize_id(&msg, &mut buf)?;
        self.peer_mngt_msg_serializer.serialize(&msg, &mut buf)?;
        endpoint.send(buf.as_slice())?;

        res.map(|(id, _)| id)
    }

    fn fallback_function(
        &mut self,
        keypair: &KeyPair,
        endpoint: &mut Endpoint,
        _listeners: &HashMap<SocketAddr, TransportType>,
    ) -> PeerNetResult<()> {
        //TODO: Fix this clone
        let keypair = keypair.clone();
        let mut endpoint = endpoint.try_clone()?;
        let db = self.peer_db.clone();
        let serializer = self.peer_mngt_msg_serializer.clone();
        let version_serializer = self.version_serializer.clone();
        let version = self.config.version;
        std::thread::spawn(move || {
            let peers_to_send = db.read().get_rand_peers_to_send(100);
            let mut buf = PeerId::from_public_key(keypair.get_public_key()).to_bytes();
            if let Err(err) = version_serializer
                .serialize(&version, &mut buf)
                .map_err(|err| {
                    PeerNetError::HandshakeError.error(
                        "Massa Handshake",
                        Some(format!(
                            "Failed serialize version, Err: {:?}",
                            err.to_string()
                        )),
                    )
                })
            {
                warn!("{}", err.to_string());
                return;
            }
            buf.push(1);
            let msg = PeerManagementMessage::ListPeers(peers_to_send).into();
            if let Err(err) = serializer.serialize_id(&msg, &mut buf) {
                warn!("Failed to serialize id message: {}", err);
                return;
            }
            if let Err(err) = serializer.serialize(&msg, &mut buf) {
                warn!("Failed to serialize message: {}", err);
                return;
            }
            //TODO: Make it non blockable
            if let Err(err) = endpoint.send(buf.as_slice()) {
                warn!("Failed to send message: {}", err);
                return;
            }
            endpoint.shutdown();
        });
        Ok(())
    }
}
