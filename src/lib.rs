pub mod fragment;
pub mod packet_handling;
pub mod routing;

pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
