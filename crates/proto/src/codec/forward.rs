use std::borrow::BorrowMut;
use std::mem::size_of;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use derive_more::From;
use futures::{Sink, Stream};

use crate::codec::Error;
use crate::proto::Payload;

#[derive(From)]
pub enum SinkKind<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
{
    Sink(S),
    Prefixed(PrefixedSink<S, E>),
}

impl<S, E> Clone for SinkKind<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Clone + Unpin,
{
    fn clone(&self) -> Self {
        match self {
            Self::Sink(s) => Self::Sink(s.clone()),
            Self::Prefixed(s) => Self::Prefixed(s.clone()),
        }
    }
}

impl<S, E, P> Sink<P> for SinkKind<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
    Payload: From<P>,
{
    type Error = E;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            SinkKind::Sink(ref mut s) => Pin::new(s).poll_ready(cx),
            SinkKind::Prefixed(ref mut s) => Pin::new(s).poll_ready(cx),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: P) -> Result<(), Self::Error> {
        match self.get_mut() {
            SinkKind::Sink(ref mut s) => Pin::new(s).start_send(Payload::from(item).into_vec()),
            SinkKind::Prefixed(ref mut s) => Pin::new(s).start_send(item),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            SinkKind::Sink(ref mut s) => Pin::new(s).poll_flush(cx),
            SinkKind::Prefixed(ref mut s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            SinkKind::Sink(s) => Pin::new(s).poll_close(cx),
            SinkKind::Prefixed(s) => Pin::new(s).poll_close(cx),
        }
    }
}

pub struct PrefixedSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
{
    sink: S,
}

impl<S, E> PrefixedSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
{
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S, E> Clone for PrefixedSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Clone + Unpin,
{
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
        }
    }
}

impl<S, E, P> Sink<P> for PrefixedSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
    Payload: From<P>,
{
    type Error = E;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: P) -> Result<(), Self::Error> {
        let mut dst = Vec::new();
        encode(item, &mut dst);
        Pin::new(&mut self.sink).start_send(dst)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_close(cx)
    }
}

pub struct PrefixedStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    stream: S,
    state: State,
    buf: BytesMut,
}

impl<S> PrefixedStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            state: State::AwaitingPrefix,
            buf: BytesMut::with_capacity(1024),
        }
    }
}

impl<S> PrefixedStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
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

impl<S> Stream for PrefixedStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.buf.is_empty() {
            match std::mem::replace(self.state.borrow_mut(), State::Poisoned) {
                State::AwaitingPrefix => match decode(&mut self.buf) {
                    Ok(bytes) => {
                        self.transition(State::AwaitingPrefix);
                        self.maybe_wake(cx);
                        return Poll::Ready(Some(Ok(bytes)));
                    }
                    Err(state) => {
                        self.transition(state);
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
                State::Poisoned => panic!("Programming error: no proper state set"),
            }
        }

        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(bytes)) => {
                if bytes.is_empty() {
                    return Poll::Ready(None);
                }
                self.state.read(bytes.len());
                self.buf.extend(bytes);
                self.poll_next(cx)
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

const PREFIX_SIZE: usize = size_of::<u32>();

fn encode(payload: impl Into<Payload>, buf: &mut Vec<u8>) {
    let payload = payload.into();
    let len = payload.len() as u32;
    buf.reserve(PREFIX_SIZE + len as usize);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend(payload.freeze().into_iter().take(len as usize));
}

fn decode(buf: &mut BytesMut) -> Result<Bytes, State> {
    if buf.len() >= PREFIX_SIZE {
        let mut length: [u8; 4] = [0u8; 4];
        length.copy_from_slice(&buf.as_ref()[0..4]);

        let length = u32::from_be_bytes(length) as usize;
        let prefixed_length = PREFIX_SIZE + length;

        if buf.len() >= prefixed_length as usize {
            buf.advance(PREFIX_SIZE);
            let bytes = buf.split_to(length);
            Ok(bytes.freeze())
        } else {
            let left = prefixed_length - buf.len();
            Err(State::Buffering { left, read: 0 })
        }
    } else {
        Err(State::AwaitingPrefix)
    }
}

#[derive(Copy, Clone, Debug)]
enum State {
    AwaitingPrefix,
    Buffering { left: usize, read: usize },
    Poisoned,
}

impl State {
    fn read(&mut self, count: usize) {
        if let Self::Buffering { left, read } = self {
            *read = (*left).min(*read + count);
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::StreamExt;
    use rand::Rng;

    use super::Payload;
    use crate::codec::forward::{PrefixedSink, PrefixedStream};

    async fn forward(messages: Vec<Vec<u8>>, chunk_size: usize) -> Vec<Bytes> {
        let count = messages.len();
        println!("{} messages of {} b chunk size", count, chunk_size);

        let (tx_full, rx_full) = futures::channel::mpsc::channel(1);
        let (tx_parts, rx_parts) = futures::channel::mpsc::channel(1);

        let sink = PrefixedSink::new(tx_full);
        let mut stream = PrefixedStream::new(rx_parts);

        tokio::task::spawn(async move {
            let _ = futures::stream::iter(messages.into_iter().map(|v| Ok(Payload::from(v))))
                .forward(sink)
                .await;
        });
        tokio::task::spawn(async move {
            let _ = rx_full
                .flat_map(|b| futures::stream::iter(b.into_iter()).chunks(chunk_size))
                .map(|v| Ok::<_, futures::channel::mpsc::SendError>(v))
                .forward(tx_parts)
                .await;
        });

        let mut collected = Vec::with_capacity(count);
        while let Some(Ok(pkt)) = stream.next().await {
            collected.push(pkt);
        }
        collected
    }

    #[tokio::test]
    async fn forward_chunked() {
        fn gen_vec<T>(max: usize) -> Vec<T>
        where
            rand::distributions::Standard: rand::distributions::Distribution<T>,
        {
            let mut rng = rand::thread_rng();
            (0..max).map(|_| rng.gen::<T>()).collect()
        }

        let packets: Vec<Vec<u8>> = vec![
            gen_vec(11),
            gen_vec(100),
            gen_vec(1),
            gen_vec(2),
            gen_vec(3),
            gen_vec(4),
            gen_vec(7),
            gen_vec(8),
            gen_vec(16),
            gen_vec(31),
            gen_vec(32),
            gen_vec(63),
            gen_vec(64),
            gen_vec(256),
            gen_vec(1023),
            gen_vec(1024),
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
        assert_eq!(packets, forward(packets.clone(), 1023).await);
        assert_eq!(packets, forward(packets.clone(), 1024).await);
    }
}
