use futures::future::LocalBoxFuture;
use url::Url;
use ya_client_model::NodeId;

pub trait AbstractServerWrapper<'a> {
    fn url(&self) -> Url;

    fn remove_node_endpoints(&'a self, node: NodeId) -> LocalBoxFuture<'a, ()>;
}
