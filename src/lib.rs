mod fragment;
mod packethandling;
mod routing;

pub mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}
