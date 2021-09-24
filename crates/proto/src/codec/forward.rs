use std::borrow::BorrowMut;
use std::mem::size_of;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BytesMut};
use futures::channel::mpsc;
use futures::{Sink, Stream};

use crate::codec::Error;
use crate::proto::Payload;

pub type ChannelSink = ForwardSink<mpsc::Sender<Vec<u8>>, mpsc::SendError>;
pub type ChannelStream = ForwardStream<mpsc::Receiver<Vec<u8>>>;

pub fn channel(buffer: usize) -> (ChannelSink, ChannelStream) {
    let (tx, rx) = mpsc::channel(buffer);
    (ForwardSink::new(tx), ForwardStream::new(rx))
}

pub struct ForwardSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
{
    sink: S,
}

impl<S, E> ForwardSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
    Error: From<E>,
{
    pub fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S, E> Clone for ForwardSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Clone + Unpin,
{
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
        }
    }
}

impl<S, E> Sink<Payload> for ForwardSink<S, E>
where
    S: Sink<Vec<u8>, Error = E> + Unpin,
{
    type Error = E;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sink).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Payload) -> Result<(), Self::Error> {
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

pub struct ForwardStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    stream: S,
    state: State,
    buf: BytesMut,
}

impl<S> ForwardStream<S>
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

impl<S> ForwardStream<S>
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

impl<S> Stream for ForwardStream<S>
where
    S: Stream<Item = Vec<u8>> + Unpin,
{
    type Item = Result<Payload, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.buf.is_empty() {
            match std::mem::replace(self.state.borrow_mut(), State::Poisoned) {
                State::AwaitingPrefix => match decode(&mut self.buf) {
                    Ok(payload) => {
                        self.transition(State::AwaitingPrefix);
                        self.maybe_wake(cx);
                        return Poll::Ready(Some(Ok(payload)));
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

fn encode(payload: Payload, buf: &mut Vec<u8>) {
    let len = payload.len() as u32;
    buf.reserve(PREFIX_SIZE + len as usize);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend(payload.freeze().into_iter().take(len as usize));
}

fn decode(buf: &mut BytesMut) -> Result<Payload, State> {
    if buf.len() >= PREFIX_SIZE {
        let mut length: [u8; 4] = [0u8; 4];
        length.copy_from_slice(&buf.as_ref()[0..4]);

        let length = u32::from_be_bytes(length) as usize;
        let prefixed_length = PREFIX_SIZE + length;

        if buf.len() >= prefixed_length as usize {
            buf.advance(PREFIX_SIZE);
            let bytes = buf.split_to(length);
            Ok(bytes.into())
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
    use futures::StreamExt;
    use rand::Rng;

    use crate::codec::forward::{ForwardSink, ForwardStream};
    use crate::proto::*;

    async fn forward(messages: Vec<Vec<u8>>, chunk_size: usize) -> Vec<Payload> {
        let count = messages.len();
        println!("{} messages of {} b chunk size", count, chunk_size);

        let (tx_full, rx_full) = futures::channel::mpsc::channel(1);
        let (tx_parts, rx_parts) = futures::channel::mpsc::channel(1);

        let sink = ForwardSink::new(tx_full);
        let mut stream = ForwardStream::new(rx_parts);

        tokio::task::spawn(async move {
            let _ = futures::stream::iter(messages.into_iter().map(|v| Ok(v.into())))
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
