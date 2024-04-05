#[cfg(any(test, feature = "test-support"))]
pub mod test;

use anyhow::{anyhow, Result};

use async_tungstenite::tungstenite::{error::Error as WebsocketError, http::StatusCode};
use collections::HashMap;
use futures::{future::LocalBoxFuture, FutureExt, Stream, StreamExt, TryFutureExt as _};
use gpui::{AnyModel, AnyWeakModel, AppContext, AsyncAppContext, Global, Model, Task, WeakModel};
use lazy_static::lazy_static;
use parking_lot::RwLock;
use postage::watch;
use rpc::proto::{AnyTypedEnvelope, EntityMessage, EnvelopedMessage, PeerId, RequestMessage};

use std::{
    any::TypeId,
    future::Future,
    marker::PhantomData,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use util::http::HttpClientWithUrl;
use util::ResultExt;

pub use rpc::*;

lazy_static! {
    pub static ref ADMIN_API_TOKEN: Option<String> = std::env::var("ZED_ADMIN_API_TOKEN")
        .ok()
        .and_then(|s| if s.is_empty() { None } else { Some(s) });
    pub static ref ZED_APP_PATH: Option<PathBuf> =
        std::env::var("ZED_APP_PATH").ok().map(PathBuf::from);
    pub static ref ZED_ALWAYS_ACTIVE: bool =
        std::env::var("ZED_ALWAYS_ACTIVE").map_or(false, |e| !e.is_empty());
}

pub const INITIAL_RECONNECTION_DELAY: Duration = Duration::from_millis(100);
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

struct GlobalClient(Arc<Client>);

impl Global for GlobalClient {}

pub struct Client {
    id: AtomicU64,
    peer: Arc<Peer>,
    http: Arc<HttpClientWithUrl>,
    state: RwLock<ClientState>,
}

#[derive(Error, Debug)]
pub enum EstablishConnectionError {
    #[error("upgrade required")]
    UpgradeRequired,
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
    #[error("{0}")]
    Http(#[from] util::http::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Websocket(#[from] async_tungstenite::tungstenite::http::Error),
}

impl From<WebsocketError> for EstablishConnectionError {
    fn from(error: WebsocketError) -> Self {
        if let WebsocketError::Http(response) = &error {
            match response.status() {
                StatusCode::UNAUTHORIZED => return EstablishConnectionError::Unauthorized,
                StatusCode::UPGRADE_REQUIRED => return EstablishConnectionError::UpgradeRequired,
                _ => {}
            }
        }
        EstablishConnectionError::Other(error.into())
    }
}

impl EstablishConnectionError {
    pub fn other(error: impl Into<anyhow::Error> + Send + Sync) -> Self {
        Self::Other(error.into())
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Status {
    SignedOut,
    UpgradeRequired,
    Authenticating,
    Connecting,
    ConnectionError,
    Connected {
        peer_id: PeerId,
        connection_id: ConnectionId,
    },
    ConnectionLost,
    Reauthenticating,
    Reconnecting,
    ReconnectionError {
        next_reconnection: Instant,
    },
}

impl Status {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn is_signed_out(&self) -> bool {
        matches!(self, Self::SignedOut | Self::UpgradeRequired)
    }
}

struct ClientState {
    status: (watch::Sender<Status>, watch::Receiver<Status>),
    entity_id_extractors: HashMap<TypeId, fn(&dyn AnyTypedEnvelope) -> u64>,
    _reconnect_task: Option<Task<()>>,
    entities_by_type_and_remote_id: HashMap<(TypeId, u64), WeakSubscriber>,
    models_by_message_type: HashMap<TypeId, AnyWeakModel>,
    entity_types_by_message_type: HashMap<TypeId, TypeId>,
    #[allow(clippy::type_complexity)]
    message_handlers: HashMap<
        TypeId,
        Arc<
            dyn Send
                + Sync
                + Fn(
                    AnyModel,
                    Box<dyn AnyTypedEnvelope>,
                    &Arc<Client>,
                    AsyncAppContext,
                ) -> LocalBoxFuture<'static, Result<()>>,
        >,
    >,
}

enum WeakSubscriber {
    Entity { handle: AnyWeakModel },
    Pending(Vec<Box<dyn AnyTypedEnvelope>>),
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            status: watch::channel_with(Status::SignedOut),
            entity_id_extractors: Default::default(),
            _reconnect_task: None,
            models_by_message_type: Default::default(),
            entities_by_type_and_remote_id: Default::default(),
            entity_types_by_message_type: Default::default(),
            message_handlers: Default::default(),
        }
    }
}

pub enum Subscription {
    Entity {
        client: Weak<Client>,
        id: (TypeId, u64),
    },
    Message {
        client: Weak<Client>,
        id: TypeId,
    },
}

impl Drop for Subscription {
    fn drop(&mut self) {
        match self {
            Subscription::Entity { client, id } => {
                if let Some(client) = client.upgrade() {
                    let mut state = client.state.write();
                    let _ = state.entities_by_type_and_remote_id.remove(id);
                }
            }
            Subscription::Message { client, id } => {
                if let Some(client) = client.upgrade() {
                    let mut state = client.state.write();
                    let _ = state.entity_types_by_message_type.remove(id);
                    let _ = state.message_handlers.remove(id);
                }
            }
        }
    }
}

pub struct PendingEntitySubscription<T: 'static> {
    client: Arc<Client>,
    remote_id: u64,
    _entity_type: PhantomData<T>,
    consumed: bool,
}

impl<T: 'static> PendingEntitySubscription<T> {
    pub fn set_model(mut self, model: &Model<T>, cx: &mut AsyncAppContext) -> Subscription {
        self.consumed = true;
        let mut state = self.client.state.write();
        let id = (TypeId::of::<T>(), self.remote_id);
        let Some(WeakSubscriber::Pending(messages)) =
            state.entities_by_type_and_remote_id.remove(&id)
        else {
            unreachable!()
        };

        state.entities_by_type_and_remote_id.insert(
            id,
            WeakSubscriber::Entity {
                handle: model.downgrade().into(),
            },
        );
        drop(state);
        for message in messages {
            self.client.handle_message(message, cx);
        }
        Subscription::Entity {
            client: Arc::downgrade(&self.client),
            id,
        }
    }
}

impl<T: 'static> Drop for PendingEntitySubscription<T> {
    fn drop(&mut self) {
        if !self.consumed {
            let mut state = self.client.state.write();
            if let Some(WeakSubscriber::Pending(messages)) = state
                .entities_by_type_and_remote_id
                .remove(&(TypeId::of::<T>(), self.remote_id))
            {
                for message in messages {
                    log::info!("unhandled message {}", message.payload_type_name());
                }
            }
        }
    }
}

impl Client {
    pub fn new(http: Arc<HttpClientWithUrl>) -> Arc<Self> {
        Arc::new(Self {
            id: AtomicU64::new(0),
            peer: Peer::new(0),
            http,
            state: Default::default(),
        })
    }

    pub fn id(&self) -> u64 {
        self.id.load(Ordering::SeqCst)
    }

    pub fn set_id(&self, id: u64) -> &Self {
        self.id.store(id, Ordering::SeqCst);
        self
    }

    pub fn http_client(&self) -> Arc<HttpClientWithUrl> {
        self.http.clone()
    }

    fn connection_id(&self) -> Result<ConnectionId> {
        if let Status::Connected { connection_id, .. } = *self.status().borrow() {
            Ok(connection_id)
        } else {
            Err(anyhow!("not connected"))
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn teardown(&self) {
        let mut state = self.state.write();
        state._reconnect_task.take();
        state.message_handlers.clear();
        state.models_by_message_type.clear();
        state.entities_by_type_and_remote_id.clear();
        state.entity_id_extractors.clear();
        self.peer.teardown();
    }

    pub fn global(cx: &AppContext) -> Arc<Self> {
        cx.global::<GlobalClient>().0.clone()
    }
    pub fn set_global(client: Arc<Client>, cx: &mut AppContext) {
        cx.set_global(GlobalClient(client))
    }

    pub fn peer_id(&self) -> Option<PeerId> {
        if let Status::Connected { peer_id, .. } = &*self.status().borrow() {
            Some(*peer_id)
        } else {
            None
        }
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.state.read().status.1.clone()
    }

    pub fn subscribe_to_entity<T>(
        self: &Arc<Self>,
        remote_id: u64,
    ) -> Result<PendingEntitySubscription<T>>
    where
        T: 'static,
    {
        let id = (TypeId::of::<T>(), remote_id);

        let mut state = self.state.write();
        if state.entities_by_type_and_remote_id.contains_key(&id) {
            return Err(anyhow!("already subscribed to entity"));
        }

        state
            .entities_by_type_and_remote_id
            .insert(id, WeakSubscriber::Pending(Default::default()));

        Ok(PendingEntitySubscription {
            client: self.clone(),
            remote_id,
            consumed: false,
            _entity_type: PhantomData,
        })
    }

    #[track_caller]
    pub fn add_message_handler<M, E, H, F>(
        self: &Arc<Self>,
        entity: WeakModel<E>,
        handler: H,
    ) -> Subscription
    where
        M: EnvelopedMessage,
        E: 'static,
        H: 'static
            + Sync
            + Fn(Model<E>, TypedEnvelope<M>, Arc<Self>, AsyncAppContext) -> F
            + Send
            + Sync,
        F: 'static + Future<Output = Result<()>>,
    {
        let message_type_id = TypeId::of::<M>();
        let mut state = self.state.write();
        state
            .models_by_message_type
            .insert(message_type_id, entity.into());

        let prev_handler = state.message_handlers.insert(
            message_type_id,
            Arc::new(move |subscriber, envelope, client, cx| {
                let subscriber = subscriber.downcast::<E>().unwrap();
                let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                handler(subscriber, *envelope, client.clone(), cx).boxed_local()
            }),
        );
        if prev_handler.is_some() {
            let location = std::panic::Location::caller();
            panic!(
                "{}:{} registered handler for the same message {} twice",
                location.file(),
                location.line(),
                std::any::type_name::<M>()
            );
        }

        Subscription::Message {
            client: Arc::downgrade(self),
            id: message_type_id,
        }
    }

    pub fn add_request_handler<M, E, H, F>(
        self: &Arc<Self>,
        model: WeakModel<E>,
        handler: H,
    ) -> Subscription
    where
        M: RequestMessage,
        E: 'static,
        H: 'static
            + Sync
            + Fn(Model<E>, TypedEnvelope<M>, Arc<Self>, AsyncAppContext) -> F
            + Send
            + Sync,
        F: 'static + Future<Output = Result<M::Response>>,
    {
        self.add_message_handler(model, move |handle, envelope, this, cx| {
            Self::respond_to_request(
                envelope.receipt(),
                handler(handle, envelope, this.clone(), cx),
                this,
            )
        })
    }

    pub fn add_model_message_handler<M, E, H, F>(self: &Arc<Self>, handler: H)
    where
        M: EntityMessage,
        E: 'static,
        H: 'static + Fn(Model<E>, TypedEnvelope<M>, Arc<Self>, AsyncAppContext) -> F + Send + Sync,
        F: 'static + Future<Output = Result<()>>,
    {
        self.add_entity_message_handler::<M, E, _, _>(move |subscriber, message, client, cx| {
            handler(subscriber.downcast::<E>().unwrap(), message, client, cx)
        })
    }

    fn add_entity_message_handler<M, E, H, F>(self: &Arc<Self>, handler: H)
    where
        M: EntityMessage,
        E: 'static,
        H: 'static + Fn(AnyModel, TypedEnvelope<M>, Arc<Self>, AsyncAppContext) -> F + Send + Sync,
        F: 'static + Future<Output = Result<()>>,
    {
        let model_type_id = TypeId::of::<E>();
        let message_type_id = TypeId::of::<M>();

        let mut state = self.state.write();
        state
            .entity_types_by_message_type
            .insert(message_type_id, model_type_id);
        state
            .entity_id_extractors
            .entry(message_type_id)
            .or_insert_with(|| {
                |envelope| {
                    envelope
                        .as_any()
                        .downcast_ref::<TypedEnvelope<M>>()
                        .unwrap()
                        .payload
                        .remote_entity_id()
                }
            });
        let prev_handler = state.message_handlers.insert(
            message_type_id,
            Arc::new(move |handle, envelope, client, cx| {
                let envelope = envelope.into_any().downcast::<TypedEnvelope<M>>().unwrap();
                handler(handle, *envelope, client.clone(), cx).boxed_local()
            }),
        );
        if prev_handler.is_some() {
            panic!("registered handler for the same message twice");
        }
    }

    pub fn add_model_request_handler<M, E, H, F>(self: &Arc<Self>, handler: H)
    where
        M: EntityMessage + RequestMessage,
        E: 'static,
        H: 'static + Fn(Model<E>, TypedEnvelope<M>, Arc<Self>, AsyncAppContext) -> F + Send + Sync,
        F: 'static + Future<Output = Result<M::Response>>,
    {
        self.add_model_message_handler(move |entity, envelope, client, cx| {
            Self::respond_to_request::<M, _>(
                envelope.receipt(),
                handler(entity, envelope, client.clone(), cx),
                client,
            )
        })
    }

    async fn respond_to_request<T: RequestMessage, F: Future<Output = Result<T::Response>>>(
        receipt: Receipt<T>,
        response: F,
        client: Arc<Self>,
    ) -> Result<()> {
        match response.await {
            Ok(response) => {
                client.respond(receipt, response)?;
                Ok(())
            }
            Err(error) => {
                client.respond_with_error(receipt, error.to_proto())?;
                Err(error)
            }
        }
    }

    pub fn send<T: EnvelopedMessage>(&self, message: T) -> Result<()> {
        log::debug!("rpc send. client_id:{}, name:{}", self.id(), T::NAME);
        self.peer.send(self.connection_id()?, message)
    }

    pub fn request<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> {
        self.request_envelope(request)
            .map_ok(|envelope| envelope.payload)
    }

    pub fn request_stream<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<impl Stream<Item = Result<T::Response>>>> {
        let client_id = self.id.load(Ordering::SeqCst);
        log::debug!(
            "rpc request start. client_id:{}. name:{}",
            client_id,
            T::NAME
        );
        let response = self
            .connection_id()
            .map(|conn_id| self.peer.request_stream(conn_id, request));
        async move {
            let response = response?.await;
            log::debug!(
                "rpc request finish. client_id:{}. name:{}",
                client_id,
                T::NAME
            );
            response
        }
    }

    pub fn request_envelope<T: RequestMessage>(
        &self,
        request: T,
    ) -> impl Future<Output = Result<TypedEnvelope<T::Response>>> {
        let client_id = self.id();
        log::debug!(
            "rpc request start. client_id:{}. name:{}",
            client_id,
            T::NAME
        );
        let response = self
            .connection_id()
            .map(|conn_id| self.peer.request_envelope(conn_id, request));
        async move {
            let response = response?.await;
            log::debug!(
                "rpc request finish. client_id:{}. name:{}",
                client_id,
                T::NAME
            );
            response
        }
    }

    fn respond<T: RequestMessage>(&self, receipt: Receipt<T>, response: T::Response) -> Result<()> {
        log::debug!("rpc respond. client_id:{}. name:{}", self.id(), T::NAME);
        self.peer.respond(receipt, response)
    }

    fn respond_with_error<T: RequestMessage>(
        &self,
        receipt: Receipt<T>,
        error: proto::Error,
    ) -> Result<()> {
        log::debug!("rpc respond. client_id:{}. name:{}", self.id(), T::NAME);
        self.peer.respond_with_error(receipt, error)
    }

    fn handle_message(
        self: &Arc<Client>,
        message: Box<dyn AnyTypedEnvelope>,
        cx: &AsyncAppContext,
    ) {
        let mut state = self.state.write();
        let type_name = message.payload_type_name();
        let payload_type_id = message.payload_type_id();
        let sender_id = message.original_sender_id();

        let mut subscriber = None;

        if let Some(handle) = state
            .models_by_message_type
            .get(&payload_type_id)
            .and_then(|handle| handle.upgrade())
        {
            subscriber = Some(handle);
        } else if let Some((extract_entity_id, entity_type_id)) =
            state.entity_id_extractors.get(&payload_type_id).zip(
                state
                    .entity_types_by_message_type
                    .get(&payload_type_id)
                    .copied(),
            )
        {
            let entity_id = (extract_entity_id)(message.as_ref());

            match state
                .entities_by_type_and_remote_id
                .get_mut(&(entity_type_id, entity_id))
            {
                Some(WeakSubscriber::Pending(pending)) => {
                    pending.push(message);
                    return;
                }
                Some(weak_subscriber) => match weak_subscriber {
                    WeakSubscriber::Entity { handle } => {
                        subscriber = handle.upgrade();
                    }

                    WeakSubscriber::Pending(_) => {}
                },
                _ => {}
            }
        }

        let subscriber = if let Some(subscriber) = subscriber {
            subscriber
        } else {
            log::info!("unhandled message {}", type_name);
            self.peer.respond_with_unhandled_message(message).log_err();
            return;
        };

        let handler = state.message_handlers.get(&payload_type_id).cloned();
        // Dropping the state prevents deadlocks if the handler interacts with rpc::Client.
        // It also ensures we don't hold the lock while yielding back to the executor, as
        // that might cause the executor thread driving this future to block indefinitely.
        drop(state);

        if let Some(handler) = handler {
            let future = handler(subscriber, message, self, cx.clone());
            let client_id = self.id();
            log::debug!(
                "rpc message received. client_id:{}, sender_id:{:?}, type:{}",
                client_id,
                sender_id,
                type_name
            );
            cx.spawn(move |_| async move {
                    match future.await {
                        Ok(()) => {
                            log::debug!(
                                "rpc message handled. client_id:{}, sender_id:{:?}, type:{}",
                                client_id,
                                sender_id,
                                type_name
                            );
                        }
                        Err(error) => {
                            log::error!(
                                "error handling message. client_id:{}, sender_id:{:?}, type:{}, error:{:?}",
                                client_id,
                                sender_id,
                                type_name,
                                error
                            );
                        }
                    }
                })
                .detach();
        } else {
            log::info!("unhandled message {}", type_name);
            self.peer.respond_with_unhandled_message(message).log_err();
        }
    }
}

/// prefix for the zed:// url scheme
pub static ZED_URL_SCHEME: &str = "zed";

/// Parses the given link into a Zed link.
///
/// Returns a [`Some`] containing the unprefixed link if the link is a Zed link.
/// Returns [`None`] otherwise.
pub fn parse_zed_link<'a>(link: &'a str) -> Option<&'a str> {
    if let Some(stripped) = link
        .strip_prefix(ZED_URL_SCHEME)
        .and_then(|result| result.strip_prefix("://"))
    {
        return Some(stripped);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::FakeServer;

    use gpui::{Context, TestAppContext};

    use util::http::FakeHttpClient;

    #[gpui::test]
    async fn test_subscribing_to_entity(cx: &mut TestAppContext) {
        let client = Client::new(FakeHttpClient::with_404_response());
        let server = FakeServer::for_client(cx).await;

        let (done_tx1, mut done_rx1) = smol::channel::unbounded();
        let (done_tx2, mut done_rx2) = smol::channel::unbounded();
        client.add_model_message_handler(
            move |model: Model<TestModel>, _: TypedEnvelope<proto::JoinProject>, _, mut cx| {
                match model.update(&mut cx, |model, _| model.id).unwrap() {
                    1 => done_tx1.try_send(()).unwrap(),
                    2 => done_tx2.try_send(()).unwrap(),
                    _ => unreachable!(),
                }
                async { Ok(()) }
            },
        );
        let model1 = cx.new_model(|_| TestModel {
            id: 1,
            subscription: None,
        });
        let model2 = cx.new_model(|_| TestModel {
            id: 2,
            subscription: None,
        });
        let model3 = cx.new_model(|_| TestModel {
            id: 3,
            subscription: None,
        });

        let _subscription1 = client
            .subscribe_to_entity(1)
            .unwrap()
            .set_model(&model1, &mut cx.to_async());
        let _subscription2 = client
            .subscribe_to_entity(2)
            .unwrap()
            .set_model(&model2, &mut cx.to_async());
        // Ensure dropping a subscription for the same entity type still allows receiving of
        // messages for other entity IDs of the same type.
        let subscription3 = client
            .subscribe_to_entity(3)
            .unwrap()
            .set_model(&model3, &mut cx.to_async());
        drop(subscription3);

        server.send(proto::JoinProject { project_id: 1 });
        server.send(proto::JoinProject { project_id: 2 });
        done_rx1.next().await.unwrap();
        done_rx2.next().await.unwrap();
    }

    #[gpui::test]
    async fn test_subscribing_after_dropping_subscription(cx: &mut TestAppContext) {
        let client = Client::new(FakeHttpClient::with_404_response());
        let server = FakeServer::for_client(cx).await;

        let model = cx.new_model(|_| TestModel::default());
        let (done_tx1, _done_rx1) = smol::channel::unbounded();
        let (done_tx2, mut done_rx2) = smol::channel::unbounded();
        let subscription1 = client.add_message_handler(
            model.downgrade(),
            move |_, _: TypedEnvelope<proto::Ping>, _, _| {
                done_tx1.try_send(()).unwrap();
                async { Ok(()) }
            },
        );
        drop(subscription1);
        let _subscription2 = client.add_message_handler(
            model.downgrade(),
            move |_, _: TypedEnvelope<proto::Ping>, _, _| {
                done_tx2.try_send(()).unwrap();
                async { Ok(()) }
            },
        );
        server.send(proto::Ping {});
        done_rx2.next().await.unwrap();
    }

    #[gpui::test]
    async fn test_dropping_subscription_in_handler(cx: &mut TestAppContext) {
        let client = Client::new(FakeHttpClient::with_404_response());
        let server = FakeServer::for_client(cx).await;

        let model = cx.new_model(|_| TestModel::default());
        let (done_tx, mut done_rx) = smol::channel::unbounded();
        let subscription = client.add_message_handler(
            model.clone().downgrade(),
            move |model: Model<TestModel>, _: TypedEnvelope<proto::Ping>, _, mut cx| {
                model
                    .update(&mut cx, |model, _| model.subscription.take())
                    .unwrap();
                done_tx.try_send(()).unwrap();
                async { Ok(()) }
            },
        );
        model.update(cx, |model, _| {
            model.subscription = Some(subscription);
        });
        server.send(proto::Ping {});
        done_rx.next().await.unwrap();
    }

    #[derive(Default)]
    struct TestModel {
        id: usize,
        subscription: Option<Subscription>,
    }
}
