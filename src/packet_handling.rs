use crate::fragment::defragment;
use crate::messages::ChatMessage;
use crate::routing::{NodeInfo, RoutingHelper};
use common::networking::flooder::Flooder;
use common::ring_buffer::RingBuffer;
use common::slc_commands::{
    ChatClientCommand as CC, ChatClientEvent as CE, ServerCommand as SC, ServerEvent as SE,
};
use common::{Client, Server};
use crossbeam::channel::{select_biased, Receiver, Sender};
use std::collections::{HashMap, VecDeque};
use log::{debug, error, info, trace, warn};
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Fragment, NackType, NodeType, Packet, PacketType};
use crate::fragment;

#[derive(Debug)]
pub struct PacketHandler<C, E, H: CommandHandler<C, E> + Send> where H: std::fmt::Debug {
    pub routing_helper: RoutingHelper,
    pub node_id: NodeId,
    pub controller_send: Sender<E>,
    pub controller_recv: Receiver<C>,
    pub packet_recv: Receiver<Packet>,
    pub packet_send: HashMap<NodeId, Sender<Packet>>,
    pub seen_flood_ids: RingBuffer<(NodeId, u64)>,
    pub handler: H,
    pub tx_queue_packets: VecDeque<(Packet, NodeId)>,
    pub sent_fragments: HashMap<u64, (NodeId, Vec<Fragment>)>, // session_id -> (node_id, fragment(s))
    pub rx_queue: HashMap<(NodeId, u64), (Vec<Fragment>, Vec<usize>)>, // (node_id, session_id) -> (fragments, missing fragment indexes)
    pub flood_flag: bool,
    pub cur_session_id: u64,
}

// Convenience traits to implement common packet handling features
pub type HandlerFunction<C, E, H> = Box<dyn FnOnce(&mut PacketHandler<C, E, H>)>;
pub trait CommandHandler<C, E> {
    fn get_node_type() -> NodeType;

    /// Returns a Vec<(NodeId, Vec<Fragment>)> to add to the tx and fragment queue
    /// Every element in the vector is a list of fragments to send to the corresponding node
    /// The second element is the list of events to be sent to the controller
    fn handle_protocol_message(&mut self, message: ChatMessage) -> (Vec<(NodeId, ChatMessage)>, Vec<E>)
    where
        Self: Sized;

    /// Returns the event that has to be sent to the controller
    fn report_sent_packet(&mut self, packet: Packet) -> E
    where
        Self: Sized;

    /// Obtains the senders hashmap and returns either a packet to be handled or an event to be sent to the controller
    fn handle_controller_command(&mut self, sender_hash: &mut HashMap<NodeId, Sender<Packet>>, command: C) -> (Option<Packet>, Vec<(NodeId, ChatMessage)>, Vec<E>)
    where
        Self: Sized;
    
    fn add_node(&mut self, id: NodeId, typ: NodeType) -> Option<(NodeId, ChatMessage)>;
    
    fn new(id: NodeId) -> Self
    where
        Self: Sized;
}
pub trait CommonChatNode<C, E> {
    fn new_node(
        id: NodeId,
        controller_send: Sender<E>,
        controller_recv: Receiver<C>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
    ) -> Self
    where
        Self: Sized;
    fn run_node(&mut self);
    fn send_msg(&mut self, msg: ChatMessage, id: NodeId);
    fn handle_packet(&mut self, packet: Packet, from_shortcut: bool);
    fn remove_sender(&mut self, node_id: NodeId);
    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>);
}
impl<H> Server for PacketHandler<SC, SE, H>
where
    H: CommandHandler<SC, SE> + Send + std::fmt::Debug,
{
    fn new(
        id: NodeId,
        controller_send: Sender<SE>,
        controller_recv: Receiver<SC>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
    ) -> Self {
        Self::new_node(
            id,
            controller_send,
            controller_recv,
            packet_recv,
            packet_send,
        )
    }
    fn run(&mut self) {
        self.run_node()
    }
}
impl<H> Client for PacketHandler<CC, CE, H>
where
    H: CommandHandler<CC, CE> + Send + std::fmt::Debug,
{
    type T = CC;
    type U = CE;

    fn new(
        id: NodeId,
        controller_send: Sender<CE>,
        controller_recv: Receiver<CC>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
    ) -> Self {
        Self::new_node(
            id,
            controller_send,
            controller_recv,
            packet_recv,
            packet_send,
        )
    }
    fn run(&mut self) {
        self.run_node()
    }
}

impl<C, E, H> CommonChatNode<C, E> for PacketHandler<C, E, H>
where
    H: CommandHandler<C, E> + Send + std::fmt::Debug,
    PacketHandler<C, E, H>: Flooder, E: std::fmt::Debug, C: std::fmt::Debug
{
    fn new_node(
        id: NodeId,
        controller_send: Sender<E>,
        controller_recv: Receiver<C>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
    ) -> Self {
        PacketHandler {
            routing_helper: RoutingHelper::new_with_neighbors(
                id,
                H::get_node_type(),
                packet_send.keys().cloned().collect(),
            ),
            node_id: id,
            controller_send,
            controller_recv,
            packet_recv,
            packet_send,
            seen_flood_ids: RingBuffer::with_capacity(1024),
            handler: H::new(id),
            tx_queue_packets: Default::default(),
            sent_fragments: Default::default(),
            rx_queue: Default::default(),
            flood_flag: true,
            cur_session_id: 1,
        }
    }
    fn run_node(&mut self) {
        loop {
            info!(target: format!("Node {}", self.node_id).as_str(), "State: {self:?}");
            if self.flood_flag {
                info!(target: format!("Node {}", self.node_id).as_str(),  "Sending flood...");
                self.flood_flag = false;
                let flood_req = self
                    .routing_helper
                    .generate_flood_requests(self.packet_send.keys().cloned().collect());
                info!(target: format!("Node {}", self.node_id).as_str(), "Generated flood packets {:?}", flood_req);
                flood_req
                    .iter()
                    .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
            }
            let mut failed_sends = vec![];
            while let Some((packet, node_id)) = self.tx_queue_packets.pop_front() {
                let mut failed = true;
                info!(target: format!("Node {}", self.node_id).as_str(), "Sending packet {} to {}", packet, node_id);
                if packet.routing_header != SourceRoutingHeader::empty_route() {
                    if let Some(sender) = self.packet_send.get(&node_id) {
                            let _ = sender.send(packet.clone());
                            let _ = self.controller_send.send(self.handler.report_sent_packet(packet.clone()));
                            failed = false;
                            info!(target: format!("Node {}", self.node_id).as_str(), "Packet sent successfully without generating route");
                        } else {
                            warn!(target: format!("Node {}", self.node_id).as_str(), "No longer connected to neighbor {}", node_id);
                        }
                } else if let Some(route) = self
                    .routing_helper
                    .generate_source_routing_header(self.node_id, node_id)
                {
                    if let Some(next_hop) = route.next_hop() {
                        if let Some(sender) = self.packet_send.get(&next_hop) {
                            let mut final_packet = packet.clone();
                            final_packet.routing_header = route;
                            final_packet.routing_header.hop_index = 1;
                            let _ = sender.send(final_packet);
                            let _ = self.controller_send.send(self.handler.report_sent_packet(packet.clone()));
                            failed = false;
                            info!(target: format!("Node {}", self.node_id).as_str(), "Packet sent successfully");
                        } else {
                            self.flood_flag = true;
                            self.routing_helper.remove_node(next_hop);
                            warn!(target: format!("Node {}", self.node_id).as_str(), "No longer connected to next hop {}", next_hop);
                        }
                    } else {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Route {route} doesn't contain a next hop!");
                    }
                } else {
                    info!(target: format!("Node {}", self.node_id).as_str(), "No route found to {}", node_id);
                }
                if failed {
                    error!(target: format!("Node {}", self.node_id).as_str(), "Cannot send packet to {node_id}, no route found!");
                    failed_sends.push((packet, node_id));
                }
            }
            if failed_sends.len() > 0 {
                info!(target: format!("Node {}", self.node_id).as_str(), "Packets {failed_sends:?} failed to send, pushing in queue");
            }
            failed_sends
                .iter()
                .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
            for (key, (frags, missing)) in self.rx_queue.clone() {
                if missing.is_empty() {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "All fragments received, proceeding to defragment: {frags:?}");
                    if let Ok(message) = defragment(&frags) {
                        let (data_to_send, events_to_send) = self.handler.handle_protocol_message(message);
                        for event in events_to_send.into_iter() {
                            info!(target: format!("Node {}", self.node_id).as_str(),  "Sending to controller: {event:?}");
                            let _ = self.controller_send.send(event);
                        }
                        for (id,msg) in data_to_send {
                            info!(target: format!("Node {}", self.node_id).as_str(),  "Sending to {id}: {msg:?}");
                            self.send_msg(msg,id);
                        }
                    } else {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Defragmentation failed! {:?}", defragment(&frags));
                    }
                    self.rx_queue.remove(&key);
                }
            }

            select_biased! {
                recv(self.controller_recv) -> cmd => {
                    if let Ok(cmd) = cmd {
                         info!(target: format!("Node {}", self.node_id).as_str(),  "Handling controller command: {cmd:?}");
                    let (p,m,e) = self.handler.handle_controller_command(&mut self.packet_send, cmd);
                        if let Some(packet) = p {
                            info!(target: format!("Node {}", self.node_id).as_str(),  "Handling packet from shortcut: {packet}");
                            self.handle_packet(packet, true);
                        }
                        for event in e.into_iter() {
                            info!(target: format!("Node {}", self.node_id).as_str(),  "Sending to controller: {event:?}");
                            let _ = self.controller_send.send(event);
                        }
                        for (id,msg) in m {
                            info!(target: format!("Node {}", self.node_id).as_str(), "Sending to {id}: {msg:?}");
                            self.send_msg(msg,id);
                        }
                    }
                },
                recv(self.packet_recv) -> pkt => {
                    if let Ok(pkt) = pkt {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Handling packet: {pkt}");
                        self.handle_packet(pkt, false);
                    }
                }
            }
        }
    }
    
    fn send_msg(&mut self, msg: ChatMessage, id: NodeId) {
        let fragments = fragment::fragment(msg);
        self.cur_session_id += 1;
        for frag in fragments.clone() {
            info!(target: format!("Node {}", self.node_id).as_str(), "Adding fragment to txq: {frag}");
            self.tx_queue_packets.push_back((Packet::new_fragment(
                SourceRoutingHeader::empty_route(),
                self.cur_session_id,
                frag,
            ),id));
        }
        info!(target: format!("Node {}", self.node_id).as_str(),  "Marking fragments for sending: {fragments:?}");
        self.sent_fragments.insert(self.cur_session_id, (id, fragments));
    }
    
    fn handle_packet(&mut self, packet: Packet, from_shortcut: bool) {
        match packet.pack_type {
            PacketType::MsgFragment(frag) => {
                info!(target: format!("Node {}", self.node_id).as_str(),  "Handling message {frag:?}");
                if !from_shortcut {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                if frag.fragment_index + 1 > frag.total_n_fragments {
                    error!(target: format!("Node {}", self.node_id).as_str(), "fragment index {} is out of bounds from {}", frag.fragment_index, frag.total_n_fragments);
                    return;
                }
                if let Some(src) = packet.routing_header.source() {
                    let entry = self.rx_queue.entry((src, packet.session_id)).or_insert((
                        Vec::with_capacity(frag.total_n_fragments as usize),
                        (0..frag.total_n_fragments as usize).collect(),
                    ));
                    entry.0.insert(frag.fragment_index as usize, frag.clone());
                    entry.1.retain(|&x| x != frag.fragment_index as usize);
                    self.routing_helper
                        .report_packet_ack(&packet.routing_header);
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Sending ack!");
                    self.tx_queue_packets.push_back((
                        Packet::new_ack(
                            SourceRoutingHeader::empty_route(),
                            packet.session_id,
                            frag.fragment_index,
                        ),
                        src,
                    ));
                } else {
                    error!(target: format!("Node {}", self.node_id).as_str(), "Packet has no source!");
                }
            }
            PacketType::Ack(ack) => {
                 info!(target: format!("Node {}", self.node_id).as_str(),  "Handling ack {ack:?}");
                if !from_shortcut {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                if let Some(src) = packet.routing_header.source() {
                    if let Some((node_id, fragments)) =
                        self.sent_fragments.get_mut(&packet.session_id)
                    {
                        if *node_id == src {
                            fragments.retain(|x| x.fragment_index != ack.fragment_index);
                            if fragments.is_empty() {
                                self.sent_fragments.remove(&packet.session_id);
                            }
                        } else {
                            error!(target: format!("Node {}", self.node_id).as_str(), "Packet from {src} is marked as being sent to {node_id}");
                        }
                    } else {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Received ack for fragment that was never sent");
                    }
                } else {
                    error!(target: format!("Node {}", self.node_id).as_str(), "Packet has no source!");
                }
            }
            PacketType::Nack(nack) => {
                match nack.nack_type {
                    NackType::ErrorInRouting(id) => {
                        warn!(target: format!("Node {}", self.node_id).as_str(), "Drone {id} probably crashed - ErrorInRouting");
                        self.routing_helper.remove_node(id);
                        self.flood_flag = true;
                    }
                    NackType::DestinationIsDrone => {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Received DestinationIsDrone {nack}");
                    }
                    NackType::Dropped => {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Handling drop");
                        if !from_shortcut {
                            info!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                            self.routing_helper
                                .add_from_incoming_routing_header(&packet.routing_header);
                        }
                        if let Some((dst, frags)) = self.sent_fragments.get(&packet.session_id) {
                            self.routing_helper.report_packet_drop(*dst);
                            if let Some(frag) = frags.get(nack.fragment_index as usize) {
                                info!(target: format!("Node {}", self.node_id).as_str(),  "Resending packet to {dst}: {frag}");
                                self.tx_queue_packets.push_back((
                                    Packet::new_fragment(
                                        SourceRoutingHeader::empty_route(),
                                        packet.session_id,
                                        frag.clone(),
                                    ),
                                    *dst,
                                ));
                            }
                        }
                    }
                    NackType::UnexpectedRecipient(_) => {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Got UnexpectedRecipient {nack}");
                    }
                }
            }
            PacketType::FloodRequest(mut req) => {
                info!(target: format!("Node {}", self.node_id).as_str(),  "Handling flood req {req}");
                if !from_shortcut {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                let _ =
                    self.handle_flood_request(&packet.routing_header, packet.session_id, &mut req);
            }
            PacketType::FloodResponse(mut res) => {
                if !from_shortcut {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                info!(target: format!("Node {}", self.node_id).as_str(),  "Handling flood res {res}");
                let tx = self.routing_helper.handle_flood_response(
                    packet.routing_header,
                    packet.session_id,
                    &mut res,
                );
                for (packet, node_id) in tx {
                    if let Some(x) = self.packet_send.get(&node_id) {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Sending packet to {node_id}: {packet}");
                        self.send_to_controller(packet.clone());
                        let _ = x.send(packet);
                    } else {
                        error!(target: format!("Node {}", self.node_id).as_str(), "Can't send flood response to {node_id}: {packet}");
                    }
                }
                let mut to_send = vec![];
                for id in self.routing_helper.topology_graph.nodes().filter(|x| *x != self.node_id) {
                    if let Some(data) = self.routing_helper.node_data.get(&id) {
                        match data.node_type { 
                            Some(typ) if typ != NodeType::Drone => { 
                                self.handler.add_node(id,typ).map(|x| to_send.push(x));
                            }
                            _ => {}
                        }
                    } else {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Node has no data {id}");
                    }
                }
                for (i,m) in to_send {
                    info!(target: format!("Node {}", self.node_id).as_str(),  "Sending discovery {i}: {m:?}");
                    self.send_msg(m,i);
                }
            }
        }
    }
    fn remove_sender(&mut self, node_id: NodeId) {
        info!(target: format!("Node {}", self.node_id).as_str(),  "Removing sender {node_id}");
        self.packet_send.remove(&node_id);
        self.routing_helper.remove_node(node_id);
        self.flood_flag = true;
    }
    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>) {
        info!(target: format!("Node {}", self.node_id).as_str(),  "Adding sender {node_id}");
        self.packet_send.insert(node_id, sender);
        self.routing_helper
            .topology_graph
            .add_edge(self.node_id, node_id, 1.0);
        self.routing_helper.node_data.insert(
            node_id,
            NodeInfo {
                id: node_id,
                dropped_packets: 0,
                acked_packets: 0,
                node_type: None,
            },
        );
        self.flood_flag = true;
    }
}
impl<H: CommandHandler<CC, CE> + Send + std::fmt::Debug> Flooder for PacketHandler<CC, CE, H>
where
    H: CommandHandler<CC, CE>,
{
    const NODE_TYPE: NodeType = NodeType::Client;

    fn get_id(&self) -> NodeId {
        self.node_id
    }

    fn get_neighbours(&self) -> impl ExactSizeIterator<Item = (&NodeId, &Sender<Packet>)> {
        self.packet_send.iter()
    }

    fn has_seen_flood(&self, flood_id: (NodeId, u64)) -> bool {
        self.seen_flood_ids.contains(&flood_id)
    }

    fn insert_flood(&mut self, flood_id: (NodeId, u64)) {
        self.seen_flood_ids.insert(flood_id);
    }

    fn send_to_controller(&self, p: Packet) {
        let _ = self.controller_send.send(CE::PacketSent(p));
    }
}
impl<H: CommandHandler<SC, SE> + Send + std::fmt::Debug> Flooder for PacketHandler<SC, SE, H>
where
    H: CommandHandler<SC, SE>,
{
    const NODE_TYPE: NodeType = NodeType::Server;

    fn get_id(&self) -> NodeId {
        self.node_id
    }

    fn get_neighbours(&self) -> impl ExactSizeIterator<Item = (&NodeId, &Sender<Packet>)> {
        self.packet_send.iter()
    }

    fn has_seen_flood(&self, flood_id: (NodeId, u64)) -> bool {
        self.seen_flood_ids.contains(&flood_id)
    }

    fn insert_flood(&mut self, flood_id: (NodeId, u64)) {
        self.seen_flood_ids.insert(flood_id);
    }

    fn send_to_controller(&self, p: Packet) {
        let _ = self.controller_send.send(SE::PacketSent(p));
    }
}
