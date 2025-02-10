pub mod packet_handling;

pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
