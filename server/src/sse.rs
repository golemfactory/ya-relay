use std::sync::{Arc, Mutex};
use actix_web_lab::sse;
use actix_web_lab::sse::Event;
use futures_util::SinkExt;
use log::{info, log};
use tokio::sync::mpsc;
use tokio::sync::mpsc::{channel, Receiver};
use tokio_stream::wrappers::ReceiverStream;
use serde::Serialize;

#[derive(Serialize)]
pub struct SseMessage {
    pub(crate) status: String,
    pub(crate) node: NodeInfo,
}

#[derive(Serialize)]
pub struct NodeInfo {
    pub(crate) id: String,
    pub(crate) peer: String,
    pub(crate) seen: String,
    #[serde(rename = "addrStatus")]
    pub(crate) addr_status: String,
}

#[derive(Debug, Clone, Default)]
pub struct SseClients{
    clients: Arc<Mutex<Vec<mpsc::Sender<sse::Event>>>>
}

impl SseClients{
    pub fn new() -> Self{
        SseClients{
            clients: Arc::new(Mutex::new(Vec::new())),
        }
    }



    pub async fn add_client(&self) -> ReceiverStream<sse::Event> {
        let (tx, rx) = mpsc::channel(10);

        // Send a "connected" message to the new client
        tx.send(sse::Data::new("connected").into()).await.unwrap();

        // Add the sender to the list of clients
        self.clients.lock().unwrap().push(tx);
        info!("New SSE connection established");

        // Return the receiver stream to be used for SSE
        ReceiverStream::new(rx)
    }

    pub fn get_no_of_clients(&self) -> usize{
        self.clients.lock().unwrap().len()
    }

    pub async fn broadcast(&self, msg:&str){
        let clients = self.clients.lock().unwrap().clone();
        let send_futures = clients.iter().map(|client| client.send(sse::Data::new(msg).into()));
        let _ = futures_util::future::join_all(send_futures).await;
    }
}
