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
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Fragment, NackType, NodeType, Packet, PacketType};

pub struct PacketHandler<C, E, H: CommandHandler<C, E>> {
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
    pub rx_queue: HashMap<(NodeId, u64), (Vec<[u8; 128]>, Vec<usize>)>, // (node_id, session_id) -> (fragments, missing fragment indexes)
    pub flood_flag: bool,
    pub cur_session_id: u64,
}

// Convenience traits to implement common packet handling features
pub type HandlerFunction<C, E, H> = Box<dyn FnOnce(&mut PacketHandler<C, E, H>)>;
pub trait CommandHandler<C, E> {
    fn get_node_type() -> NodeType;

    fn handle_protocol_message(&mut self, message: ChatMessage) -> HandlerFunction<C, E, Self>
    where
        Self: Sized;
    fn report_sent_packet(&mut self, packet: Packet) -> HandlerFunction<C, E, Self>
    where
        Self: Sized;
    fn handle_controller_command(&mut self, command: C) -> HandlerFunction<C, E, Self>
    where
        Self: Sized;
    fn new() -> Self
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
    fn handle_packet(&mut self, packet: Packet, from_shortcut: bool);
    fn remove_sender(&mut self, node_id: NodeId);
    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>);
}
impl<H> Server for PacketHandler<SC, SE, H>
where
    H: CommandHandler<SC, SE>,
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
impl<H> Client<CC, CE> for PacketHandler<CC, CE, H>
where
    H: CommandHandler<CC, CE>,
{
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
    H: CommandHandler<C, E>,
    PacketHandler<C, E, H>: Flooder,
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
            handler: H::new(),
            tx_queue_packets: Default::default(),
            sent_fragments: Default::default(),
            rx_queue: Default::default(),
            flood_flag: true,
            cur_session_id: 0,
        }
    }
    fn run_node(&mut self) {
        loop {
            if self.flood_flag {
                self.flood_flag = false;
                let flood_req = self
                    .routing_helper
                    .generate_flood_requests(self.packet_send.keys().cloned().collect());
                flood_req
                    .iter()
                    .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
            }
            let mut failed_sends = vec![];
            while let Some((packet, node_id)) = self.tx_queue_packets.pop_front() {
                let mut failed = true;
                if let Some(route) = self
                    .routing_helper
                    .generate_source_routing_header(self.node_id, node_id)
                {
                    if let Some(next_hop) = route.next_hop() {
                        if let Some(sender) = self.packet_send.get(&next_hop) {
                            let _ = sender.send(packet.clone());
                            self.handler.report_sent_packet(packet.clone())(self);
                            failed = false;
                        }
                    }
                }
                if failed {
                    failed_sends.push((packet, node_id));
                }
            }
            failed_sends
                .iter()
                .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
            for (key, (frags, missing)) in self.rx_queue.clone() {
                if missing.is_empty() {
                    if let Ok(message) = defragment(&frags) {
                        self.handler.handle_protocol_message(message)(self);
                    } else {
                        // Error: defragmentation failed
                    }
                    self.rx_queue.remove(&key);
                }
            }

            select_biased! {
                recv(self.controller_recv) -> cmd => {
                    if let Ok(cmd) = cmd {
                    self.handler.handle_controller_command(cmd)(self);
                    }
                },
                recv(self.packet_recv) -> pkt => {
                    if let Ok(pkt) = pkt {
                        self.handle_packet(pkt, false);
                    }
                }
            }
        }
    }
    fn handle_packet(&mut self, packet: Packet, from_shortcut: bool) {
        match packet.pack_type {
            PacketType::MsgFragment(frag) => {
                if !from_shortcut {
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                if frag.fragment_index + 1 > frag.total_n_fragments {
                    // Error: fragment index is out of bounds
                    return;
                }
                if let Some(src) = packet.routing_header.source() {
                    let entry = self.rx_queue.entry((src, packet.session_id)).or_insert((
                        Vec::with_capacity(frag.total_n_fragments as usize),
                        (0..frag.total_n_fragments as usize).collect(),
                    ));
                    entry.0.insert(frag.fragment_index as usize, frag.data);
                    entry.1.retain(|&x| x != frag.fragment_index as usize);
                    self.routing_helper
                        .report_packet_ack(&packet.routing_header);
                    // prepare ack to send
                    self.tx_queue_packets.push_back((
                        Packet::new_ack(
                            SourceRoutingHeader::empty_route(),
                            packet.session_id,
                            frag.fragment_index,
                        ),
                        src,
                    ));
                }
            }
            PacketType::Ack(ack) => {
                if !from_shortcut {
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
                        }
                    }
                }
            }
            PacketType::Nack(nack) => {
                match nack.nack_type {
                    NackType::ErrorInRouting(id) => {
                        // Drone probably crashed, so we remove it
                        self.routing_helper.remove_node(id);
                        self.flood_flag = true;
                    }
                    NackType::DestinationIsDrone => {}
                    NackType::Dropped => {
                        if !from_shortcut {
                            self.routing_helper
                                .add_from_incoming_routing_header(&packet.routing_header);
                        }
                        if let Some((dst, frags)) = self.sent_fragments.get(&packet.session_id) {
                            self.routing_helper.report_packet_drop(*dst);
                            if let Some(frag) = frags.get(nack.fragment_index as usize) {
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
                    NackType::UnexpectedRecipient(_) => {}
                }
            }
            PacketType::FloodRequest(mut req) => {
                if !from_shortcut {
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                let _ =
                    self.handle_flood_request(&packet.routing_header, packet.session_id, &mut req);
            }
            PacketType::FloodResponse(mut res) => {
                if !from_shortcut {
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                let tx = self.routing_helper.handle_flood_response(
                    packet.routing_header,
                    packet.session_id,
                    &mut res,
                );
                for (packet, node_id) in tx {
                    if let Some(x) = self.packet_send.get(&node_id) {
                        self.send_to_controller(packet.clone());
                        let _ = x.send(packet);
                    }
                }
            }
        }
    }
    fn remove_sender(&mut self, node_id: NodeId) {
        self.packet_send.remove(&node_id);
        self.routing_helper.remove_node(node_id);
        self.flood_flag = true;
    }
    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>) {
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
impl<H: CommandHandler<CC, CE>> Flooder for PacketHandler<CC, CE, H>
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
        let _ = self.controller_send.send(CE::Shortcut(p));
    }
}
impl<H: CommandHandler<SC, SE>> Flooder for PacketHandler<SC, SE, H>
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
        let _ = self.controller_send.send(SE::ShortCut(p));
    }
}
