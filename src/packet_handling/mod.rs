use crate::messages::ChatMessage;
use crate::packet_handling::routing::{NodeInfo, RoutingHelper};
use common::networking::flooder::Flooder;
use common::ring_buffer::RingBuffer;
use crossbeam::channel::{Receiver, Sender};
use log::trace;
use std::collections::{HashMap, VecDeque};
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::{Fragment, NodeType, Packet, PacketType};

pub mod fragment;
mod handling_steps;
mod packet_handler_impls;
pub mod routing;

#[derive(Debug)]
pub struct PacketHandler<C, E, H: CommandHandler<C, E> + Send> {
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

    /// Returns a `Vec<(NodeId, Vec<Fragment>)>` to add to the tx and fragment queue
    /// Every element in the vector is a list of fragments to send to the corresponding node
    /// The second element is the list of events to be sent to the controller
    fn handle_protocol_message(
        &mut self,
        message: ChatMessage,
    ) -> (Vec<(NodeId, ChatMessage)>, Vec<E>)
    where
        Self: Sized;

    /// Returns the event that has to be sent to the controller
    fn report_sent_packet(&mut self, packet: Packet) -> E
    where
        Self: Sized;

    /// Obtains the senders hashmap and returns either a packet to be handled or an event to be sent to the controller
    fn handle_controller_command(
        &mut self,
        sender_hash: &mut HashMap<NodeId, Sender<Packet>>,
        command: C,
    ) -> (Option<Packet>, Vec<(NodeId, ChatMessage)>, Vec<E>)
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

impl<C, E, H> CommonChatNode<C, E> for PacketHandler<C, E, H>
where
    H: CommandHandler<C, E> + Send + std::fmt::Debug,
    Self: Flooder,
    E: std::fmt::Debug,
    C: std::fmt::Debug,
{
    fn new_node(
        id: NodeId,
        controller_send: Sender<E>,
        controller_recv: Receiver<C>,
        packet_recv: Receiver<Packet>,
        packet_send: HashMap<NodeId, Sender<Packet>>,
    ) -> Self {
        Self {
            routing_helper: RoutingHelper::new_with_neighbors(
                id,
                H::get_node_type(),
                packet_send.keys().copied().collect(),
            ),
            node_id: id,
            controller_send,
            controller_recv,
            packet_recv,
            packet_send,
            seen_flood_ids: RingBuffer::with_capacity(1024),
            handler: H::new(id),
            tx_queue_packets: VecDeque::default(),
            sent_fragments: HashMap::default(),
            rx_queue: HashMap::default(),
            flood_flag: true,
            cur_session_id: 1,
        }
    }
    fn run_node(&mut self) {
        let mut busy = true;
        loop {
            if busy {
                trace!(target: format!("Node {}", self.node_id).as_str(), "State: {self:?}");
                busy = false;
            }
            self.do_flood(&mut busy);
            self.send_packets(&mut busy);
            self.recieve_fragments_from_queue(&mut busy);

            self.try_recv_controller(&mut busy);
            self.try_recv_packet(&mut busy);
        }
    }

    fn send_msg(&mut self, msg: ChatMessage, id: NodeId) {
        let fragments = fragment::fragment(&msg);
        self.cur_session_id += 1;
        for frag in fragments.clone() {
            let x = (
                Packet::new_fragment(
                    SourceRoutingHeader::empty_route(),
                    self.cur_session_id,
                    frag,
                ),
                id,
            );
            trace!(target: format!("Node {}", self.node_id).as_str(), "Adding packet to txq: {x:?}");
            self.tx_queue_packets.push_back(x);
        }
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Marking fragments for sending: {fragments:?}");
        self.sent_fragments
            .insert(self.cur_session_id, (id, fragments));
    }

    fn handle_packet(&mut self, packet: Packet, from_shortcut: bool) {
        match packet.pack_type {
            PacketType::MsgFragment(ref frag) => self.pkt_msgfragment(&packet, from_shortcut, frag),
            PacketType::Ack(ref ack) => self.pkt_ack(&packet, from_shortcut, ack),
            PacketType::Nack(ref nack) => self.pkt_nack(&packet, from_shortcut, nack),
            PacketType::FloodRequest(ref req) => {
                self.pkt_floodrequest(&packet, from_shortcut, &mut req.clone());
            }
            PacketType::FloodResponse(ref res) => {
                self.pkt_floodresponse(&packet, from_shortcut, &mut res.clone());
            }
        }
    }
    fn remove_sender(&mut self, node_id: NodeId) {
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Removing sender {node_id}");
        self.packet_send.remove(&node_id);
        self.routing_helper.remove_node(node_id);
        self.flood_flag = true;
    }
    fn add_sender(&mut self, node_id: NodeId, sender: Sender<Packet>) {
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Adding sender {node_id}");
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
