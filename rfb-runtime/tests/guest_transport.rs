#![cfg(target_os = "linux")]

use rfb_runtime::codec::{write_frame, FrameCodec, MessageType};
use rfb_runtime::guest_protocol::{
    read_runtime_message, read_runtime_message_with_sequence, write_control_message,
};
use rfb_runtime::session::{ControlMessage, RuntimeMessage};
use tokio::io::duplex;

#[tokio::test]
async fn guest_transport_uses_framed_control_and_runtime_messages() {
    let codec = FrameCodec::default();
    let (mut host, mut guest) = duplex(4096);
    let control = ControlMessage::Shutdown;

    write_control_message(&mut host, &codec, &control, 7)
        .await
        .unwrap();
    let (_, decoded): (_, ControlMessage) = rfb_runtime::codec::read_frame(&mut guest, &codec)
        .await
        .unwrap();
    assert_eq!(decoded, control);

    write_frame(
        &mut guest,
        &codec,
        MessageType::Shutdown,
        8,
        &RuntimeMessage::ShutdownAck,
    )
    .await
    .unwrap();
    assert_eq!(
        read_runtime_message(&mut host, &codec).await.unwrap(),
        RuntimeMessage::ShutdownAck
    );

    write_frame(
        &mut guest,
        &codec,
        MessageType::Shutdown,
        9,
        &RuntimeMessage::ShutdownAck,
    )
    .await
    .unwrap();
    assert_eq!(
        read_runtime_message_with_sequence(&mut host, &codec)
            .await
            .unwrap(),
        (9, RuntimeMessage::ShutdownAck)
    );
}
