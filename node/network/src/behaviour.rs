use crate::{
	config::{ProtocolId, Role},
	debug_info, discovery::{DiscoveryBehaviour, DiscoveryConfig, DiscoveryOut},
	Event, ObservedRole, DhtEvent, ExHashT,
};
use crate::protocol::{self, light_client_handler, message::Roles, CustomMessageOutcome, Protocol};
use libp2p::NetworkBehaviour;
use libp2p::core::{Multiaddr, PeerId, PublicKey};
use libp2p::kad::record;
use libp2p::swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters};
use log::debug;
use sp_consensus::{BlockOrigin, import_queue::{IncomingBlock, Origin}};
use sp_runtime::{traits::{Block as BlockT, NumberFor}, ConsensusEngineId, Justification};
use std::{borrow::Cow, iter, task::Context, task::Poll};
use void;

/// General behaviour of the network. Combines all protocols together.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourOut<B>", poll_method = "poll")]
pub struct Behaviour<B: BlockT, H: ExHashT> {
	/// All the substrate-specific protocols.
	substrate: Protocol<B, H>,
	/// Periodically pings and identifies the nodes we are connected to, and store information in a
	/// cache.
	debug_info: debug_info::DebugInfoBehaviour,
	/// Discovers nodes of the network.
	discovery: DiscoveryBehaviour,
	/// Block request handling.
	block_requests: protocol::BlockRequests<B>,
	/// Light client request handling.
	light_client_handler: protocol::LightClientHandler<B>,

	/// Queue of events to produce for the outside.
	#[behaviour(ignore)]
	events: Vec<BehaviourOut<B>>,

	/// Role of our local node, as originally passed from the configuration.
	#[behaviour(ignore)]
	role: Role,
}

/// Event generated by `Behaviour`.
pub enum BehaviourOut<B: BlockT> {
	BlockImport(BlockOrigin, Vec<IncomingBlock<B>>),
	JustificationImport(Origin, B::Hash, NumberFor<B>, Justification),
	FinalityProofImport(Origin, B::Hash, NumberFor<B>, Vec<u8>),
	/// Started a random Kademlia discovery query.
	RandomKademliaStarted(ProtocolId),
	Event(Event),
}

impl<B: BlockT, H: ExHashT> Behaviour<B, H> {
	/// Builds a new `Behaviour`.
	pub fn new(
		substrate: Protocol<B, H>,
		role: Role,
		user_agent: String,
		local_public_key: PublicKey,
		block_requests: protocol::BlockRequests<B>,
		light_client_handler: protocol::LightClientHandler<B>,
		disco_config: DiscoveryConfig,
	) -> Self {
		Behaviour {
			substrate,
			debug_info: debug_info::DebugInfoBehaviour::new(user_agent, local_public_key.clone()),
			discovery: disco_config.finish(),
			block_requests,
			light_client_handler,
			events: Vec::new(),
			role,
		}
	}

	/// Returns the list of nodes that we know exist in the network.
	pub fn known_peers(&mut self) -> impl Iterator<Item = &PeerId> {
		self.discovery.known_peers()
	}

	/// Adds a hard-coded address for the given peer, that never expires.
	pub fn add_known_address(&mut self, peer_id: PeerId, addr: Multiaddr) {
		self.discovery.add_known_address(peer_id, addr)
	}

	/// Returns the number of nodes that are in the Kademlia k-buckets.
	pub fn num_kbuckets_entries(&mut self) -> impl ExactSizeIterator<Item = (&ProtocolId, usize)> {
		self.discovery.num_kbuckets_entries()
	}

	/// Returns the number of records in the Kademlia record stores.
	pub fn num_kademlia_records(&mut self) -> impl ExactSizeIterator<Item = (&ProtocolId, usize)> {
		self.discovery.num_kademlia_records()
	}

	/// Returns the total size in bytes of all the records in the Kademlia record stores.
	pub fn kademlia_records_total_size(&mut self) -> impl ExactSizeIterator<Item = (&ProtocolId, usize)> {
		self.discovery.kademlia_records_total_size()
	}

	/// Borrows `self` and returns a struct giving access to the information about a node.
	///
	/// Returns `None` if we don't know anything about this node. Always returns `Some` for nodes
	/// we're connected to, meaning that if `None` is returned then we're not connected to that
	/// node.
	pub fn node(&self, peer_id: &PeerId) -> Option<debug_info::Node> {
		self.debug_info.node(peer_id)
	}

	/// Registers a new notifications protocol.
	///
	/// After that, you can call `write_notifications`.
	///
	/// Please call `event_stream` before registering a protocol, otherwise you may miss events
	/// about the protocol that you have registered.
	///
	/// You are very strongly encouraged to call this method very early on. Any connection open
	/// will retain the protocols that were registered then, and not any new one.
	pub fn register_notifications_protocol(
		&mut self,
		engine_id: ConsensusEngineId,
		protocol_name: impl Into<Cow<'static, [u8]>>,
	) {
		let list = self.substrate.register_notifications_protocol(engine_id, protocol_name);
		for (remote, roles) in list {
			let role = reported_roles_to_observed_role(&self.role, remote, roles);
			let ev = Event::NotificationStreamOpened {
				remote: remote.clone(),
				engine_id,
				role,
			};
			self.events.push(BehaviourOut::Event(ev));
		}
	}

	/// Returns a shared reference to the user protocol.
	pub fn user_protocol(&self) -> &Protocol<B, H> {
		&self.substrate
	}

	/// Returns a mutable reference to the user protocol.
	pub fn user_protocol_mut(&mut self) -> &mut Protocol<B, H> {
		&mut self.substrate
	}

	/// Start querying a record from the DHT. Will later produce either a `ValueFound` or a `ValueNotFound` event.
	pub fn get_value(&mut self, key: &record::Key) {
		self.discovery.get_value(key);
	}

	/// Starts putting a record into DHT. Will later produce either a `ValuePut` or a `ValuePutFailed` event.
	pub fn put_value(&mut self, key: record::Key, value: Vec<u8>) {
		self.discovery.put_value(key, value);
	}

	/// Issue a light client request.
	pub fn light_client_request(&mut self, r: light_client_handler::Request<B>) -> Result<(), light_client_handler::Error> {
		self.light_client_handler.request(r)
	}
}

fn reported_roles_to_observed_role(local_role: &Role, remote: &PeerId, roles: Roles) -> ObservedRole {
	if roles.is_authority() {
		match local_role {
			Role::Authority { sentry_nodes }
				if sentry_nodes.iter().any(|s| s.peer_id == *remote) => ObservedRole::OurSentry,
			Role::Sentry { validators }
				if validators.iter().any(|s| s.peer_id == *remote) => ObservedRole::OurGuardedAuthority,
			_ => ObservedRole::Authority
		}
	} else if roles.is_full() {
		ObservedRole::Full
	} else {
		ObservedRole::Light
	}
}

impl<B: BlockT, H: ExHashT> NetworkBehaviourEventProcess<void::Void> for
Behaviour<B, H> {
	fn inject_event(&mut self, event: void::Void) {
		void::unreachable(event)
	}
}

impl<B: BlockT, H: ExHashT> NetworkBehaviourEventProcess<CustomMessageOutcome<B>> for
Behaviour<B, H> {
	fn inject_event(&mut self, event: CustomMessageOutcome<B>) {
		match event {
			CustomMessageOutcome::BlockImport(origin, blocks) =>
				self.events.push(BehaviourOut::BlockImport(origin, blocks)),
			CustomMessageOutcome::JustificationImport(origin, hash, nb, justification) =>
				self.events.push(BehaviourOut::JustificationImport(origin, hash, nb, justification)),
			CustomMessageOutcome::FinalityProofImport(origin, hash, nb, proof) =>
				self.events.push(BehaviourOut::FinalityProofImport(origin, hash, nb, proof)),
			CustomMessageOutcome::NotificationStreamOpened { remote, protocols, roles } => {
				let role = reported_roles_to_observed_role(&self.role, &remote, roles);
				for engine_id in protocols {
					self.events.push(BehaviourOut::Event(Event::NotificationStreamOpened {
						remote: remote.clone(),
						engine_id,
						role: role.clone(),
					}));
				}
			},
			CustomMessageOutcome::NotificationStreamClosed { remote, protocols } =>
				for engine_id in protocols {
					self.events.push(BehaviourOut::Event(Event::NotificationStreamClosed {
						remote: remote.clone(),
						engine_id,
					}));
				},
			CustomMessageOutcome::NotificationsReceived { remote, messages } => {
				let ev = Event::NotificationsReceived { remote, messages };
				self.events.push(BehaviourOut::Event(ev));
			},
			CustomMessageOutcome::PeerNewBest(peer_id, number) => {
				self.light_client_handler.update_best_block(&peer_id, number);
			}
			CustomMessageOutcome::None => {}
		}
	}
}

impl<B: BlockT, H: ExHashT> NetworkBehaviourEventProcess<debug_info::DebugInfoEvent>
	for Behaviour<B, H> {
	fn inject_event(&mut self, event: debug_info::DebugInfoEvent) {
		let debug_info::DebugInfoEvent::Identified { peer_id, mut info } = event;
		if info.listen_addrs.len() > 30 {
			debug!(target: "sub-libp2p", "Node {:?} has reported more than 30 addresses; \
				it is identified by {:?} and {:?}", peer_id, info.protocol_version,
				info.agent_version
			);
			info.listen_addrs.truncate(30);
		}
		for addr in &info.listen_addrs {
			self.discovery.add_self_reported_address(&peer_id, addr.clone());
		}
		self.substrate.add_discovered_nodes(iter::once(peer_id.clone()));
	}
}

impl<B: BlockT, H: ExHashT> NetworkBehaviourEventProcess<DiscoveryOut>
	for Behaviour<B, H> {
	fn inject_event(&mut self, out: DiscoveryOut) {
		match out {
			DiscoveryOut::UnroutablePeer(_peer_id) => {
				// Obtaining and reporting listen addresses for unroutable peers back
				// to Kademlia is handled by the `Identify` protocol, part of the
				// `DebugInfoBehaviour`. See the `NetworkBehaviourEventProcess`
				// implementation for `DebugInfoEvent`.
			}
			DiscoveryOut::Discovered(peer_id) => {
				self.substrate.add_discovered_nodes(iter::once(peer_id));
			}
			DiscoveryOut::ValueFound(results) => {
				self.events.push(BehaviourOut::Event(Event::Dht(DhtEvent::ValueFound(results))));
			}
			DiscoveryOut::ValueNotFound(key) => {
				self.events.push(BehaviourOut::Event(Event::Dht(DhtEvent::ValueNotFound(key))));
			}
			DiscoveryOut::ValuePut(key) => {
				self.events.push(BehaviourOut::Event(Event::Dht(DhtEvent::ValuePut(key))));
			}
			DiscoveryOut::ValuePutFailed(key) => {
				self.events.push(BehaviourOut::Event(Event::Dht(DhtEvent::ValuePutFailed(key))));
			}
			DiscoveryOut::RandomKademliaStarted(protocols) => {
				for protocol in protocols {
					self.events.push(BehaviourOut::RandomKademliaStarted(protocol));
				}
			}
		}
	}
}

impl<B: BlockT, H: ExHashT> Behaviour<B, H> {
	fn poll<TEv>(&mut self, _: &mut Context, _: &mut impl PollParameters) -> Poll<NetworkBehaviourAction<TEv, BehaviourOut<B>>> {
		if !self.events.is_empty() {
			return Poll::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)))
		}

		Poll::Pending
	}
}
