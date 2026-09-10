use super::*;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn receives_icmp_for_empty_and_short_datagrams_then_regular_data() {
    let socket = UdpSocketConfig::new()
        .recv_err()
        .bind("127.0.0.1:0".parse().unwrap())
        .unwrap();
    let sender = BaseUpdSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buffer = BytesMut::with_capacity(256);

    for payload in [&[][..], &[0x0a][..]] {
        let closed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = closed.local_addr().unwrap();
        drop(closed);
        socket.send_to(payload, destination).await.unwrap();
        let (peer, event) = timeout(Duration::from_secs(2), socket.recv_any(&mut buffer))
            .await
            .expect("no ICMP returned for the closed loopback port")
            .unwrap();
        assert_eq!(peer, destination);
        assert!(matches!(
            event,
            PacketType::Unreachable(UnreachableReason::Port)
        ));
        assert_eq!(buffer.split().as_ref(), payload);

        sender
            .send_to(b"next", socket.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, event) = timeout(Duration::from_secs(2), socket.recv_any(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer, sender.local_addr().unwrap());
        assert!(matches!(event, PacketType::Data));
        assert_eq!(buffer.split().as_ref(), b"next");
    }
}
