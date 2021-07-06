use std::borrow::BorrowMut;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{BytesCodec, FramedRead, FramedWrite};

use crate::codec::*;
use crate::proto::Packet;

pub struct EncoderSink<S, E>
where
    S: Sink<Bytes, Error = E> + Unpin,
    Error: From<E>,
{
    sink: S,
}

impl<S, E> EncoderSink<S, E>
where
    S: Sink<Bytes, Error = E> + Unpin,
    Error: From<E>,
{
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<W> From<W> for EncoderSink<FramedWrite<W, BytesCodec>, std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    fn from(write: W) -> Self {
        Self::new(FramedWrite::new(write, BytesCodec::new()))
    }
}

impl<S, E> Sink<PacketKind> for EncoderSink<S, E>
where
    S: Sink<Bytes, Error = E> + Unpin,
    Error: From<E>,
{
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_ready(cx).map_err(Error::from)
    }

    fn start_send(mut self: Pin<&mut Self>, item: PacketKind) -> Result<(), Self::Error> {
        let mut dst = BytesMut::new();
        match item {
            PacketKind::Packet(pkt) => {
                reserve_and_encode(&mut dst, pkt.encoded_len())?;
                pkt.encode(&mut dst)?;
            }
            PacketKind::Forward(fwd) => {
                reserve_and_encode(&mut dst, fwd.encoded_len())?;
                fwd.encode(&mut dst);
            }
            PacketKind::ForwardCtd(buf) => {
                dst.reserve(buf.len());
                dst.extend(buf.into_iter())
            }
        }
        Pin::new(&mut self.sink)
            .start_send(dst.freeze())
            .map_err(Error::from)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_flush(cx).map_err(Error::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_close(cx).map_err(Error::from)
    }
}

#[inline]
fn reserve_and_encode(dst: &mut BytesMut, len: usize) -> Result<(), Error> {
    dst.reserve(prost::length_delimiter_len(len) + len);
    Ok(write_size(len as u32, dst)?)
}

pub struct DecoderStream<S, E>
where
    S: Stream<Item = Result<BytesMut, E>> + Unpin,
    Error: From<E>,
{
    stream: S,
    state: State,
    buf: BytesMut,
    limit: usize,
}

impl<S, E> DecoderStream<S, E>
where
    S: Stream<Item = Result<BytesMut, E>> + Unpin,
    Error: From<E>,
{
    pub fn new(stream: S) -> Self {
        Self::with(stream, MAX_PARSE_MESSAGE_SIZE)
    }

    pub fn with(stream: S, limit: usize) -> Self {
        Self {
            stream,
            state: State::AwaitingPrefix,
            buf: BytesMut::with_capacity(1024),
            limit,
        }
    }
}

impl<R> From<R> for DecoderStream<FramedRead<R, BytesCodec>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    fn from(read: R) -> Self {
        Self::new(FramedRead::new(read, BytesCodec::new()))
    }
}

impl<S, E> DecoderStream<S, E>
where
    S: Stream<Item = Result<BytesMut, E>> + Unpin,
    Error: From<E>,
{
    #[inline(always)]
    fn transition(&mut self, state: State) {
        let _ = std::mem::replace(&mut self.state, state);
    }

    #[inline(always)]
    fn maybe_wake(&mut self, cx: &mut Context<'_>) {
        if !self.buf.is_empty() {
            cx.waker().wake_by_ref();
        }
    }
}

impl<S, E> Stream for DecoderStream<S, E>
where
    S: Stream<Item = Result<BytesMut, E>> + Unpin,
    Error: From<E>,
{
    type Item = Result<PacketKind, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use DecodeError::*;

        if !self.buf.is_empty() {
            let max = self.limit;
            match std::mem::replace(self.state.borrow_mut(), State::Poisoned) {
                State::AwaitingPrefix => match read_bytes(&mut self.buf, max) {
                    Ok(Some(bytes)) => {
                        self.transition(State::AwaitingPrefix);
                        return match Packet::decode(bytes) {
                            Ok(pkt) => {
                                self.maybe_wake(cx);
                                Poll::Ready(Some(Ok(PacketKind::Packet(pkt))))
                            }
                            Err(err) => Poll::Ready(Some(Err(err.into()))),
                        };
                    }
                    Ok(None) | Err(PrefixTooShort) => {
                        self.transition(State::AwaitingPrefix);
                    }
                    Err(PayloadInvalid { left, .. }) | Err(PayloadTooLong { left, .. }) => {
                        self.transition(State::Discarding { left, read: 0 });
                    }
                    Err(PayloadTooShort { left, .. }) => {
                        self.transition(State::Buffering { left, read: 0 });
                    }
                    Err(Forward { bytes, left }) => {
                        if left > 0 {
                            self.transition(State::Forwarding { left, read: 0 });
                        } else {
                            self.transition(State::AwaitingPrefix);
                            self.maybe_wake(cx);
                        }
                        let fwd = crate::proto::Forward::decode(bytes).unwrap();
                        return Poll::Ready(Some(Ok(PacketKind::Forward(fwd))));
                    }
                    Err(e) => {
                        self.transition(State::AwaitingPrefix);
                        return Poll::Ready(Some(Err(e.into())));
                    }
                },
                State::Buffering { mut left, read } => {
                    left -= read;
                    if left == 0 {
                        self.transition(State::AwaitingPrefix);
                        return self.poll_next(cx);
                    }
                    self.transition(State::Buffering { left, read: 0 });
                }
                State::Forwarding { mut left, read } => {
                    left -= read;
                    if read > 0 {
                        let bytes = self.buf.split_to(read);

                        if left == 0 {
                            self.transition(State::AwaitingPrefix);
                            self.maybe_wake(cx);
                        } else {
                            self.transition(State::Forwarding { left, read: 0 });
                        }
                        return Poll::Ready(Some(Ok(PacketKind::ForwardCtd(bytes))));
                    }
                    self.transition(State::Forwarding { left, read: 0 });
                }
                State::Discarding { mut left, read } => {
                    left -= read;
                    if read > 0 {
                        let _ = self.buf.split_to(read);

                        if left == 0 {
                            self.transition(State::AwaitingPrefix);
                            return self.poll_next(cx);
                        }
                    }
                    self.transition(State::Discarding { left, read: 0 });
                }
                State::Poisoned => panic!("Programming error: no proper state set"),
            }
        }

        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if bytes.is_empty() {
                    return Poll::Ready(None);
                }
                self.state.read(bytes.len());
                self.buf.extend(bytes);
                self.poll_next(cx)
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

enum State {
    AwaitingPrefix,
    /// Buffering a (small) message to parse
    Buffering {
        left: usize,
        read: usize,
    },
    /// Remaining bytes to forward
    Forwarding {
        left: usize,
        read: usize,
    },
    /// Remaining bytes to discard, e.g. due to an invalid protobuf message
    Discarding {
        left: usize,
        read: usize,
    },
    Poisoned,
}

impl State {
    fn read(&mut self, count: usize) {
        match self {
            Self::Buffering { left, read }
            | Self::Forwarding { left, read }
            | Self::Discarding { left, read } => {
                *read = (*left).min(*read + count);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::iter::FromIterator;

    use bytes::BytesMut;
    use futures::{SinkExt, StreamExt};

    use crate::codec::stream::{DecoderStream, EncoderSink};
    use crate::codec::*;
    use crate::proto::*;

    const SESSION_ID: [u8; SESSION_ID_SIZE] = [0x0f; SESSION_ID_SIZE];

    async fn forward(messages: Vec<PacketKind>, chunk_size: usize) -> Vec<PacketKind> {
        let count = messages.len();
        println!("{} messages of {} b chunk size", count, chunk_size);

        let (tx_full, rx_full) = futures::channel::mpsc::channel(1);
        let (tx_parts, rx_parts) = futures::channel::mpsc::channel(1);

        let sink = EncoderSink::new(tx_full);
        let mut stream = DecoderStream::new(rx_parts);

        tokio::task::spawn(async move {
            let _ = futures::stream::iter(messages.into_iter().map(Ok))
                .forward(sink)
                .await;
        });
        tokio::task::spawn(async move {
            let _ = rx_full
                .flat_map(|b| futures::stream::iter(b.into_iter()).chunks(chunk_size))
                .map(|v| Ok(Ok::<_, Error>(BytesMut::from_iter(v))))
                .forward(tx_parts.sink_map_err(Error::from))
                .await;
        });

        let mut collected = Vec::with_capacity(count);
        while let Some(Ok(pkt)) = stream.next().await {
            match pkt {
                PacketKind::Packet(pkt) => {
                    collected.push(PacketKind::Packet(pkt));
                }
                PacketKind::Forward(fwd) => {
                    collected.push(PacketKind::Forward(fwd));
                }
                // reassemble chunks
                PacketKind::ForwardCtd(bytes) => match collected.last_mut().unwrap() {
                    PacketKind::Forward(f) => f.payload.extend(bytes),
                    _ => panic!("Misplaced `Forward` continuation"),
                },
            }
        }
        collected
    }

    #[tokio::test]
    async fn receive_chunked() {
        let packets = vec![
            PacketKind::Packet(Packet {
                kind: Some(packet::Kind::Request(Request {
                    session_id: Vec::new(),
                    kind: Some(request::Kind::Session(request::Session {
                        challenge_resp: vec![0x0d, 0x0e, 0x0a, 0x0d, 0x0b, 0x0e, 0x0e, 0x0f],
                        node_id: vec![0x0c, 0x00, 0x0f, 0x0f, 0x0e, 0x0e],
                        public_key: vec![0x05, 0x0e, 0x0c],
                    })),
                })),
            }),
            PacketKind::Packet(Packet {
                kind: Some(packet::Kind::Request(Request {
                    session_id: SESSION_ID.to_vec(),
                    kind: Some(request::Kind::Register(request::Register {
                        endpoints: vec![Endpoint {
                            protocol: Protocol::Tcp as i32,
                            address: "1.2.3.4".to_string(),
                            port: 12345,
                        }],
                    })),
                })),
            }),
            PacketKind::Forward(Forward {
                session_id: SESSION_ID,
                slot: 42,
                payload: (0..8192).map(|_| rand::random::<u8>()).collect(),
            }),
            PacketKind::Packet(Packet {
                kind: Some(packet::Kind::Request(Request {
                    session_id: SESSION_ID.to_vec(),
                    kind: Some(request::Kind::RandomNode(request::RandomNode {
                        public_key: true,
                    })),
                })),
            }),
        ];

        assert_eq!(packets, forward(packets.clone(), 1).await);
        assert_eq!(packets, forward(packets.clone(), 2).await);
        assert_eq!(packets, forward(packets.clone(), 3).await);
        assert_eq!(packets, forward(packets.clone(), 4).await);
        assert_eq!(packets, forward(packets.clone(), 7).await);
        assert_eq!(packets, forward(packets.clone(), 8).await);
        assert_eq!(packets, forward(packets.clone(), 16).await);
        assert_eq!(packets, forward(packets.clone(), 32).await);
        assert_eq!(packets, forward(packets.clone(), 64).await);
    }

    #[tokio::test]
    async fn receive_chunked_and_skip() {
        let random_node = PacketKind::Packet(Packet {
            kind: Some(packet::Kind::Request(Request {
                session_id: SESSION_ID.to_vec(),
                kind: Some(request::Kind::RandomNode(request::RandomNode {
                    public_key: true,
                })),
            })),
        });

        let packets = vec![
            random_node.clone(),
            PacketKind::Packet(Packet {
                kind: Some(packet::Kind::Request(Request {
                    session_id: Vec::new(),
                    kind: Some(request::Kind::Session(request::Session {
                        challenge_resp: vec![0u8; MAX_PARSE_MESSAGE_SIZE],
                        node_id: vec![],
                        public_key: vec![],
                    })),
                })),
            }),
            random_node.clone(),
        ];
        let expected = vec![random_node.clone(), random_node.clone()];

        assert_eq!(expected, forward(packets.clone(), 5).await);
    }
}
