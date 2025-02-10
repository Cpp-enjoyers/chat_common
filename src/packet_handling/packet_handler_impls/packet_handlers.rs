use crate::packet_handling::{CommandHandler, PacketHandler};
use common::networking::flooder::Flooder;
use log::{error, trace, warn};
use wg_2024::network::SourceRoutingHeader;
use wg_2024::packet::{
    Ack, FloodRequest, FloodResponse, Fragment, Nack, NackType, NodeType, Packet,
};

impl<C, E, H> PacketHandler<C, E, H>
where
    C: std::fmt::Debug,
    E: std::fmt::Debug,
    H: CommandHandler<C, E> + Send + std::fmt::Debug,
    Self: Flooder,
{
    pub(crate) fn pkt_floodresponse(
        &mut self,
        packet: &Packet,
        from_shortcut: bool,
        res: &mut FloodResponse,
    ) {
        if !from_shortcut {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
            self.routing_helper
                .add_from_incoming_routing_header(&packet.routing_header);
        }
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling flood res {res}");
        let tx = self.routing_helper.handle_flood_response(
            packet.routing_header.clone(),
            packet.session_id,
            res,
        );
        for (packet, node_id) in tx {
            if let Some(x) = self.packet_send.get(&node_id) {
                trace!(target: format!("Node {}", self.node_id).as_str(),  "Sending packet to {node_id}: {packet}");
                self.send_to_controller(packet.clone());
                let _ = x.send(packet);
            } else {
                error!(target: format!("Node {}", self.node_id).as_str(), "Can't send flood response to {node_id}: {packet}");
            }
        }
        let mut to_send = vec![];
        for id in self
            .routing_helper
            .topology_graph
            .nodes()
            .filter(|x| *x != self.node_id)
        {
            if let Some(data) = self.routing_helper.node_data.get(&id) {
                match data.node_type {
                    Some(typ) if typ != NodeType::Drone => {
                        if let Some(x) = self.handler.add_node(id, typ) {
                            to_send.push(x)
                        }
                    }
                    _ => {}
                }
            } else {
                trace!(target: format!("Node {}", self.node_id).as_str(),  "Node has no data {id}");
            }
        }
        self.send_messages(to_send);
    }
    pub(crate) fn pkt_floodrequest(
        &mut self,
        packet: &Packet,
        from_shortcut: bool,
        req: &mut FloodRequest,
    ) {
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling flood req {req}");
        if !from_shortcut {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
            self.routing_helper
                .add_from_incoming_routing_header(&packet.routing_header);
        }
        let _ = self.handle_flood_request(&packet.routing_header, packet.session_id, req);
    }
    pub(crate) fn pkt_nack(&mut self, packet: &Packet, from_shortcut: bool, nack: &Nack) {
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
                trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling drop");
                if !from_shortcut {
                    trace!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
                    self.routing_helper
                        .add_from_incoming_routing_header(&packet.routing_header);
                }
                if let Some((dst, frags)) = self.sent_fragments.get(&packet.session_id) {
                    self.routing_helper.report_packet_drop(*dst);
                    #[allow(clippy::cast_possible_truncation)]
                    if let Some(frag) = frags.get(nack.fragment_index as usize) {
                        trace!(target: format!("Node {}", self.node_id).as_str(),  "Resending packet to {dst}: {frag}");
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
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn pkt_msgfragment(
        &mut self,
        packet: &Packet,
        from_shortcut: bool,
        frag: &Fragment,
    ) {
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling message {frag:?}");
        if !from_shortcut {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
            self.routing_helper
                .add_from_incoming_routing_header(&packet.routing_header);
        }
        if frag.fragment_index + 1 > frag.total_n_fragments {
            error!(target: format!("Node {}", self.node_id).as_str(), "fragment index {} is out of bounds from {}", frag.fragment_index, frag.total_n_fragments);
            return;
        }
        if let Some(src) = packet.routing_header.source() {
            let entry = self
                .rx_queue
                .entry((src, packet.session_id))
                .or_insert_with(|| {
                    (
                        Vec::with_capacity(frag.total_n_fragments as usize),
                        (0..frag.total_n_fragments as usize).collect(),
                    )
                });
            entry.0.insert(frag.fragment_index as usize, frag.clone());
            entry.1.retain(|&x| x != frag.fragment_index as usize);
            self.routing_helper
                .report_packet_ack(&packet.routing_header);
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Sending ack!");
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
    pub(crate) fn pkt_ack(&mut self, packet: &Packet, from_shortcut: bool, ack: &Ack) {
        trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling ack {ack:?}");
        if !from_shortcut {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Updating routing table from header");
            self.routing_helper
                .add_from_incoming_routing_header(&packet.routing_header);
        }
        if let Some(src) = packet.routing_header.source() {
            if let Some((node_id, fragments)) = self.sent_fragments.get_mut(&packet.session_id) {
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
}
