use common::networking::flooder::Flooder;
use log::{debug, error, trace, warn};
use wg_2024::network::{NodeId, SourceRoutingHeader};
use crate::messages::ChatMessage;
use crate::packet_handling::{CommandHandler, CommonChatNode, PacketHandler};
use crate::packet_handling::fragment::defragment;

impl<C, E, H> PacketHandler<C, E, H>
where
    C: std::fmt::Debug,
    E: std::fmt::Debug,
    H: CommandHandler<C, E> + Send + std::fmt::Debug,
    PacketHandler<C, E, H>: Flooder,
{
    pub(crate) fn try_recv_packet(&mut self, busy: &mut bool) {
        if let Ok(pkt) = self.packet_recv.try_recv() {
            *busy = true;
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling packet: {pkt}");
            self.handle_packet(pkt, false);
        }
    }
    pub(crate) fn try_recv_controller(&mut self, busy: &mut bool) {
        if let Ok(cmd) = self.controller_recv.try_recv() {
            *busy = true;
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling controller command: {cmd:?}");
            let (p, m, e) = self.handler.handle_controller_command(&mut self.packet_send, cmd);
            if let Some(packet) = p {
                trace!(target: format!("Node {}", self.node_id).as_str(),  "Handling packet from shortcut: {packet}");
                self.handle_packet(packet, true);
            }
            self.send_controller_events(e);
            self.send_messages(m);
        }
    }
    pub(crate) fn recieve_fragments_from_queue(&mut self, busy: &mut bool) {
        for (key, (frags, missing)) in self.rx_queue.clone() {
            *busy = true;
            if missing.is_empty() {
                trace!(target: format!("Node {}", self.node_id).as_str(),  "All fragments received, proceeding to defragment: {frags:?}");
                if let Ok(message) = defragment(&frags) {
                    let (data_to_send, events_to_send) = self.handler.handle_protocol_message(message);
                    self.send_controller_events(events_to_send);
                    self.send_messages(data_to_send);
                } else {
                    warn!(target: format!("Node {}", self.node_id).as_str(), "Defragmentation failed! {:?}", defragment(&frags).err());
                }
                self.rx_queue.remove(&key);
            }
        }
    }
    pub(crate) fn send_messages(&mut self, data_to_send: Vec<(NodeId, ChatMessage)>) {
        for (id, msg) in data_to_send {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Sending to {id}: {msg:?}");
            self.send_msg(msg, id);
        }
    }
    fn send_controller_events(&mut self, e: Vec<E>) {
        for event in e {
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Sending to controller: {event:?}");
            let _ = self.controller_send.send(event);
        }
    }
    pub(crate) fn send_packets(&mut self, busy: &mut bool) {
        let mut failed_sends = vec![];
        while let Some((packet, node_id)) = self.tx_queue_packets.pop_front() {
            *busy = true;
            let mut failed = true;
            trace!(target: format!("Node {}", self.node_id).as_str(), "Sending packet {} to {}", packet, node_id);
            if packet.routing_header != SourceRoutingHeader::empty_route() {
                if let Some(sender) = self.packet_send.get(&node_id) {
                    let _ = sender.send(packet.clone());
                    let _ = self.controller_send.send(self.handler.report_sent_packet(packet.clone()));
                    failed = false;
                    trace!(target: format!("Node {}", self.node_id).as_str(), "Packet sent successfully without generating route");
                } else {
                    trace!(target: format!("Node {}", self.node_id).as_str(), "No longer connected to neighbor {}", node_id);
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
                        trace!(target: format!("Node {}", self.node_id).as_str(), "Packet sent successfully");
                    } else {
                        self.flood_flag = true;
                        self.routing_helper.remove_node(next_hop);
                        debug!(target: format!("Node {}", self.node_id).as_str(), "No longer connected to next hop {}", next_hop);
                    }
                } else {
                    error!(target: format!("Node {}", self.node_id).as_str(), "Route {route} doesn't contain a next hop!");
                }
            } else {
                debug!(target: format!("Node {}", self.node_id).as_str(), "No route found to {}", node_id);
            }
            if failed {
                debug!(target: format!("Node {}", self.node_id).as_str(), "Cannot send packet to {node_id}, no route found!");
                failed_sends.push((packet, node_id));
            }
        }
        if !failed_sends.is_empty() {
            trace!(target: format!("Node {}", self.node_id).as_str(), "Packets {failed_sends:?} failed to send, pushing in queue");
        }
        failed_sends
            .iter()
            .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
    }
    pub(crate) fn do_flood(&mut self, busy: &mut bool) {
        if self.flood_flag {
            *busy = true;
            trace!(target: format!("Node {}", self.node_id).as_str(),  "Sending flood...");
            self.flood_flag = false;
            let flood_req = self
                .routing_helper
                .generate_flood_requests(self.packet_send.keys().copied().collect());
            flood_req
                .iter()
                .for_each(|x| self.tx_queue_packets.push_back(x.clone()));
        }
    }
}
