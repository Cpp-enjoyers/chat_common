use itertools::Itertools;
use petgraph::algo::astar;
use petgraph::prelude::DiGraphMap;
use std::collections::HashMap;
use log::{error, info, trace};
use wg_2024::network::{NodeId, SourceRoutingHeader};
use wg_2024::packet::NodeType::Drone;
use wg_2024::packet::{FloodRequest, FloodResponse, NodeType, Packet};

pub struct RoutingHelper {
    pub cur_flood_id: u64,
    pub topology_graph: DiGraphMap<NodeId, f64>,
    pub node_data: HashMap<NodeId, NodeInfo>,
    pub node_id: NodeId,
    pub node_type: NodeType,
}
pub struct NodeInfo {
    pub id: NodeId,
    pub dropped_packets: u64,
    pub acked_packets: u64,
    pub node_type: Option<NodeType>,
}
impl RoutingHelper {
    pub fn new(node_id: NodeId, node_type: NodeType) -> Self {
        RoutingHelper {
            cur_flood_id: 1,
            topology_graph: DiGraphMap::new(),
            node_data: HashMap::new(),
            node_id,
            node_type,
        }
    }
    pub fn new_with_neighbors(
        node_id: NodeId,
        node_type: NodeType,
        neighbors: Vec<NodeId>,
    ) -> Self {
        let mut helper = RoutingHelper::new(node_id, node_type);
        for neighbor in neighbors {
            helper.topology_graph.add_edge(node_id, neighbor, 1.0);
            helper.node_data.insert(
                neighbor,
                NodeInfo {
                    id: neighbor,
                    dropped_packets: 0,
                    acked_packets: 0,
                    node_type: None,
                },
            );
        }
        helper
    }
    pub fn generate_source_routing_header(
        &self,
        src: NodeId,
        dst: NodeId,
    ) -> Option<SourceRoutingHeader> {
        if let Some(path) = astar(
            &self.topology_graph,
            src,
            |finish| finish == dst,
            |e| {
                let a_pdr = {
                    if let Some(node) = self.node_data.get(&e.0) {
                        node.dropped_packets as f64
                            / (node.acked_packets + node.dropped_packets + 1) as f64
                    } else {
                        1.0
                    }
                };
                let b_pdr = {
                    if let Some(node) = self.node_data.get(&e.1) {
                        node.dropped_packets as f64
                            / (node.acked_packets + node.dropped_packets + 1) as f64
                    } else {
                        1.0
                    }
                };
                a_pdr + b_pdr
            },
            |_| 0f64,
        ) {
            info!(target: format!("Node {}", self.node_id).as_str(), "Generated SRH {:?}", path.1.clone());
            Some(SourceRoutingHeader::new(path.1, 1))
        } else {
            None
        }
    }
    pub fn report_packet_ack(&mut self, route: &SourceRoutingHeader) {
        route.hops.iter().for_each(|id| {
            if let Some(node) = self.node_data.get_mut(id) {
                node.acked_packets += 1;
            }
        });
    }
    pub fn report_packet_drop(&mut self, node_id: NodeId) {
        if let Some(node) = self.node_data.get_mut(&node_id) {
            node.dropped_packets += 1;
        }
    }
    pub fn remove_node(&mut self, node_id: NodeId) {
        self.node_data.remove(&node_id);
        self.topology_graph.remove_node(node_id);
    }
    pub fn generate_flood_requests(&mut self, neighbors: Vec<NodeId>) -> Vec<(Packet, NodeId)> {
        let mut packets = Vec::new();
        for neighbor in &neighbors {
            let header = SourceRoutingHeader::new(vec![self.node_id, *neighbor], 1);
            packets.push((
                Packet::new_flood_request(
                    header,
                    self.cur_flood_id,
                    FloodRequest::initialize(self.cur_flood_id, self.node_id, self.node_type),
                ),
                *neighbor,
            ));
        }
        self.cur_flood_id += 1;
        info!(target: format!("Node {}", self.node_id).as_str(),  "Generated flood requests for neighbors: {:?}: {:?}", neighbors, packets.clone());
        packets
    }
    pub fn handle_flood_response(
        &mut self,
        mut rh: SourceRoutingHeader,
        sid: u64,
        pkt: &mut FloodResponse,
    ) -> Vec<(Packet, NodeId)> {
        info!(target: format!("Node {}", self.node_id).as_str(),  "Handling flood response{pkt}");
        if let Some(first) = pkt.path_trace.first() {
            if first.0 == self.node_id {
                for ((id_a, typ_a), (id_b, typ_b)) in pkt.path_trace.iter().tuple_windows() {
                    self.node_data
                        .entry(*id_a)
                        .or_insert(NodeInfo {
                            id: *id_a,
                            dropped_packets: 0,
                            acked_packets: 0,
                            node_type: Some(*typ_a),
                        })
                        .node_type = Some(*typ_a);
                    self.node_data
                        .entry(*id_b)
                        .or_insert(NodeInfo {
                            id: *id_b,
                            dropped_packets: 0,
                            acked_packets: 0,
                            node_type: Some(*typ_b),
                        })
                        .node_type = Some(*typ_b);
                    if *typ_a == Drone && *typ_b == Drone {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Adding path {} <-> {}", id_a, id_b);
                        self.topology_graph.add_edge(*id_a, *id_b, 1.0);
                        self.topology_graph.add_edge(*id_b, *id_a, 1.0);
                    } else if *typ_a == Drone {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Adding path {} -> {}", id_a, id_b);
                        self.topology_graph.add_edge(*id_a, *id_b, 1.0);
                    } else if *typ_b == Drone {
                        info!(target: format!("Node {}", self.node_id).as_str(),  "Adding path {} -> {}", id_b, id_a);
                        self.topology_graph.add_edge(*id_b, *id_a, 1.0);
                    } else {
                        error!(target: format!("Node {}", self.node_id).as_str(),  "Trying to add connection between two clients/servers {} - {}", id_a, id_b);
                    }
                }
                trace!(target: format!("Node {}", self.node_id).as_str(),  "New topology: {:?}", self.topology_graph.all_edges());
                vec![]
            } else if let Some(nh) = rh.next_hop() {
                rh.increase_hop_index();
                let p = Packet::new_flood_response(rh.clone(), sid, pkt.clone());
                info!(target: format!("Node {}", self.node_id).as_str(),  "Sending flood response to {}: {}", nh, p);
                vec![(p, nh)]
            } else {
                error!(target: format!("Node {}", self.node_id).as_str(),  "No next hop!");
                vec![]
            }
        } else {
            error!(target: format!("Node {}", self.node_id).as_str(),  "Path trace is empty!");
            vec![]
        }
    }
    pub fn reset_topology(&mut self) {
        self.topology_graph.clear();
    }
    pub fn add_from_incoming_routing_header(&mut self, header: &SourceRoutingHeader) {
        let mut h = header.clone();
        h.reverse();
        for (a, b) in h.hops.iter().tuple_windows() {
            info!(target: format!("Node {}", self.node_id).as_str(),  "Adding path {} -> {}", a, b);
            self.topology_graph.add_edge(*a, *b, 1.0);
            self.node_data.entry(*a).or_insert(NodeInfo {
                id: *a,
                dropped_packets: 0,
                acked_packets: 0,
                node_type: None,
            });
            trace!(target: format!("Node {}", self.node_id).as_str(),  "New topology: {:?}", self.topology_graph.all_edges());
            // We do not know the type from a simple routing header, it will be temporarily set to
            // none and will be updated when a flood response is received
        }
    }
}
