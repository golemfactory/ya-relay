use std::mem::size_of;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{future, Sink, SinkExt, Stream};

use crate::codec::Error;
use crate::proto::Payload;


pub fn prefixed_sink<S, E>(sink : S) -> impl Sink<Bytes> where S: Sink<Bytes, Error = E> + Unpin {
    sink.with(|it| future::ok::<_, E>(encode(it)))
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

pub fn encode(data: bytes::Bytes) -> bytes::Bytes {
    let len = data.len() as u32;
    let mut output = bytes::BytesMut::with_capacity(PREFIX_SIZE + data.len());

    output.put_u32(len);
    output.put_slice(&data);

    output.freeze()
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
