//! Two-node demo: two peers exchange framed messages over an in-memory
//! `mock_pair`, exercising the codec + frame path end to end.
//!
//! Run with:
//!   cargo run -p mlink-core --example two_nodes

use mlink_core::{
    decode_frame, encode_flags, encode_frame, mock_pair, Connection, Frame, MessageType, Result,
    MAGIC, PROTOCOL_VERSION,
};

fn build_frame(seq: u16, payload: Vec<u8>) -> Frame {
    let length = payload.len() as u16;
    Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, MessageType::Message),
        seq,
        length,
        payload,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let (mut node_a, mut node_b) = mock_pair();
    println!(
        "connected: a={} <-> b={}",
        node_a.peer_id(),
        node_b.peer_id()
    );

    // A -> B
    let outgoing = build_frame(1, b"hello from A".to_vec());
    node_a.write(&encode_frame(&outgoing)).await?;
    let bytes = node_b.read().await?;
    let received = decode_frame(&bytes)?;
    println!(
        "B received seq={} payload={:?}",
        received.seq,
        String::from_utf8_lossy(&received.payload)
    );
    assert_eq!(received.payload, outgoing.payload);

    // B -> A
    let reply = build_frame(2, b"hi back from B".to_vec());
    node_b.write(&encode_frame(&reply)).await?;
    let bytes = node_a.read().await?;
    let received = decode_frame(&bytes)?;
    println!(
        "A received seq={} payload={:?}",
        received.seq,
        String::from_utf8_lossy(&received.payload)
    );
    assert_eq!(received.payload, reply.payload);

    node_a.close().await?;
    node_b.close().await?;
    println!("done");
    Ok(())
}
