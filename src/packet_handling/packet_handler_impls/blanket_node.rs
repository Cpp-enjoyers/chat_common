use common::slc_commands::{
    ChatClientCommand as CC, ChatClientEvent as CE, ServerCommand as SC, ServerEvent as SE,
};
use std::collections::HashMap;
use common::{Client, Server};
use common::networking::flooder::Flooder;
use crossbeam::channel::{Receiver, Sender};
use wg_2024::network::NodeId;
use wg_2024::packet::{NodeType, Packet};
use crate::packet_handling::{CommandHandler, CommonChatNode, PacketHandler};

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