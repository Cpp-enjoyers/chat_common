use crate::messages::ChatMessage;
use prost::Message;
use wg_2024::packet::{Fragment, FRAGMENT_DSIZE};

/// # Panics
///
/// Cannot panic, since the buffer is created with the size of the message.
#[must_use] pub fn fragment(message: ChatMessage) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut buf: Vec<u8> = Vec::with_capacity(message.encoded_len());
    message.encode(&mut buf).unwrap();
    let message_data = buf.chunks(FRAGMENT_DSIZE);
    let total_n_fragments = message_data.len() as u64;
    for (fragment_index, fragment_data) in message_data.enumerate() {
        let mut data = [0; FRAGMENT_DSIZE];
        data[..fragment_data.len()].copy_from_slice(fragment_data);
        data[fragment_data.len()..].iter_mut().for_each(|b| *b = 0);
        fragments.push(Fragment {
            fragment_index: fragment_index as u64,
            total_n_fragments,
            length: fragment_data.len() as u8,
            data,
        });
    }
    fragments
}

pub fn defragment(fragments: &Vec<Fragment>) -> Result<ChatMessage, prost::DecodeError> {
    let mut message_data: Vec<u8> = Vec::new();
    for fragment in fragments {
        message_data.extend_from_slice(&fragment.data[..fragment.length as usize]);
    }
    ChatMessage::decode(&message_data[..])
}
