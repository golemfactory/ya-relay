use std::mem::size_of;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BytesMut};
use derive_more::From;
use futures::{Sink, Stream};

use crate::codec::Error;
use crate::proto::Payload;

#[derive(From)]
pub enum SinkKind<S, E>
where
    S: Sink<Payload, Error = E> + Unpin,
{
    Sink(S),
    Prefixed(PrefixedSink<S, E>),
}

impl<S, E> Clone for SinkKind<S, E>
where
    S: Sink<Payload, Error = E> + Clone + Unpin,
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
    S: Sink<Payload, Error = E> + Unpin,
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
            SinkKind::Sink(ref mut s) => Pin::new(s).start_send(item.into()),
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
    S: Sink<Payload, Error = E> + Unpin,
{
    sink: S,
}

impl<S, E> PrefixedSink<S, E>
where
    S: Sink<Payload, Error = E> + Unpin,
{
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S, E> Clone for PrefixedSink<S, E>
where
    S: Sink<Payload, Error = E> + Clone + Unpin,
{
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
        }
    }
}

impl<S, E, P> Sink<P> for PrefixedSink<S, E>
where
    S: Sink<Payload, Error = E> + Unpin,
    Payload: From<P>,
{
    type Error = E;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: P) -> Result<(), Self::Error> {
        let dst = encode(item);
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
    S: Stream<Item = Payload> + Unpin,
{
    stream: S,
    buf: BytesMut,
}

impl<S> PrefixedStream<S>
where
    S: Stream<Item = Payload> + Unpin,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(65535),
        }
    }
}

impl<S> PrefixedStream<S>
where
    S: Stream<Item = Payload> + Unpin,
{
    #[inline(always)]
    fn maybe_wake(&mut self, cx: &mut Context<'_>) {
        if !self.buf.is_empty() {
            cx.waker().wake_by_ref();
        }
    }
}

impl<S> Stream for PrefixedStream<S>
where
    S: Stream<Item = Payload> + Unpin,
{
    type Item = Result<BytesMut, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.buf.is_empty() {
            if let Ok(bytes) = decode(&mut self.buf) {
                self.maybe_wake(cx);
                return Poll::Ready(Some(Ok(bytes)));
            }
        }

        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Ready(Some(payload)) => {
                if payload.is_empty() {
                    return Poll::Ready(None);
                }
                self.buf.extend(payload);
                self.poll_next(cx)
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

const PREFIX_SIZE: usize = size_of::<u32>();

pub fn encode(data: impl Into<Payload>) -> Payload {
    // FIXME: handle Payload variants instead of converting to vec
    let mut payload = data.into();
    let len = payload.len() as u32;
    payload.reserve(PREFIX_SIZE);
    payload.prepend(&len.to_be_bytes());
    payload
}

#[allow(clippy::result_unit_err)]
pub fn decode(buf: &mut BytesMut) -> Result<BytesMut, ()> {
    if buf.len() < PREFIX_SIZE {
        return Err(());
    }

    let mut length: [u8; PREFIX_SIZE] = [0u8; PREFIX_SIZE];
    length.copy_from_slice(&buf.as_ref()[0..PREFIX_SIZE]);
    let length = u32::from_be_bytes(length) as usize;
    let prefixed_length = PREFIX_SIZE + length;

    if buf.len() >= prefixed_length {
        buf.advance(PREFIX_SIZE);
        let bytes = buf.split_to(length);
        Ok(bytes)
    } else {
        Err(())
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
        println!("{count} messages of {chunk_size} B chunk size");

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
                .flat_map(|b| futures::stream::iter(b.into_vec().into_iter()).chunks(chunk_size))
                .map(|v| Ok::<_, futures::channel::mpsc::SendError>(v.into()))
                .forward(tx_parts)
                .await;
        });

        let mut collected = Vec::with_capacity(count);
        while let Some(Ok(pkt)) = stream.next().await {
            collected.push(pkt.into());
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

        let mut packets: Vec<Vec<u8>> = vec![
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

        packets.push(gen_vec(65535));
        packets.push(gen_vec(65535 * 2));
        packets.push(gen_vec(65535));

        assert_eq!(packets, forward(packets.clone(), 7).await);
        assert_eq!(packets, forward(packets.clone(), 8).await);
        assert_eq!(packets, forward(packets.clone(), 16).await);
        assert_eq!(packets, forward(packets.clone(), 32).await);
        assert_eq!(packets, forward(packets.clone(), 64).await);
        assert_eq!(packets, forward(packets.clone(), 1023).await);
        assert_eq!(packets, forward(packets.clone(), 1024).await);
    }
}
