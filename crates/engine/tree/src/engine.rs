//! An engine API handler for the chain.

use crate::{
    backfill::BackfillAction,
    chain::{ChainHandler, FromOrchestrator, HandlerEvent},
    download::{BlockDownloader, DownloadAction, DownloadOutcome},
};
use futures::{Stream, StreamExt};
use reth_beacon_consensus::{BeaconConsensusEngineEvent, BeaconEngineMessage};
use reth_engine_primitives::EngineTypes;
use reth_primitives::{SealedBlockWithSenders, B256};
use std::{
    collections::HashSet,
    sync::mpsc::Sender,
    task::{ready, Context, Poll},
};
use tokio::sync::mpsc::UnboundedReceiver;

/// A [`ChainHandler`] that advances the chain based on incoming requests (CL engine API).
///
/// This is a general purpose request handler with network access.
/// This type listens for incoming messages and processes them via the configured request handler.
///
/// ## Overview
///
/// This type is an orchestrator for incoming messages and responsible for delegating requests
/// received from the CL to the handler.
///
/// It is responsible for handling the following:
/// - Delegating incoming requests to the [`EngineRequestHandler`].
/// - Advancing the [`EngineRequestHandler`] by polling it and emitting events.
/// - Downloading blocks on demand from the network if requested by the [`EngineApiRequestHandler`].
///
/// The core logic is part of the [`EngineRequestHandler`], which is responsible for processing the
/// incoming requests.
#[derive(Debug)]
pub struct EngineHandler<T, S, D> {
    /// Processes requests.
    ///
    /// This type is responsible for processing incoming requests.
    handler: T,
    /// Receiver for incoming requests (from the engine API endpoint) that need to be processed.
    incoming_requests: S,
    /// A downloader to download blocks on demand.
    downloader: D,
}

impl<T, S, D> EngineHandler<T, S, D> {
    /// Creates a new [`EngineHandler`] with the given handler and downloader and incoming stream of
    /// requests.
    pub const fn new(handler: T, downloader: D, incoming_requests: S) -> Self
    where
        T: EngineRequestHandler,
    {
        Self { handler, incoming_requests, downloader }
    }
}

impl<T, S, D> ChainHandler for EngineHandler<T, S, D>
where
    T: EngineRequestHandler,
    S: Stream<Item = T::Request> + Send + Sync + Unpin + 'static,
    D: BlockDownloader,
{
    type Event = T::Event;

    fn on_event(&mut self, event: FromOrchestrator) {
        // delegate event to the handler
        self.handler.on_event(event.into());
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<HandlerEvent<Self::Event>> {
        loop {
            // drain the handler first
            while let Poll::Ready(ev) = self.handler.poll(cx) {
                match ev {
                    RequestHandlerEvent::HandlerEvent(ev) => {
                        return match ev {
                            HandlerEvent::BackfillAction(target) => {
                                // bubble up backfill sync request request
                                self.downloader.on_action(DownloadAction::Clear);
                                Poll::Ready(HandlerEvent::BackfillAction(target))
                            }
                            HandlerEvent::Event(ev) => {
                                // bubble up the event
                                Poll::Ready(HandlerEvent::Event(ev))
                            }
                            HandlerEvent::FatalError => Poll::Ready(HandlerEvent::FatalError),
                        }
                    }
                    RequestHandlerEvent::Download(req) => {
                        // delegate download request to the downloader
                        self.downloader.on_action(DownloadAction::Download(req));
                    }
                }
            }

            // pop the next incoming request
            if let Poll::Ready(Some(req)) = self.incoming_requests.poll_next_unpin(cx) {
                // and delegate the request to the handler
                self.handler.on_event(FromEngine::Request(req));
                // skip downloading in this iteration to allow the handler to process the request
                continue
            }

            // advance the downloader
            if let Poll::Ready(DownloadOutcome::Blocks(blocks)) = self.downloader.poll(cx) {
                // delegate the downloaded blocks to the handler
                self.handler.on_event(FromEngine::DownloadedBlocks(blocks));
                continue
            }

            return Poll::Pending
        }
    }
}

/// A type that processes incoming requests (e.g. requests from the consensus layer, engine API,
/// such as newPayload).
///
/// ## Control flow
///
/// Requests and certain updates, such as a change in backfill sync status, are delegated to this
/// type via [`EngineRequestHandler::on_event`]. This type is responsible for processing the
/// incoming requests and advancing the chain and emit events when it is polled.
pub trait EngineRequestHandler: Send + Sync {
    /// Even type this handler can emit
    type Event: Send;
    /// The request type this handler can process.
    type Request;

    /// Informs the handler about an event from the [`EngineHandler`].
    fn on_event(&mut self, event: FromEngine<Self::Request>);

    /// Advances the handler.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<RequestHandlerEvent<Self::Event>>;
}

/// An [`EngineRequestHandler`] that processes engine API requests by delegating to an execution
/// task.
///
/// This type is responsible for advancing the chain during live sync (following the tip of the
/// chain).
///
/// It advances the chain based on received engine API requests by delegating them to the tree
/// executor.
///
/// There are two types of requests that can be processed:
///
/// - `on_new_payload`: Executes the payload and inserts it into the tree. These are allowed to be
///   processed concurrently.
/// - `on_forkchoice_updated`: Updates the fork choice based on the new head. These require write
///   access to the database and are skipped if the handler can't acquire exclusive access to the
///   database.
///
/// In case required blocks are missing, the handler will request them from the network, by emitting
/// a download request upstream.
#[derive(Debug)]
pub struct EngineApiRequestHandler<T: EngineTypes> {
    /// channel to send messages to the tree to execute the payload.
    to_tree: Sender<FromEngine<BeaconEngineMessage<T>>>,
    /// channel to receive messages from the tree.
    from_tree: UnboundedReceiver<EngineApiEvent>,
}

impl<T> EngineApiRequestHandler<T>
where
    T: EngineTypes,
{
    /// Creates a new `EngineApiRequestHandler`.
    pub const fn new(
        to_tree: Sender<FromEngine<BeaconEngineMessage<T>>>,
        from_tree: UnboundedReceiver<EngineApiEvent>,
    ) -> Self {
        Self { to_tree, from_tree }
    }
}

impl<T> EngineRequestHandler for EngineApiRequestHandler<T>
where
    T: EngineTypes,
{
    type Event = BeaconConsensusEngineEvent;
    type Request = BeaconEngineMessage<T>;

    fn on_event(&mut self, event: FromEngine<Self::Request>) {
        // delegate to the tree
        let _ = self.to_tree.send(event);
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<RequestHandlerEvent<Self::Event>> {
        let Some(ev) = ready!(self.from_tree.poll_recv(cx)) else {
            return Poll::Ready(RequestHandlerEvent::HandlerEvent(HandlerEvent::FatalError))
        };

        let ev = match ev {
            EngineApiEvent::BeaconConsensus(ev) => {
                RequestHandlerEvent::HandlerEvent(HandlerEvent::Event(ev))
            }
            EngineApiEvent::BackfillAction(action) => {
                RequestHandlerEvent::HandlerEvent(HandlerEvent::BackfillAction(action))
            }
            EngineApiEvent::Download(action) => RequestHandlerEvent::Download(action),
        };
        Poll::Ready(ev)
    }
}

/// The type for specify which is kind of engine api
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineApiKind {
    /// The chain contains Ethereum configuration.
    Ethereum,
    /// The chain contains Optimism configuration.
    OpStack,
}

/// Events emitted by the engine API handler.
#[derive(Debug)]
pub enum EngineApiEvent {
    /// Event from the consensus engine.
    // TODO(mattsse): find a more appropriate name for this variant, consider phasing it out.
    BeaconConsensus(BeaconConsensusEngineEvent),
    /// Backfill action is needed.
    BackfillAction(BackfillAction),
    /// Block download is needed.
    Download(DownloadRequest),
}

impl EngineApiEvent {
    /// Returns `true` if the event is a backfill action.
    pub const fn is_backfill_action(&self) -> bool {
        matches!(self, Self::BackfillAction(_))
    }
}

impl From<BeaconConsensusEngineEvent> for EngineApiEvent {
    fn from(event: BeaconConsensusEngineEvent) -> Self {
        Self::BeaconConsensus(event)
    }
}

/// Events received from the engine.
#[derive(Debug)]
pub enum FromEngine<Req> {
    /// Event from the top level orchestrator.
    Event(FromOrchestrator),
    /// Request from the engine.
    Request(Req),
    /// Downloaded blocks from the network.
    DownloadedBlocks(Vec<SealedBlockWithSenders>),
}

impl<Req> From<FromOrchestrator> for FromEngine<Req> {
    fn from(event: FromOrchestrator) -> Self {
        Self::Event(event)
    }
}

/// Requests produced by a [`EngineRequestHandler`].
#[derive(Debug)]
pub enum RequestHandlerEvent<T> {
    /// An event emitted by the handler.
    HandlerEvent(HandlerEvent<T>),
    /// Request to download blocks.
    Download(DownloadRequest),
}

/// A request to download blocks from the network.
#[derive(Debug)]
pub enum DownloadRequest {
    /// Download the given set of blocks.
    BlockSet(HashSet<B256>),
    /// Download the given range of blocks.
    BlockRange(B256, u64),
}

impl DownloadRequest {
    /// Returns a [`DownloadRequest`] for a single block.
    pub fn single_block(hash: B256) -> Self {
        Self::BlockSet(HashSet::from([hash]))
    }
}
