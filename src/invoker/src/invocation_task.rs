use std::collections::HashSet;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use common::types::{EntryIndex, PartitionLeaderEpoch, ServiceInvocationId};
use futures::{future, stream, Stream, StreamExt};
use hyper::body::Sender;
use hyper::client::HttpConnector;
use hyper::http::response::Parts;
use hyper::http::HeaderValue;
use hyper::{http, Body, Request, Uri};
use hyper_tls::HttpsConnector;
use journal::raw::RawEntry;
use journal::Completion;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::sdk::propagation::TraceContextPropagator;
use opentelemetry_http::HeaderInjector;
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tracing::trace;

use super::message::{
    Decoder, Encoder, EncodingError, MessageHeader, MessageType, ProtocolMessage,
};
use super::{EndpointMetadata, InvokeInputJournal, JournalMetadata, JournalReader, ProtocolType};

// Clippy false positive, might be caused by Bytes contained within HeaderValue.
// https://github.com/rust-lang/rust/issues/40543#issuecomment-1212981256
#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_RESTATE: HeaderValue = HeaderValue::from_static("application/restate");

#[derive(Debug, thiserror::Error)]
pub(crate) enum InvocationTaskError {
    #[error("unexpected http status code: {0}")]
    UnexpectedResponse(http::StatusCode),
    #[error("unexpected http status code: {0:?}")]
    UnexpectedContentType(Option<HeaderValue>),
    #[error("received unexpected message: {0:?}")]
    UnexpectedMessage(MessageType),
    #[error("encoding/decoding error: {0}")]
    Encoding(#[from] EncodingError),
    #[error("error when trying to read the journal: {0}")]
    JournalReader(Box<dyn Error + Send + Sync + 'static>),
    #[error("other hyper error: {0}")]
    Network(hyper::Error),
    #[error("unexpected join error, looks like hyper panicked: {0}")]
    UnexpectedJoinError(#[from] JoinError),
    #[error(transparent)]
    Other(#[from] Box<dyn Error + Send + Sync + 'static>),
}

// Copy pasted from hyper::Error
// https://github.com/hyperium/hyper/blob/40c01dfb4f87342a6f86f07564ddc482194c6240/src/error.rs#L229
// TODO hopefully this code is not needed anymore with hyper 1.0,
//  as we'll have more control on the h2 frames themselves.
//  Revisit when upgrading to hyper 1.0.
fn find_source<E: Error + 'static>(err: &hyper::Error) -> Option<&E> {
    let mut cause = err.source();
    while let Some(err) = cause {
        if let Some(typed) = err.downcast_ref() {
            return Some(typed);
        }
        cause = err.source();
    }

    // else
    None
}

fn h2_reason(err: &hyper::Error) -> h2::Reason {
    // Find an h2::Reason somewhere in the cause stack, if it exists,
    // otherwise assume an INTERNAL_ERROR.
    find_source::<h2::Error>(err)
        .and_then(|h2_err| h2_err.reason())
        .unwrap_or(h2::Reason::INTERNAL_ERROR)
}

pub(crate) struct InvocationTaskOutput {
    pub(crate) partition: PartitionLeaderEpoch,
    pub(crate) service_invocation_id: ServiceInvocationId,
    pub(crate) inner: InvocationTaskOutputInner,
}

pub(crate) enum InvocationTaskOutputInner {
    NewEntry {
        entry_index: EntryIndex,
        raw_entry: RawEntry,
    },
    Closed,
    Suspended(HashSet<EntryIndex>),
    Failed(InvocationTaskError),
}

impl From<InvocationTaskError> for InvocationTaskOutputInner {
    fn from(value: InvocationTaskError) -> Self {
        InvocationTaskOutputInner::Failed(value)
    }
}

/// Represents an open invocation stream
pub(crate) struct InvocationTask<JR> {
    // Connection params
    partition: PartitionLeaderEpoch,
    service_invocation_id: ServiceInvocationId,
    endpoint_metadata: EndpointMetadata,

    next_journal_index: EntryIndex,

    // Invoker tx/rx
    journal_reader: JR,
    invoker_tx: mpsc::UnboundedSender<InvocationTaskOutput>,
    invoker_rx: Option<mpsc::UnboundedReceiver<Completion>>,

    // Encoder/Decoder
    encoder: Encoder,
    decoder: Decoder,
}

/// This is needed to split the run_internal in multiple loop functions and have shortcircuiting.
enum TerminalLoopState<T> {
    Continue(T),
    Closed,
    Suspended(HashSet<EntryIndex>),
    Failed(InvocationTaskError),
}

impl<T, E: Into<InvocationTaskError>> From<Result<T, E>> for TerminalLoopState<T> {
    fn from(value: Result<T, E>) -> Self {
        match value {
            Ok(v) => TerminalLoopState::Continue(v),
            Err(e) => TerminalLoopState::Failed(e.into()),
        }
    }
}

impl<T> From<hyper::Error> for TerminalLoopState<T> {
    fn from(err: hyper::Error) -> Self {
        if h2_reason(&err) == h2::Reason::NO_ERROR {
            TerminalLoopState::Closed
        } else {
            TerminalLoopState::Failed(InvocationTaskError::Network(err))
        }
    }
}

/// Could be replaced by ? operator if we had Try stable. See [`InvocationTask::run_internal`]
macro_rules! shortcircuit {
    ($value:expr) => {
        match TerminalLoopState::from($value) {
            TerminalLoopState::Continue(v) => v,
            TerminalLoopState::Closed => return TerminalLoopState::Closed,
            TerminalLoopState::Suspended(v) => return TerminalLoopState::Suspended(v),
            TerminalLoopState::Failed(e) => return TerminalLoopState::Failed(e),
        }
    };
}

impl<JR, JS> InvocationTask<JR>
where
    JR: JournalReader<JournalStream = JS>,
    JS: Stream<Item = RawEntry> + Unpin,
{
    pub fn new(
        partition: PartitionLeaderEpoch,
        sid: ServiceInvocationId,
        protocol_version: u16,
        endpoint_metadata: EndpointMetadata,
        journal_reader: JR,
        invoker_tx: mpsc::UnboundedSender<InvocationTaskOutput>,
        invoker_rx: Option<mpsc::UnboundedReceiver<Completion>>,
    ) -> Self {
        Self {
            partition,
            service_invocation_id: sid,
            endpoint_metadata,
            next_journal_index: 0,
            journal_reader,
            invoker_tx,
            invoker_rx,
            encoder: Encoder::new(protocol_version),
            decoder: Default::default(),
        }
    }

    /// Loop opening the request to service endpoint and consuming the stream
    #[tracing::instrument(name = "run", level = "trace", skip_all, fields(restate.sid = %self.service_invocation_id))]
    pub async fn run(mut self, input_journal: InvokeInputJournal) {
        let result = self.run_internal(input_journal).await;
        let inner = match result {
            TerminalLoopState::Continue(_) => {
                unreachable!("This is not supposed to happen")
            }
            TerminalLoopState::Closed => InvocationTaskOutputInner::Closed,
            TerminalLoopState::Suspended(v) => InvocationTaskOutputInner::Suspended(v),
            TerminalLoopState::Failed(e) => InvocationTaskOutputInner::Failed(e),
        };
        let _ = self.invoker_tx.send(InvocationTaskOutput {
            partition: self.partition,
            service_invocation_id: self.service_invocation_id,
            inner,
        });
    }

    async fn run_internal(&mut self, input_journal: InvokeInputJournal) -> TerminalLoopState<()> {
        // Resolve journal and its metadata
        let (journal_metadata, journal_stream) = match input_journal {
            InvokeInputJournal::NoCachedJournal => {
                let (journal_meta, journal_stream) = shortcircuit!(self
                    .journal_reader
                    .read_journal(&self.service_invocation_id)
                    .await
                    .map_err(|e| InvocationTaskError::JournalReader(Box::new(e))));
                (journal_meta, future::Either::Left(journal_stream))
            }
            InvokeInputJournal::CachedJournal(journal_meta, journal_items) => (
                journal_meta,
                future::Either::Right(stream::iter(journal_items)),
            ),
        };

        // Acquire an HTTP client
        let client = Self::get_client();

        // Prepare the request and send start message
        let (mut http_stream_tx, http_request) = self.prepare_request(&journal_metadata);
        shortcircuit!(
            self.write_start(&mut http_stream_tx, &journal_metadata)
                .await
        );

        // Start the request
        let mut http_stream_rx = shortcircuit!(
            self.wait_response_and_replay_end_loop(
                &mut http_stream_tx,
                client,
                http_request,
                journal_stream,
            )
            .await
        );

        // Check all the entries have been replayed
        debug_assert_eq!(self.next_journal_index, journal_metadata.journal_size);

        // If we have the invoker_rx, we can use the bidi stream loop,
        // which both reads the invoker_rx and the http_stream_rx
        if let Some(invoker_rx) = self.invoker_rx.take() {
            shortcircuit!(
                self.bidi_stream_loop(&mut http_stream_tx, invoker_rx, &mut http_stream_rx)
                    .await
            );
        }

        // We don't have the invoker_rx, so we simply consume the response
        self.response_stream_loop(&mut http_stream_rx).await
    }

    // --- Loops

    /// This loop concurrently pushes journal entries and waits for the response headers and end of replay.
    async fn wait_response_and_replay_end_loop<JournalStream>(
        &mut self,
        http_stream_tx: &mut Sender,
        client: hyper::Client<HttpsConnector<HttpConnector>>,
        req: Request<Body>,
        mut journal_stream: JournalStream,
    ) -> TerminalLoopState<Body>
    where
        JournalStream: Stream<Item = RawEntry> + Unpin,
    {
        // Because the body sender blocks on waiting for the request body buffer to be available,
        // we need to spawn the request initiation separately, otherwise the loop below
        // will deadlock on the journal entry write.
        // This task::spawn won't be required by hyper 1.0, as the connection will be driven by a task
        // spawned somewhere else (perhaps in the connection pool).
        // See: https://github.com/restatedev/restate/issues/96 and https://github.com/restatedev/restate/issues/76
        let mut req_fut = AbortOnDrop(tokio::task::spawn(client.request(req)));

        let mut http_stream_rx_res = None;

        loop {
            tokio::select! {
                response_res = &mut req_fut, if http_stream_rx_res.is_none() => {
                    let http_response = match shortcircuit!(response_res) {
                        Ok(res) => res,
                        Err(hyper_err) => shortcircuit!(hyper_err),
                    };

                    // Check the response is valid
                    let (http_response_header, http_stream_rx) = http_response.into_parts();
                    shortcircuit!(Self::validate_response(http_response_header));

                    http_stream_rx_res = Some(http_stream_rx);
                },
                Some(je) = journal_stream.next() => {
                    shortcircuit!(self.write(http_stream_tx, ProtocolMessage::UnparsedEntry(je)).await);
                    self.next_journal_index += 1;
                },
                else => break,
            }
        }

        trace!("Finished to replay the journal");
        TerminalLoopState::Continue(http_stream_rx_res.unwrap())
    }

    /// This loop concurrently reads the http response stream and journal completions from the invoker.
    async fn bidi_stream_loop(
        &mut self,
        http_stream_tx: &mut Sender,
        mut invoker_rx: mpsc::UnboundedReceiver<Completion>,
        http_stream_rx: &mut Body,
    ) -> TerminalLoopState<()> {
        loop {
            tokio::select! {
                opt_completion = invoker_rx.recv() => {
                    match opt_completion {
                        Some(completion) => {
                            trace!("Sending the completion to the wire");
                            shortcircuit!(self.write(http_stream_tx, completion.into()).await);
                        },
                        None => {
                            // Completion channel is closed,
                            // the invoker main loop won't send completions anymore.
                            // Response stream might still be open though.
                            return TerminalLoopState::Continue(())
                        },
                    }
                },
                opt_buf = http_stream_rx.next() => {
                    match opt_buf {
                        Some(Ok(buf)) => shortcircuit!(self.handle_read(buf)),
                        Some(Err(hyper_err)) => shortcircuit!(hyper_err),
                        None => {
                            // Response stream is closed. No further processing is needed.
                            return TerminalLoopState::Closed
                        }
                    }
                },
            }
        }
    }

    async fn response_stream_loop(&mut self, http_stream_rx: &mut Body) -> TerminalLoopState<()> {
        while let Some(buf_res) = http_stream_rx.next().await {
            match buf_res {
                Ok(buf) => shortcircuit!(self.handle_read(buf)),
                Err(hyper_err) => shortcircuit!(hyper_err),
            }
        }

        TerminalLoopState::Closed
    }

    // --- Read and write methods

    async fn write_start(
        &mut self,
        http_stream_tx: &mut Sender,
        journal_metadata: &JournalMetadata,
    ) -> Result<(), InvocationTaskError> {
        // Send the invoke frame
        self.write(
            http_stream_tx,
            ProtocolMessage::new_start_message(
                self.service_invocation_id
                    .invocation_id
                    .as_bytes()
                    .to_vec()
                    .into(),
                self.service_invocation_id.service_id.key.clone(),
                journal_metadata.journal_size,
            ),
        )
        .await
    }

    async fn write(
        &mut self,
        http_stream_tx: &mut Sender,
        msg: ProtocolMessage,
    ) -> Result<(), InvocationTaskError> {
        let buf = self.encoder.encode(msg);

        if let Err(hyper_err) = http_stream_tx.send_data(buf).await {
            // is_closed() is try only if the request channel (Sender) has been closed.
            // This can happen if the service endpoint is suspending.
            if !hyper_err.is_closed() {
                return Err(InvocationTaskError::Network(hyper_err));
            }
        };
        Ok(())
    }

    fn handle_read(&mut self, buf: Bytes) -> TerminalLoopState<()> {
        self.decoder.push(buf);

        while let Some((frame_header, frame)) = shortcircuit!(self.decoder.consume_next()) {
            shortcircuit!(self.handle_message(frame_header, frame));
        }

        TerminalLoopState::Continue(())
    }

    fn handle_message(
        &mut self,
        mh: MessageHeader,
        message: ProtocolMessage,
    ) -> TerminalLoopState<()> {
        trace!(restate.protocol.message_header = ?mh, restate.protocol.message = ?message, "Received message");
        match message {
            ProtocolMessage::Start(_) => TerminalLoopState::Failed(
                InvocationTaskError::UnexpectedMessage(MessageType::Start),
            ),
            ProtocolMessage::Completion(_) => TerminalLoopState::Failed(
                InvocationTaskError::UnexpectedMessage(MessageType::Completion),
            ),
            ProtocolMessage::Suspension(suspension) => TerminalLoopState::Suspended(
                HashSet::from_iter(suspension.entry_indexes.into_iter()),
            ),
            ProtocolMessage::UnparsedEntry(raw_entry) => {
                let _ = self.invoker_tx.send(InvocationTaskOutput {
                    partition: self.partition,
                    service_invocation_id: self.service_invocation_id.clone(),
                    inner: InvocationTaskOutputInner::NewEntry {
                        entry_index: self.next_journal_index,
                        raw_entry,
                    },
                });
                self.next_journal_index += 1;
                TerminalLoopState::Continue(())
            }
        }
    }

    // --- HTTP related methods

    fn prepare_request(&mut self, journal_metadata: &JournalMetadata) -> (Sender, Request<Body>) {
        let (http_stream_tx, req_body) = Body::channel();
        let mut http_request_builder = Request::builder()
            .method(http::Method::POST)
            .header(http::header::CONTENT_TYPE, APPLICATION_RESTATE)
            .uri(Self::append_path(
                &self.endpoint_metadata.address,
                &[
                    self.service_invocation_id
                        .service_id
                        .service_name
                        .chars()
                        .as_str(),
                    &journal_metadata.method,
                ],
            ));

        // In case it's bidi stream, force HTTP/2
        if self.endpoint_metadata.protocol_type == ProtocolType::BidiStream {
            http_request_builder = http_request_builder.version(http::Version::HTTP_2);
        }

        // Inject OpenTelemetry context
        TraceContextPropagator::new().inject_context(
            &journal_metadata.tracing_context,
            &mut HeaderInjector(
                http_request_builder
                    .headers_mut()
                    .expect("The request builder shouldn't fail"),
            ),
        );

        // Inject additional headers
        for (header_name, header_value) in
            &self.endpoint_metadata.delivery_options.additional_headers
        {
            http_request_builder = http_request_builder.header(header_name, header_value);
        }

        let http_request = http_request_builder
            .body(req_body)
            // This fails only in case the URI is malformed, which should never happen
            .expect("The request builder shouldn't fail");

        (http_stream_tx, http_request)
    }

    fn append_path(uri: &Uri, fragments: &[&str]) -> Uri {
        let p = format!(
            "{}/{}",
            match uri.path().strip_suffix('/') {
                None => uri.path(),
                Some(s) => s,
            },
            fragments.join("/")
        );

        Uri::builder()
            .authority(
                uri.authority()
                    .expect("The function endpoint URI must have the authority")
                    .clone(),
            )
            .scheme(
                uri.scheme()
                    .expect("The function endpoint URI must have the scheme")
                    .clone(),
            )
            .path_and_query(p)
            .build()
            .unwrap()
    }

    // TODO pooling https://github.com/restatedev/restate/issues/76
    fn get_client() -> hyper::Client<HttpsConnector<HttpConnector>, Body> {
        hyper::Client::builder()
            .http2_only(true)
            .build::<_, Body>(HttpsConnector::new())
    }

    fn validate_response(mut parts: Parts) -> Result<(), InvocationTaskError> {
        if !parts.status.is_success() {
            return Err(InvocationTaskError::UnexpectedResponse(parts.status));
        }

        let content_type = parts.headers.remove(http::header::CONTENT_TYPE);
        match content_type {
            // Check content type is application/restate
            Some(ct) =>
            {
                #[allow(clippy::borrow_interior_mutable_const)]
                if ct != APPLICATION_RESTATE {
                    return Err(InvocationTaskError::UnexpectedContentType(Some(ct)));
                }
            }
            None => return Err(InvocationTaskError::UnexpectedContentType(None)),
        }

        Ok(())
    }
}

/// This wrapper makes sure we abort the task when the JoinHandle is dropped,
/// but it doesn't wait for the task to complete, because we simply don't have async drops!
/// For more: https://github.com/tokio-rs/tokio/issues/2596
/// Inspired by: https://github.com/cyb0124/abort-on-drop
#[derive(Debug)]
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Future for AbortOnDrop<T> {
    type Output = <JoinHandle<T> as Future>::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort()
    }
}