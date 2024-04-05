use anyhow::{anyhow, Result};
use futures::{stream::BoxStream, StreamExt};
use gpui::{BackgroundExecutor, TestAppContext};
use parking_lot::Mutex;
use rpc::{
    proto::{self, GetPrivateUserInfo, GetPrivateUserInfoResponse},
    ConnectionId, Peer, Receipt, TypedEnvelope,
};
use std::sync::Arc;

pub struct FakeServer {
    peer: Arc<Peer>,
    state: Arc<Mutex<FakeServerState>>,
    executor: BackgroundExecutor,
}

#[derive(Default)]
struct FakeServerState {
    incoming: Option<BoxStream<'static, Box<dyn proto::AnyTypedEnvelope>>>,
    connection_id: Option<ConnectionId>,
    forbid_connections: bool,
    auth_count: usize,
    access_token: usize,
}

impl FakeServer {
    pub async fn for_client(cx: &TestAppContext) -> Self {
        let server = Self {
            peer: Peer::new(0),
            state: Default::default(),
            executor: cx.executor(),
        };

        server
    }

    pub fn disconnect(&self) {
        if self.state.lock().connection_id.is_some() {
            self.peer.disconnect(self.connection_id());
            let mut state = self.state.lock();
            state.connection_id.take();
            state.incoming.take();
        }
    }

    pub fn auth_count(&self) -> usize {
        self.state.lock().auth_count
    }

    pub fn roll_access_token(&self) {
        self.state.lock().access_token += 1;
    }

    pub fn forbid_connections(&self) {
        self.state.lock().forbid_connections = true;
    }

    pub fn allow_connections(&self) {
        self.state.lock().forbid_connections = false;
    }

    pub fn send<T: proto::EnvelopedMessage>(&self, message: T) {
        self.peer.send(self.connection_id(), message).unwrap();
    }

    #[allow(clippy::await_holding_lock)]
    pub async fn receive<M: proto::EnvelopedMessage>(&self) -> Result<TypedEnvelope<M>> {
        self.executor.start_waiting();

        loop {
            let message = self
                .state
                .lock()
                .incoming
                .as_mut()
                .expect("not connected")
                .next()
                .await
                .ok_or_else(|| anyhow!("other half hung up"))?;
            self.executor.finish_waiting();
            let type_name = message.payload_type_name();
            let message = message.into_any();

            if message.is::<TypedEnvelope<M>>() {
                return Ok(*message.downcast().unwrap());
            }

            if message.is::<TypedEnvelope<GetPrivateUserInfo>>() {
                self.respond(
                    message
                        .downcast::<TypedEnvelope<GetPrivateUserInfo>>()
                        .unwrap()
                        .receipt(),
                    GetPrivateUserInfoResponse {
                        metrics_id: "the-metrics-id".into(),
                        staff: false,
                        flags: Default::default(),
                    },
                );
                continue;
            }

            panic!(
                "fake server received unexpected message type: {:?}",
                type_name
            );
        }
    }

    pub fn respond<T: proto::RequestMessage>(&self, receipt: Receipt<T>, response: T::Response) {
        self.peer.respond(receipt, response).unwrap()
    }

    fn connection_id(&self) -> ConnectionId {
        self.state.lock().connection_id.expect("not connected")
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.disconnect();
    }
}
