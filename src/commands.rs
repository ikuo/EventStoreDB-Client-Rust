#![allow(clippy::large_enum_variant)]
//! Commands this client supports.
use futures::{Stream, StreamExt, TryStreamExt};
use std::ops::Add;
use std::time::{Duration, SystemTime};

use crate::event_store::client::{persistent, shared, streams};
use crate::types::{
    EventData, ExpectedRevision, PersistentSubscriptionSettings, Position, ReadDirection,
    RecordedEvent, ResolvedEvent, StreamPosition, SubscriptionEvent, WriteResult,
};

use async_stream::try_stream;
use persistent::persistent_subscriptions_client::PersistentSubscriptionsClient;
use prost_types::Timestamp;
use shared::{Empty, StreamIdentifier, Uuid};
use streams::streams_client::StreamsClient;

use crate::batch::BatchAppendClient;
use crate::grpc::{handle_error, GrpcClient, Handle, Msg};
use crate::options::append_to_stream::AppendToStreamOptions;
use crate::options::batch_append::BatchAppendOptions;
use crate::options::persistent_subscription::PersistentSubscriptionOptions;
use crate::options::read_all::ReadAllOptions;
use crate::options::read_stream::ReadStreamOptions;
use crate::options::subscribe_to_stream::SubscribeToStreamOptions;
use crate::options::{CommonOperationOptions, OperationKind, Options};
use crate::server_features::Features;
use crate::{
    ClientSettings, CurrentRevision, DeletePersistentSubscriptionOptions, DeleteStreamOptions,
    GetPersistentSubscriptionInfoOptions, ListPersistentSubscriptionsOptions, NakAction,
    PersistentSubscriptionConnectionInfo, PersistentSubscriptionEvent, PersistentSubscriptionInfo,
    PersistentSubscriptionMeasurements, PersistentSubscriptionStats,
    PersistentSubscriptionToAllOptions, ReplayParkedMessagesOptions,
    RestartPersistentSubscriptionSubsystem, RetryOptions, RevisionOrPosition,
    SubscribeToAllOptions, SubscribeToPersistentSubscriptionOptions, SubscriptionFilter,
    SystemConsumerStrategy, TombstoneStreamOptions,
};
use tonic::{Request, Streaming};

fn raw_uuid_to_uuid(src: Uuid) -> uuid::Uuid {
    use byteorder::{BigEndian, ByteOrder};

    let value = src
        .value
        .expect("We expect Uuid value to be defined for now");

    match value {
        shared::uuid::Value::Structured(s) => {
            let mut buf = [0u8; 16];

            BigEndian::write_i64(&mut buf, s.most_significant_bits);
            BigEndian::write_i64(&mut buf[8..16], s.least_significant_bits);

            uuid::Uuid::from_bytes(buf)
        }

        shared::uuid::Value::String(s) => s
            .parse()
            .expect("We expect a valid UUID out of this String"),
    }
}

fn raw_persistent_uuid_to_uuid(src: Uuid) -> uuid::Uuid {
    use byteorder::{BigEndian, ByteOrder};

    let value = src
        .value
        .expect("We expect Uuid value to be defined for now");

    match value {
        shared::uuid::Value::Structured(s) => {
            let mut buf = [0u8; 16];

            BigEndian::write_i64(&mut buf, s.most_significant_bits);
            BigEndian::write_i64(&mut buf[8..16], s.least_significant_bits);

            uuid::Uuid::from_bytes(buf)
        }

        shared::uuid::Value::String(s) => s
            .parse()
            .expect("We expect a valid UUID out of this String"),
    }
}

fn convert_event_data(event: EventData) -> streams::AppendReq {
    use streams::append_req;

    let id = event.id_opt.unwrap_or_else(uuid::Uuid::new_v4);
    let id = shared::uuid::Value::String(id.to_string());
    let id = Uuid { value: Some(id) };
    let custom_metadata = event
        .custom_metadata
        .map_or_else(Vec::new, |b| (&*b).into());

    let msg = append_req::ProposedMessage {
        id: Some(id),
        metadata: event.metadata,
        custom_metadata,
        data: (&*event.payload).into(),
    };

    let content = append_req::Content::ProposedMessage(msg);

    streams::AppendReq {
        content: Some(content),
    }
}

fn convert_proto_recorded_event(
    event: streams::read_resp::read_event::RecordedEvent,
) -> RecordedEvent {
    let id = event
        .id
        .map(raw_uuid_to_uuid)
        .expect("Unable to parse Uuid [convert_proto_recorded_event]");

    let position = Position {
        commit: event.commit_position,
        prepare: event.prepare_position,
    };

    let event_type = if let Some(tpe) = event.metadata.get("type") {
        tpe.to_string()
    } else {
        "<no-event-type-provided>".to_string()
    };

    let is_json = if let Some(content_type) = event.metadata.get("content-type") {
        matches!(content_type.as_str(), "application/json")
    } else {
        false
    };

    let stream_id = String::from_utf8(
        event
            .stream_identifier
            .expect("stream_identifier is always defined")
            .stream_name,
    )
    .expect("It's always UTF-8");
    RecordedEvent {
        id,
        stream_id,
        revision: event.stream_revision,
        position,
        event_type,
        is_json,
        metadata: event.metadata,
        custom_metadata: event.custom_metadata.into(),
        data: event.data.into(),
    }
}

fn convert_persistent_proto_recorded_event(
    event: persistent::read_resp::read_event::RecordedEvent,
) -> RecordedEvent {
    let id = event
        .id
        .map(raw_persistent_uuid_to_uuid)
        .expect("Unable to parse Uuid [convert_persistent_proto_recorded_event]");

    let position = Position {
        commit: event.commit_position,
        prepare: event.prepare_position,
    };

    let event_type = if let Some(tpe) = event.metadata.get("type") {
        tpe.to_string()
    } else {
        "<no-event-type-provided>".to_owned()
    };

    let is_json = if let Some(content_type) = event.metadata.get("content-type") {
        matches!(content_type.as_str(), "application/json")
    } else {
        false
    };

    let stream_id = String::from_utf8(
        event
            .stream_identifier
            .expect("stream_identifier is always defined")
            .stream_name,
    )
    .expect("string is UTF-8 valid");

    RecordedEvent {
        id,
        stream_id,
        revision: event.stream_revision,
        position,
        event_type,
        is_json,
        metadata: event.metadata,
        custom_metadata: event.custom_metadata.into(),
        data: event.data.into(),
    }
}

fn convert_event_data_to_batch_proposed_message(
    event: EventData,
) -> streams::batch_append_req::ProposedMessage {
    use streams::batch_append_req;

    let id = event.id_opt.unwrap_or_else(uuid::Uuid::new_v4);
    let id = shared::uuid::Value::String(id.to_string());
    let id = Uuid { value: Some(id) };
    let custom_metadata = event
        .custom_metadata
        .map_or_else(Vec::new, |b| (&*b).into());

    batch_append_req::ProposedMessage {
        id: Some(id),
        metadata: event.metadata,
        custom_metadata,
        data: (&*event.payload).into(),
    }
}

/// This trait is added as a compatibility layer when interacting with EventStoreDB servers <= 21 version.
/// Its goal is to translate a persistent subscription starting position value to a u64 so the deprecated
/// revision field can be used.
///
/// When dealing with persistent subscription to $all, we default any persistent subscription to $all
/// starting position to 0 as the server will just ignore the field anyway.
pub(crate) trait PsPosition: Copy {
    fn to_deprecated_value(self) -> Option<u64>;
}

impl PsPosition for u64 {
    fn to_deprecated_value(self) -> Option<u64> {
        Some(self)
    }
}

impl PsPosition for Position {
    fn to_deprecated_value(self) -> Option<u64> {
        None
    }
}

fn ps_to_deprecated_revision_value<A>(value: StreamPosition<A>) -> u64
where
    A: PsPosition,
{
    match value {
        StreamPosition::Start => 0,
        StreamPosition::End => u64::MAX,
        StreamPosition::Position(value) => value.to_deprecated_value().unwrap_or(0),
    }
}

fn convert_settings_create<A>(
    settings: &PersistentSubscriptionSettings<A>,
) -> crate::Result<persistent::create_req::Settings>
where
    A: PsPosition,
{
    let named_consumer_strategy = match settings.consumer_strategy_name {
        SystemConsumerStrategy::DispatchToSingle => 0,
        SystemConsumerStrategy::RoundRobin => 1,
        SystemConsumerStrategy::Pinned => 2,
        SystemConsumerStrategy::Custom(_) | SystemConsumerStrategy::PinnedByCorrelation => {
            // Currently unsupported by all released servers.
            return Err(crate::Error::UnsupportedFeature);
        }
    };

    let deprecated_revision_value = ps_to_deprecated_revision_value(settings.start_from);
    let consumer_strategy = settings.consumer_strategy_name.to_string();

    #[allow(deprecated)]
    let settings = persistent::create_req::Settings {
        resolve_links: settings.resolve_link_tos,
        revision: deprecated_revision_value,
        extra_statistics: settings.extra_statistics,
        message_timeout: Some(
            persistent::create_req::settings::MessageTimeout::MessageTimeoutMs(
                settings.message_timeout.as_millis() as i32,
            ),
        ),
        max_retry_count: settings.max_retry_count,
        checkpoint_after: Some(
            persistent::create_req::settings::CheckpointAfter::CheckpointAfterMs(
                settings.checkpoint_after.as_millis() as i32,
            ),
        ),
        min_checkpoint_count: settings.checkpoint_lower_bound,
        max_checkpoint_count: settings.checkpoint_upper_bound,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
        consumer_strategy,
    };

    Ok(settings)
}

fn convert_settings_update<A>(
    settings: &PersistentSubscriptionSettings<A>,
) -> crate::Result<persistent::update_req::Settings>
where
    A: PsPosition,
{
    let named_consumer_strategy = match settings.consumer_strategy_name {
        SystemConsumerStrategy::DispatchToSingle => 0,
        SystemConsumerStrategy::RoundRobin => 1,
        SystemConsumerStrategy::Pinned => 2,
        SystemConsumerStrategy::Custom(_) | SystemConsumerStrategy::PinnedByCorrelation => {
            // Currently unsupported by all released servers.
            return Err(crate::Error::UnsupportedFeature);
        }
    };

    let deprecated_revision_value = ps_to_deprecated_revision_value(settings.start_from);

    #[allow(deprecated)]
    let settings = persistent::update_req::Settings {
        resolve_links: settings.resolve_link_tos,
        revision: deprecated_revision_value,
        extra_statistics: settings.extra_statistics,
        message_timeout: Some(
            persistent::update_req::settings::MessageTimeout::MessageTimeoutMs(
                settings.message_timeout.as_millis() as i32,
            ),
        ),
        max_retry_count: settings.max_retry_count,
        checkpoint_after: Some(
            persistent::update_req::settings::CheckpointAfter::CheckpointAfterMs(
                settings.checkpoint_after.as_millis() as i32,
            ),
        ),
        min_checkpoint_count: settings.checkpoint_lower_bound,
        max_checkpoint_count: settings.checkpoint_upper_bound,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
    };

    Ok(settings)
}

fn convert_proto_read_event(event: streams::read_resp::ReadEvent) -> ResolvedEvent {
    let commit_position = if let Some(pos_alt) = event.position {
        match pos_alt {
            streams::read_resp::read_event::Position::CommitPosition(pos) => Some(pos),
            streams::read_resp::read_event::Position::NoPosition(_) => None,
        }
    } else {
        None
    };

    ResolvedEvent {
        event: event.event.map(convert_proto_recorded_event),
        link: event.link.map(convert_proto_recorded_event),
        commit_position,
    }
}

fn convert_persistent_proto_read_event(
    event: persistent::read_resp::ReadEvent,
) -> PersistentSubscriptionEvent {
    use persistent::read_resp::read_event::Count;
    let commit_position = if let Some(pos_alt) = event.position {
        match pos_alt {
            persistent::read_resp::read_event::Position::CommitPosition(pos) => Some(pos),
            persistent::read_resp::read_event::Position::NoPosition(_) => None,
        }
    } else {
        None
    };

    let resolved = ResolvedEvent {
        event: event.event.map(convert_persistent_proto_recorded_event),
        link: event.link.map(convert_persistent_proto_recorded_event),
        commit_position,
    };

    let retry_count = event.count.map_or(0usize, |repr| match repr {
        Count::RetryCount(count) => count as usize,
        Count::NoRetryCount(_) => 0,
    });

    PersistentSubscriptionEvent::EventAppeared {
        retry_count,
        event: resolved,
    }
}

pub fn filter_into_proto(filter: SubscriptionFilter) -> streams::read_req::options::FilterOptions {
    use options::filter_options::{Expression, Filter, Window};
    use streams::read_req::options::{self, FilterOptions};

    let window = match filter.max {
        Some(max) => Window::Max(max),
        None => Window::Count(Empty {}),
    };

    let expr = Expression {
        regex: filter.regex.unwrap_or_else(|| "".to_string()),
        prefix: filter.prefixes,
    };

    let filter = if filter.based_on_stream {
        Filter::StreamIdentifier(expr)
    } else {
        Filter::EventType(expr)
    };

    FilterOptions {
        filter: Some(filter),
        window: Some(window),
        checkpoint_interval_multiplier: 1,
    }
}

pub fn ps_create_filter_into_proto(
    filter: &SubscriptionFilter,
) -> persistent::create_req::all_options::FilterOptions {
    use persistent::create_req::all_options::filter_options::{Expression, Filter, Window};
    use persistent::create_req::all_options::FilterOptions;

    let window = match filter.max {
        Some(max) => Window::Max(max),
        None => Window::Count(Empty {}),
    };

    let expr = Expression {
        regex: filter.regex.clone().unwrap_or_else(|| "".to_string()),
        prefix: filter.prefixes.clone(),
    };

    let filter = if filter.based_on_stream {
        Filter::StreamIdentifier(expr)
    } else {
        Filter::EventType(expr)
    };

    FilterOptions {
        filter: Some(filter),
        window: Some(window),
        checkpoint_interval_multiplier: 1,
    }
}

fn build_request_metadata(
    settings: &ClientSettings,
    options: &CommonOperationOptions,
) -> tonic::metadata::MetadataMap
where
{
    use tonic::metadata::MetadataValue;

    let mut metadata = tonic::metadata::MetadataMap::new();
    let credentials = options
        .credentials
        .as_ref()
        .or_else(|| settings.default_authenticated_user().as_ref());

    if let Some(creds) = credentials {
        let login = String::from_utf8_lossy(&*creds.login).into_owned();
        let password = String::from_utf8_lossy(&*creds.password).into_owned();

        let basic_auth_string = base64::encode(&format!("{}:{}", login, password));
        let basic_auth = format!("Basic {}", basic_auth_string);
        let header_value = MetadataValue::from_str(basic_auth.as_str())
            .expect("Auth header value should be valid metadata header value");

        metadata.insert("authorization", header_value);
    }

    if options.requires_leader {
        let header_value = MetadataValue::from_str("true").expect("valid metadata header value");
        metadata.insert("requires-leader", header_value);
    }

    if let Some(conn_name) = settings.connection_name.as_ref() {
        let header_value =
            MetadataValue::from_str(conn_name.as_str()).expect("valid metadata header value");
        metadata.insert("connection-name", header_value);
    }

    metadata
}

pub(crate) fn new_request<Message, Opts>(
    settings: &ClientSettings,
    options: &Opts,
    message: Message,
) -> tonic::Request<Message>
where
    Opts: crate::options::Options,
{
    let mut req = tonic::Request::new(message);
    let kind = options.kind();
    let options = options.common_operation_options();

    *req.metadata_mut() = build_request_metadata(settings, options);

    if let Some(duration) = options.deadline {
        req.set_timeout(duration);
    } else if let Some(duration) = settings.default_deadline {
        if kind != OperationKind::Streaming {
            req.set_timeout(duration);
        }
    } else if kind == crate::options::OperationKind::Regular {
        req.set_timeout(Duration::from_secs(10));
    }

    req
}

/// Sends asynchronously the write command to the server.
pub async fn append_to_stream(
    connection: &GrpcClient,
    stream: impl AsRef<str>,
    options: &AppendToStreamOptions,
    mut events: impl Iterator<Item = EventData> + Send + 'static,
) -> crate::Result<WriteResult> {
    use streams::append_req::{self, Content};
    use streams::AppendReq;

    let stream = stream.as_ref().to_string();
    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.into_bytes(),
    });
    let header = Content::Options(append_req::Options {
        stream_identifier,
        expected_stream_revision: Some(options.version.clone()),
    });
    let header = AppendReq {
        content: Some(header),
    };

    let payload = async_stream::stream! {
        yield header;

        while let Some(event) = events.next() {
            yield convert_event_data(event);
        }
    };

    let handle = connection.current_selected_node().await?;
    let handle_id = handle.id();
    let req = new_request(connection.connection_settings(), options, payload);
    let mut client = StreamsClient::new(handle.channel);
    let resp = match client.append(req).await {
        Err(e) => {
            let e = crate::Error::from_grpc(e);
            handle_error(&connection.sender, handle_id, &e).await;

            return Err(e);
        }

        Ok(resp) => resp.into_inner(),
    };

    match resp.result.unwrap() {
        streams::append_resp::Result::Success(success) => {
            let next_expected_version = match success.current_revision_option.unwrap() {
                streams::append_resp::success::CurrentRevisionOption::CurrentRevision(rev) => rev,
                streams::append_resp::success::CurrentRevisionOption::NoStream(_) => 0,
            };

            let position = match success.position_option.unwrap() {
                streams::append_resp::success::PositionOption::Position(pos) => Position {
                    commit: pos.commit_position,
                    prepare: pos.prepare_position,
                },

                streams::append_resp::success::PositionOption::NoPosition(_) => Position::start(),
            };

            let write_result = WriteResult {
                next_expected_version,
                position,
            };

            Ok(write_result)
        }

        streams::append_resp::Result::WrongExpectedVersion(error) => {
            let current = match error.current_revision_option.unwrap() {
                streams::append_resp::wrong_expected_version::CurrentRevisionOption::CurrentRevision(rev) => CurrentRevision::Current(rev),
                streams::append_resp::wrong_expected_version::CurrentRevisionOption::CurrentNoStream(_) => CurrentRevision::NoStream,
            };

            let expected = match error.expected_revision_option.unwrap() {
                streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedRevision(rev) => ExpectedRevision::Exact(rev),
                streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedAny(_) => ExpectedRevision::Any,
                streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedStreamExists(_) => ExpectedRevision::StreamExists,
                streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedNoStream(_) => ExpectedRevision::NoStream,
            };

            Err(crate::Error::WrongExpectedVersion { current, expected })
        }
    }
}

pub async fn batch_append(
    connection: &GrpcClient,
    options: &BatchAppendOptions,
) -> crate::Result<BatchAppendClient> {
    use futures::SinkExt;
    use streams::{
        batch_append_req::{options::ExpectedStreamPosition, Options, ProposedMessage},
        batch_append_resp::{
            self,
            success::{CurrentRevisionOption, PositionOption},
        },
        BatchAppendReq,
    };

    let connection = connection.clone();
    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::BATCH_APPEND) {
        return Err(crate::Error::UnsupportedFeature);
    }

    let (forward, receiver) = futures::channel::mpsc::unbounded::<crate::batch::Req>();
    let (batch_sender, batch_receiver) = futures::channel::mpsc::unbounded();
    let mut cloned_batch_sender = batch_sender.clone();
    let batch_client = BatchAppendClient::new(batch_sender, batch_receiver, forward);

    let common_operation_options = options.common_operation_options.clone();
    let receiver = receiver.map(move |req| {
        let correlation_id = shared::uuid::Value::String(req.id.to_string());
        let correlation_id = Some(Uuid {
            value: Some(correlation_id),
        });
        let stream_identifier = Some(StreamIdentifier {
            stream_name: req.stream_name.into_bytes(),
        });

        let expected_stream_position = match req.expected_revision {
            ExpectedRevision::Exact(rev) => ExpectedStreamPosition::StreamPosition(rev),
            ExpectedRevision::NoStream => ExpectedStreamPosition::NoStream(()),
            ExpectedRevision::StreamExists => ExpectedStreamPosition::StreamExists(()),
            ExpectedRevision::Any => ExpectedStreamPosition::Any(()),
        };

        let expected_stream_position = Some(expected_stream_position);

        let proposed_messages: Vec<ProposedMessage> = req
            .events
            .into_iter()
            .map(convert_event_data_to_batch_proposed_message)
            .collect();

        let deadline = common_operation_options
            .deadline
            .unwrap_or_else(|| Duration::from_secs(10));

        let deadline = SystemTime::now().add(deadline);
        let deadline = deadline.duration_since(SystemTime::UNIX_EPOCH).unwrap();

        let options = Some(Options {
            stream_identifier,
            deadline: Some(Timestamp {
                seconds: deadline.as_secs() as i64,
                nanos: deadline.subsec_nanos() as i32,
            }),
            expected_stream_position,
        });

        BatchAppendReq {
            correlation_id,
            options,
            proposed_messages,
            is_final: true,
        }
    });

    let req = new_request(connection.connection_settings(), options, receiver);
    tokio::spawn(async move {
        let mut client = StreamsClient::new(handle.channel.clone());
        match client.batch_append(req).await {
            Err(e) => {
                let _ = cloned_batch_sender
                    .send(crate::batch::BatchMsg::Error(crate::Error::from_grpc(e)))
                    .await;
            }

            Ok(resp) => {
                let resp_stream = resp.into_inner();

                let mut resp_stream = resp_stream.map_ok(|resp| {
                    let stream_name =
                        String::from_utf8(resp.stream_identifier.unwrap().stream_name)
                            .expect("valid UTF-8 string");

                    let correlation_id = raw_uuid_to_uuid(resp.correlation_id.unwrap());
                    let result = match resp.result.unwrap() {
                        batch_append_resp::Result::Success(success) => {
                            let current_revision =
                                success.current_revision_option.and_then(|rev| match rev {
                                    CurrentRevisionOption::CurrentRevision(rev) => Some(rev),
                                    CurrentRevisionOption::NoStream(()) => None,
                                });

                            let position = success.position_option.and_then(|pos| match pos {
                                PositionOption::Position(pos) => Some(Position {
                                    commit: pos.commit_position,
                                    prepare: pos.prepare_position,
                                }),
                                PositionOption::NoPosition(_) => None,
                            });

                            let expected_version =
                                resp.expected_stream_position.map(|exp| match exp {
                                    batch_append_resp::ExpectedStreamPosition::Any(_) => {
                                        crate::types::ExpectedRevision::Any
                                    }
                                    batch_append_resp::ExpectedStreamPosition::NoStream(_) => {
                                        crate::types::ExpectedRevision::NoStream
                                    }
                                    batch_append_resp::ExpectedStreamPosition::StreamExists(_) => {
                                        crate::types::ExpectedRevision::StreamExists
                                    }
                                    batch_append_resp::ExpectedStreamPosition::StreamPosition(
                                        rev,
                                    ) => crate::types::ExpectedRevision::Exact(rev),
                                });

                            Ok(crate::batch::BatchWriteResult::new(
                                stream_name,
                                current_revision,
                                position,
                                expected_version,
                            ))
                        }
                        batch_append_resp::Result::Error(code) => {
                            let message = code.message;
                            let code = tonic::Code::from(code.code);
                            let err = crate::Error::Grpc { code, message };

                            Err(err)
                        }
                    };

                    crate::batch::Out {
                        correlation_id,
                        result,
                    }
                });

                tokio::spawn(async move {
                    loop {
                        match resp_stream.try_next().await {
                            Err(e) => {
                                let err = crate::Error::from_grpc(e);
                                crate::grpc::handle_error(handle.sender(), handle.id(), &err).await;

                                // We notify the batch-append client that its session has been closed because of a gRPC error.
                                let _ = cloned_batch_sender
                                    .send(crate::batch::BatchMsg::Error(err))
                                    .await;
                                break;
                            }

                            Ok(out) => {
                                if let Some(out) = out {
                                    if cloned_batch_sender
                                        .send(crate::batch::BatchMsg::Out(out))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }

                                    continue;
                                }

                                break;
                            }
                        }
                    }
                });
            }
        };

        Ok::<(), crate::Error>(())
    });

    Ok(batch_client)
}

pub enum ReadEvent {
    Event(ResolvedEvent),
    FirstStreamPosition(u64),
    LastStreamPosition(u64),
    LastAllStreamPosition(Position),
}

#[derive(Debug)]
pub struct ReadStream {
    sender: futures::channel::mpsc::UnboundedSender<Msg>,
    channel_id: uuid::Uuid,
    inner: Streaming<crate::event_store::client::streams::ReadResp>,
}

impl ReadStream {
    pub async fn next_read_event(&mut self) -> crate::Result<Option<ReadEvent>> {
        loop {
            match self.inner.try_next().await.map_err(crate::Error::from_grpc) {
                Err(e) => {
                    handle_error(&self.sender, self.channel_id, &e).await;
                    return Err(e);
                }

                Ok(resp) => {
                    if let Some(resp) = resp {
                        match resp.content.unwrap() {
                            streams::read_resp::Content::StreamNotFound(_) => {
                                return Err(crate::Error::ResourceNotFound)
                            }
                            streams::read_resp::Content::Event(event) => {
                                return Ok(Some(ReadEvent::Event(convert_proto_read_event(event))))
                            }

                            streams::read_resp::Content::FirstStreamPosition(event_number) => {
                                return Ok(Some(ReadEvent::FirstStreamPosition(event_number)));
                            }
                            streams::read_resp::Content::LastStreamPosition(event_number) => {
                                return Ok(Some(ReadEvent::LastStreamPosition(event_number)));
                            }
                            streams::read_resp::Content::LastAllStreamPosition(position) => {
                                return Ok(Some(ReadEvent::LastAllStreamPosition(Position {
                                    commit: position.commit_position,
                                    prepare: position.prepare_position,
                                })));
                            }
                            _ => continue,
                        }
                    }

                    return Ok(None);
                }
            }
        }
    }

    pub async fn next(&mut self) -> crate::Result<Option<ResolvedEvent>> {
        while let Some(event) = self.next_read_event().await? {
            if let ReadEvent::Event(event) = event {
                return Ok(Some(event));
            }

            continue;
        }

        Ok(None)
    }

    pub fn into_stream_of_read_events(mut self) -> impl Stream<Item = crate::Result<ReadEvent>> {
        try_stream! {
            while let Some(event) = self.next_read_event().await? {
                yield event;
            }
        }
    }

    pub fn into_stream(mut self) -> impl Stream<Item = crate::Result<ResolvedEvent>> {
        try_stream! {
            while let Some(event) = self.next().await? {
                yield event;
            }
        }
    }
}

/// Sends asynchronously the read command to the server.
pub async fn read_stream<S: AsRef<str>>(
    connection: GrpcClient,
    options: &ReadStreamOptions,
    stream: S,
    count: u64,
) -> crate::Result<ReadStream> {
    use streams::read_req::options::stream_options::RevisionOption;
    use streams::read_req::options::{self, StreamOption, StreamOptions};
    use streams::read_req::Options;

    let read_direction = match options.direction {
        ReadDirection::Forward => 0,
        ReadDirection::Backward => 1,
    };

    let revision_option = match options.position {
        StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        StreamPosition::Start => RevisionOption::Start(Empty {}),
        StreamPosition::End => RevisionOption::End(Empty {}),
    };

    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let stream_options = StreamOptions {
        stream_identifier,
        revision_option: Some(revision_option),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let req_options = Options {
        stream_option: Some(StreamOption::Stream(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Count(count)),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let req = streams::ReadReq {
        options: Some(req_options),
    };

    let req = new_request(connection.connection_settings(), options, req);
    let handle = connection.current_selected_node().await?;
    let channel_id = handle.id();
    let mut client = StreamsClient::new(handle.channel);

    match client.read(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(&connection.sender, channel_id, &e).await;

            Err(e)
        }

        Ok(resp) => Ok(ReadStream {
            sender: connection.sender.clone(),
            channel_id,
            inner: resp.into_inner(),
        }),
    }
}

pub async fn read_all(
    connection: GrpcClient,
    options: &ReadAllOptions,
    count: u64,
) -> crate::Result<ReadStream> {
    use streams::read_req::options::all_options::AllOption;
    use streams::read_req::options::{self, AllOptions, StreamOption};
    use streams::read_req::Options;

    let read_direction = match options.direction {
        ReadDirection::Forward => 0,
        ReadDirection::Backward => 1,
    };

    let all_option = match options.position {
        StreamPosition::Position(pos) => {
            let pos = options::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            };

            AllOption::Position(pos)
        }

        StreamPosition::Start => AllOption::Start(Empty {}),
        StreamPosition::End => AllOption::End(Empty {}),
    };

    let stream_options = AllOptions {
        all_option: Some(all_option),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let req_options = Options {
        stream_option: Some(StreamOption::All(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Count(count)),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let req = streams::ReadReq {
        options: Some(req_options),
    };

    let req = new_request(connection.connection_settings(), options, req);
    let handle = connection.current_selected_node().await?;
    let channel_id = handle.id();
    let mut client = StreamsClient::new(handle.channel);

    match client.read(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(&connection.sender, channel_id, &e).await;

            Err(e)
        }

        Ok(resp) => Ok(ReadStream {
            sender: connection.sender.clone(),
            channel_id,
            inner: resp.into_inner(),
        }),
    }
}

/// Sends asynchronously the delete command to the server.
pub async fn delete_stream<S: AsRef<str>>(
    connection: &GrpcClient,
    stream: S,
    options: &DeleteStreamOptions,
) -> crate::Result<Option<Position>> {
    use streams::delete_req::options::ExpectedStreamRevision;
    use streams::delete_req::Options;
    use streams::delete_resp::PositionOption;

    let expected_stream_revision = match options.version {
        ExpectedRevision::Any => ExpectedStreamRevision::Any(Empty {}),
        ExpectedRevision::NoStream => ExpectedStreamRevision::NoStream(Empty {}),
        ExpectedRevision::StreamExists => ExpectedStreamRevision::StreamExists(Empty {}),
        ExpectedRevision::Exact(rev) => ExpectedStreamRevision::Revision(rev),
    };

    let expected_stream_revision = Some(expected_stream_revision);
    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let req_options = Options {
        stream_identifier,
        expected_stream_revision,
    };

    let req = new_request(
        connection.connection_settings(),
        options,
        streams::DeleteReq {
            options: Some(req_options),
        },
    );

    connection
        .execute(|channel| async {
            let mut client = StreamsClient::new(channel.channel);
            let result = client.delete(req).await?.into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::NoPosition(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        })
        .await
}

/// Sends asynchronously the tombstone command to the server.
pub async fn tombstone_stream<S: AsRef<str>>(
    connection: &GrpcClient,
    stream: S,
    options: &TombstoneStreamOptions,
) -> crate::Result<Option<Position>> {
    use streams::tombstone_req::options::ExpectedStreamRevision;
    use streams::tombstone_req::Options;
    use streams::tombstone_resp::PositionOption;

    let expected_stream_revision = match options.version {
        ExpectedRevision::Any => ExpectedStreamRevision::Any(Empty {}),
        ExpectedRevision::NoStream => ExpectedStreamRevision::NoStream(Empty {}),
        ExpectedRevision::StreamExists => ExpectedStreamRevision::StreamExists(Empty {}),
        ExpectedRevision::Exact(rev) => ExpectedStreamRevision::Revision(rev),
    };

    let expected_stream_revision = Some(expected_stream_revision);
    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let req_options = Options {
        stream_identifier,
        expected_stream_revision,
    };

    let req = new_request(
        connection.connection_settings(),
        options,
        streams::TombstoneReq {
            options: Some(req_options),
        },
    );

    connection
        .execute(|channel| async {
            let mut client = StreamsClient::new(channel.channel);
            let result = client.tombstone(req).await?.into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::NoPosition(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        })
        .await
}

pub struct Subscription {
    connection: GrpcClient,
    channel_id: uuid::Uuid,
    stream: Option<Streaming<crate::event_store::client::streams::ReadResp>>,
    attempts: usize,
    limit: usize,
    retry_enabled: bool,
    delay: std::time::Duration,
    options: streams::read_req::Options,
    metadata: tonic::metadata::MetadataMap,
}

impl Subscription {
    fn new(
        connection: GrpcClient,
        retry: Option<RetryOptions>,
        metadata: tonic::metadata::MetadataMap,
        options: streams::read_req::Options,
    ) -> Self {
        let (limit, delay, retry_enabled) = if let Some(retry) = retry {
            (retry.limit, retry.delay, true)
        } else {
            (1, Default::default(), false)
        };

        Self {
            connection,
            channel_id: uuid::Uuid::nil(),
            limit,
            delay,
            retry_enabled,
            options,
            stream: None,
            attempts: 1,
            metadata,
        }
    }

    pub fn into_stream_of_subscription_events(
        mut self,
    ) -> impl Stream<Item = crate::Result<SubscriptionEvent>> {
        try_stream! {
            loop {
                let event = self.next_subscription_event().await?;

                yield event;
            }
        }
    }

    pub fn into_stream(mut self) -> impl Stream<Item = crate::Result<ResolvedEvent>> {
        try_stream! {
            loop {
                let event = self.next().await?;
                yield event;
            }
        }
    }

    pub async fn next(&mut self) -> crate::Result<ResolvedEvent> {
        loop {
            if let SubscriptionEvent::EventAppeared(event) = self.next_subscription_event().await? {
                return Ok(event);
            }
        }
    }

    pub async fn next_subscription_event(&mut self) -> crate::Result<SubscriptionEvent> {
        use streams::read_req::options::all_options::AllOption;
        use streams::read_req::options::stream_options::RevisionOption;
        use streams::read_req::options::{self, StreamOption};

        loop {
            if let Some(mut stream) = self.stream.take() {
                match stream.try_next().await {
                    Err(status) => {
                        let e = crate::Error::from_grpc(status);
                        handle_error(&self.connection.sender, self.channel_id, &e).await;
                        self.attempts = 1;

                        error!("Subscription dropped. cause: {}", e);

                        if e.is_access_denied() || !self.retry_enabled {
                            return Err(e);
                        }
                    }

                    Ok(resp) => {
                        if let Some(content) = resp.and_then(|r| r.content) {
                            self.stream = Some(stream);

                            match content {
                                streams::read_resp::Content::Event(event) => {
                                    let event = convert_proto_read_event(event);

                                    let stream_options =
                                        self.options.stream_option.as_mut().unwrap();

                                    match stream_options {
                                        StreamOption::Stream(stream_options) => {
                                            let revision = RevisionOption::Revision(
                                                event.get_original_event().revision as u64,
                                            );
                                            stream_options.revision_option = Some(revision);
                                        }

                                        StreamOption::All(all_options) => {
                                            let position = event.get_original_event().position;
                                            let position = options::Position {
                                                prepare_position: position.prepare,
                                                commit_position: position.commit,
                                            };

                                            all_options.all_option =
                                                Some(AllOption::Position(position));
                                        }
                                    }

                                    return Ok(SubscriptionEvent::EventAppeared(event));
                                }

                                streams::read_resp::Content::Confirmation(info) => {
                                    return Ok(SubscriptionEvent::Confirmed(info.subscription_id))
                                }

                                streams::read_resp::Content::Checkpoint(chk) => {
                                    let position = Position {
                                        commit: chk.commit_position,
                                        prepare: chk.prepare_position,
                                    };

                                    return Ok(SubscriptionEvent::Checkpoint(position));
                                }

                                streams::read_resp::Content::FirstStreamPosition(event_number) => {
                                    return Ok(SubscriptionEvent::FirstStreamPosition(
                                        event_number,
                                    ));
                                }

                                streams::read_resp::Content::LastStreamPosition(event_number) => {
                                    return Ok(SubscriptionEvent::LastStreamPosition(event_number));
                                }

                                streams::read_resp::Content::LastAllStreamPosition(position) => {
                                    return Ok(SubscriptionEvent::LastAllPosition(Position {
                                        commit: position.commit_position,
                                        prepare: position.prepare_position,
                                    }));
                                }

                                _ => unreachable!(),
                            }
                        }

                        unreachable!()
                    }
                }
            } else {
                let handle = self.connection.current_selected_node().await?;

                self.channel_id = handle.id();

                let mut client = StreamsClient::new(handle.channel);
                let mut req = Request::new(streams::ReadReq {
                    options: Some(self.options.clone()),
                });

                *req.metadata_mut() = self.metadata.clone();

                match client.read(req).await {
                    Err(status) => {
                        let e = crate::Error::from_grpc(status);

                        handle_error(&self.connection.sender, self.channel_id, &e).await;

                        if !e.is_access_denied() && self.attempts < self.limit {
                            error!(
                                "Subscription: attempt ({}/{}) failure, cause: {}, retrying...",
                                self.attempts, self.limit, e
                            );
                            self.attempts += 1;
                            tokio::time::sleep(self.delay).await;

                            continue;
                        }

                        if !e.is_access_denied() && self.retry_enabled {
                            error!(
                                "Subscription: maximum retry threshold reached, cause: {}",
                                e
                            );
                        }

                        return Err(e);
                    }

                    Ok(stream) => {
                        self.stream = Some(stream.into_inner());

                        continue;
                    }
                }
            }
        }
    }
}

/// Runs the subscription command.
pub fn subscribe_to_stream<S: AsRef<str>>(
    connection: GrpcClient,
    stream_id: S,
    options: &SubscribeToStreamOptions,
) -> Subscription {
    use streams::read_req::options::stream_options::RevisionOption;
    use streams::read_req::options::{self, StreamOption, StreamOptions, SubscriptionOptions};
    use streams::read_req::Options;

    let retry = options.retry.as_ref().cloned();
    let read_direction = 0; // <- Going forward.

    let revision = match options.position {
        StreamPosition::Start => RevisionOption::Start(Empty {}),
        StreamPosition::End => RevisionOption::End(Empty {}),
        StreamPosition::Position(revision) => RevisionOption::Revision(revision),
    };

    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream_id.as_ref().to_string().into_bytes(),
    });
    let stream_options = StreamOptions {
        stream_identifier,
        revision_option: Some(revision),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let req_options = Options {
        stream_option: Some(StreamOption::Stream(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Subscription(SubscriptionOptions {})),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let metadata = build_request_metadata(
        connection.connection_settings(),
        options.common_operation_options(),
    );
    Subscription::new(connection, retry, metadata, req_options)
}

pub fn subscribe_to_all(connection: GrpcClient, options: &SubscribeToAllOptions) -> Subscription {
    use streams::read_req::options::all_options::AllOption;
    use streams::read_req::options::{self, AllOptions, StreamOption, SubscriptionOptions};
    use streams::read_req::Options;

    let retry = options.retry.as_ref().cloned();
    let read_direction = 0; // <- Going forward.

    let revision = match options.position {
        StreamPosition::Start => AllOption::Start(Empty {}),
        StreamPosition::Position(pos) => AllOption::Position(options::Position {
            prepare_position: pos.prepare,
            commit_position: pos.commit,
        }),
        StreamPosition::End => AllOption::End(Empty {}),
    };

    let stream_options = AllOptions {
        all_option: Some(revision),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let filter_option = match options.filter.as_ref() {
        Some(filter) => options::FilterOption::Filter(filter_into_proto(filter.clone())),
        None => options::FilterOption::NoFilter(Empty {}),
    };

    let req_options = Options {
        stream_option: Some(StreamOption::All(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(filter_option),
        count_option: Some(options::CountOption::Subscription(SubscriptionOptions {})),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let metadata = build_request_metadata(
        connection.connection_settings(),
        options.common_operation_options(),
    );
    Subscription::new(connection, retry, metadata, req_options)
}

/// This trait is used to avoid code duplication when introducing persistent subscription to $all. It
/// allows us to re-use most of regular persistent subscription code.
pub(crate) trait PsSettings: crate::options::Options {
    type Pos: PsPosition;

    fn settings(&self) -> &PersistentSubscriptionSettings<Self::Pos>;

    fn to_create_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption;

    fn to_update_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption;
}

impl PsSettings for PersistentSubscriptionOptions {
    type Pos = u64;

    fn settings(&self) -> &PersistentSubscriptionSettings<Self::Pos> {
        &self.setts
    }

    fn to_create_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption {
        use persistent::create_req::{
            options::StreamOption, stream_options::RevisionOption, StreamOptions,
        };

        let revision_option = match self.setts.start_from {
            StreamPosition::Start => RevisionOption::Start(Empty {}),
            StreamPosition::End => RevisionOption::End(Empty {}),
            StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        };

        StreamOption::Stream(StreamOptions {
            stream_identifier: Some(stream_identifier),
            revision_option: Some(revision_option),
        })
    }

    fn to_update_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption {
        use persistent::update_req::{
            options::StreamOption, stream_options::RevisionOption, StreamOptions,
        };

        let revision_option = match self.setts.start_from {
            StreamPosition::Start => RevisionOption::Start(Empty {}),
            StreamPosition::End => RevisionOption::End(Empty {}),
            StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        };

        StreamOption::Stream(StreamOptions {
            stream_identifier: Some(stream_identifier),
            revision_option: Some(revision_option),
        })
    }
}

impl PsSettings for PersistentSubscriptionToAllOptions {
    type Pos = Position;

    fn settings(&self) -> &PersistentSubscriptionSettings<Self::Pos> {
        &self.setts
    }

    fn to_create_options(
        &self,
        _stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption {
        use persistent::create_req::{
            self,
            all_options::{self, AllOption},
            options::StreamOption,
            AllOptions,
        };

        let filter_option = match self.filter.as_ref() {
            Some(filter) => all_options::FilterOption::Filter(ps_create_filter_into_proto(filter)),
            None => all_options::FilterOption::NoFilter(Empty {}),
        };

        let all_option = match self.setts.start_from {
            StreamPosition::Start => AllOption::Start(Empty {}),
            StreamPosition::End => AllOption::End(Empty {}),
            StreamPosition::Position(pos) => AllOption::Position(create_req::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            }),
        };

        StreamOption::All(AllOptions {
            filter_option: Some(filter_option),
            all_option: Some(all_option),
        })
    }

    fn to_update_options(
        &self,
        _stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption {
        use persistent::update_req::{
            self, all_options::AllOption, options::StreamOption, AllOptions,
        };

        let all_option = match self.setts.start_from {
            StreamPosition::Start => AllOption::Start(Empty {}),
            StreamPosition::End => AllOption::End(Empty {}),
            StreamPosition::Position(pos) => AllOption::Position(update_req::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            }),
        };

        StreamOption::All(AllOptions {
            all_option: Some(all_option),
        })
    }
}

pub(crate) async fn create_persistent_subscription<S: AsRef<str>, Options>(
    connection: &GrpcClient,
    stream: S,
    group: S,
    options: &Options,
) -> crate::Result<()>
where
    Options: PsSettings,
{
    use persistent::create_req::Options;
    use persistent::CreateReq;

    let handle = connection.current_selected_node().await?;
    let settings = convert_settings_create(options.settings())?;
    let stream_identifier = StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    };

    let req_options = options.to_create_options(stream_identifier.clone());
    let is_to_all = matches!(
        req_options,
        persistent::create_req::options::StreamOption::All(_)
    );

    if is_to_all && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL) {
        return Err(crate::Error::UnsupportedFeature);
    }

    #[allow(deprecated)]
    let req_options = Options {
        stream_option: Some(req_options),
        stream_identifier: Some(stream_identifier),
        group_name: group.as_ref().to_string(),
        settings: Some(settings),
    };

    let req = CreateReq {
        options: Some(req_options),
    };

    let req = new_request(connection.connection_settings(), options, req);

    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);
    if let Err(e) = client.create(req).await {
        let e = crate::Error::from_grpc(e);
        handle_error(&connection.sender, id, &e).await;

        return Err(e);
    };

    Ok(())
}

pub(crate) async fn update_persistent_subscription<S: AsRef<str>, Options>(
    connection: &GrpcClient,
    stream: S,
    group: S,
    options: &Options,
) -> crate::Result<()>
where
    Options: PsSettings,
{
    use persistent::update_req::Options;
    use persistent::UpdateReq;

    let handle = connection.current_selected_node().await?;
    let settings = convert_settings_update(options.settings())?;
    let stream_identifier = StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    };

    let req_options = options.to_update_options(stream_identifier.clone());
    let is_to_all = matches!(
        req_options,
        persistent::update_req::options::StreamOption::All(_)
    );

    if is_to_all && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL) {
        return Err(crate::Error::UnsupportedFeature);
    }

    #[allow(deprecated)]
    let req_options = Options {
        group_name: group.as_ref().to_string(),
        stream_option: Some(req_options),
        stream_identifier: Some(stream_identifier),
        settings: Some(settings),
    };

    let req = UpdateReq {
        options: Some(req_options),
    };

    let req = new_request(connection.connection_settings(), options, req);
    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);
    if let Err(e) = client.update(req).await {
        let e = crate::Error::from_grpc(e);
        handle_error(&connection.sender, id, &e).await;

        return Err(e);
    }

    Ok(())
}

pub async fn delete_persistent_subscription<S: AsRef<str>>(
    connection: &GrpcClient,
    stream_id: S,
    group_name: S,
    options: &DeletePersistentSubscriptionOptions,
    to_all: bool,
) -> crate::Result<()> {
    use persistent::delete_req::{options::StreamOption, Options};

    let handle = connection.current_selected_node().await?;

    if to_all && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL) {
        return Err(crate::Error::UnsupportedFeature);
    };

    let stream_option = if !to_all {
        StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_id.as_ref().to_string().into_bytes(),
        })
    } else {
        StreamOption::All(Empty {})
    };

    let stream_option = Some(stream_option);

    let req_options = Options {
        stream_option,
        group_name: group_name.as_ref().to_string(),
    };

    let req = persistent::DeleteReq {
        options: Some(req_options),
    };

    let req = new_request(connection.connection_settings(), options, req);
    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);

    if let Err(e) = client.delete(req).await {
        let e = crate::Error::from_grpc(e);
        handle_error(&connection.sender, id, &e).await;

        return Err(e);
    }

    Ok(())
}

/// Sends the persistent subscription connection request to the server
/// asynchronously even if the subscription is available right away.
pub async fn subscribe_to_persistent_subscription<S: AsRef<str>>(
    connection: &GrpcClient,
    stream_id: S,
    group_name: S,
    options: &SubscribeToPersistentSubscriptionOptions,
    to_all: bool,
) -> crate::Result<PersistentSubscription> {
    use futures::channel::mpsc;
    use futures::sink::SinkExt;
    use persistent::read_req::options::{self, UuidOption};
    use persistent::read_req::{self, options::StreamOption, Options};
    use persistent::ReadReq;

    let handle = connection.current_selected_node().await?;

    if to_all && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL) {
        return Err(crate::Error::UnsupportedFeature);
    }

    let (mut sender, recv) = mpsc::channel(500);

    let uuid_option = UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let stream_option = if !to_all {
        StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_id.as_ref().to_string().into_bytes(),
        })
    } else {
        StreamOption::All(Empty {})
    };

    let stream_option = Some(stream_option);

    let req_options = Options {
        stream_option,
        group_name: group_name.as_ref().to_string(),
        buffer_size: options.buffer_size as i32,
        uuid_option: Some(uuid_option),
    };

    let read_req = ReadReq {
        content: Some(read_req::Content::Options(req_options)),
    };

    let req = new_request(connection.connection_settings(), options, recv);

    let _ = sender.send(read_req).await;
    let stream_id = stream_id.as_ref().to_string();
    let group_name = group_name.as_ref().to_string();
    let channel_id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);

    match client.read(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(&connection.sender, channel_id, &e).await;

            Err(e)
        }
        Ok(resp) => Ok(PersistentSubscription {
            sender: connection.sender.clone(),
            ack_sender: sender,
            channel_id,
            inner: resp.into_inner(),
            stream_id,
            group_name,
        }),
    }
}

pub struct PersistentSubscription {
    sender: futures::channel::mpsc::UnboundedSender<Msg>,
    ack_sender: futures::channel::mpsc::Sender<crate::event_store::client::persistent::ReadReq>,
    channel_id: uuid::Uuid,
    inner: tonic::Streaming<crate::event_store::client::persistent::ReadResp>,
    stream_id: String,
    group_name: String,
}

impl PersistentSubscription {
    pub async fn next_subscription_event(&mut self) -> crate::Result<PersistentSubscriptionEvent> {
        match self.inner.try_next().await {
            Err(status) => {
                if let Some("persistent-subscription-dropped") = status
                    .metadata()
                    .get("exception")
                    .and_then(|e| e.to_str().ok())
                {
                    let message = format!(
                        "Persistent subscription on '{}' and group '{}' has dropped",
                        self.stream_id, self.group_name,
                    );

                    return Err(crate::Error::IllegalStateError(message));
                }

                let e = crate::Error::from_grpc(status);
                handle_error(&self.sender, self.channel_id, &e).await;

                Err(e)
            }
            Ok(resp) => {
                if let Some(content) = resp.and_then(|r| r.content) {
                    match content {
                        crate::event_store::client::persistent::read_resp::Content::Event(
                            event,
                        ) => {
                            return Ok(convert_persistent_proto_read_event(event));
                        }

                        crate::event_store::client::persistent::read_resp::Content::SubscriptionConfirmation(sub) => {
                            return Ok(PersistentSubscriptionEvent::Confirmed(sub.subscription_id));
                        }
                    }
                }

                unreachable!()
            }
        }
    }

    pub async fn next(&mut self) -> crate::Result<ResolvedEvent> {
        loop {
            let event = self.next_subscription_event().await?;

            if let PersistentSubscriptionEvent::EventAppeared { event, .. } = event {
                return Ok(event);
            }
        }
    }

    pub async fn ack(&mut self, event: ResolvedEvent) -> crate::Result<()> {
        self.ack_ids(vec![event.get_original_event().id]).await
    }

    pub async fn ack_ids<I>(&mut self, event_ids: I) -> crate::Result<()>
    where
        I: IntoIterator<Item = uuid::Uuid>,
    {
        use futures::sink::SinkExt;
        use persistent::read_req::{Ack, Content};
        use persistent::ReadReq;

        let ids = event_ids.into_iter().map(to_proto_uuid).collect();
        let ack = Ack {
            id: Vec::new(),
            ids,
        };

        let content = Content::Ack(ack);
        let read_req = ReadReq {
            content: Some(content),
        };

        self.ack_sender.send(read_req).await.map_err(|_| {
            crate::Error::IllegalStateError(
                "Ack was ignored as the transaction no longer exists".to_string(),
            )
        })
    }

    pub async fn nack(
        &mut self,
        event: ResolvedEvent,
        action: NakAction,
        reason: impl AsRef<str>,
    ) -> crate::Result<()> {
        self.nack_ids(vec![event.get_original_event().id], action, reason)
            .await
    }

    pub async fn nack_ids<I>(
        &mut self,
        event_ids: I,
        action: NakAction,
        reason: impl AsRef<str>,
    ) -> crate::Result<()>
    where
        I: IntoIterator<Item = uuid::Uuid>,
    {
        use futures::sink::SinkExt;
        use persistent::read_req::{Content, Nack};
        use persistent::ReadReq;

        let ids = event_ids.into_iter().map(to_proto_uuid).collect();

        let action = match action {
            NakAction::Unknown => 0,
            NakAction::Park => 1,
            NakAction::Retry => 2,
            NakAction::Skip => 3,
            NakAction::Stop => 4,
        };

        let nack = Nack {
            id: Vec::new(),
            ids,
            action,
            reason: reason.as_ref().to_string(),
        };

        let content = Content::Nack(nack);
        let read_req = ReadReq {
            content: Some(content),
        };

        self.ack_sender.send(read_req).await.map_err(|_| {
            crate::Error::IllegalStateError(
                "Ack was ignored as the transaction no longer exists".to_string(),
            )
        })
    }
}
fn to_proto_uuid(id: uuid::Uuid) -> Uuid {
    Uuid {
        value: Some(shared::uuid::Value::String(format!("{}", id))),
    }
}

pub(crate) struct RegularStream(pub(crate) String);
pub(crate) struct AllStream;
pub(crate) struct BothTypeOfStream;

pub(crate) trait StreamPositionTypeSelector {
    type Value;

    fn select(&self, value: RevisionOrPosition) -> Self::Value;
}

pub(crate) trait StreamKind {
    fn is_all(&self) -> bool;
    fn name(&self) -> &str;
}

impl StreamPositionTypeSelector for AllStream {
    type Value = Position;

    fn select(&self, value: RevisionOrPosition) -> Self::Value {
        match value {
            RevisionOrPosition::Position(p) => p,
            _ => unreachable!(),
        }
    }
}

impl StreamKind for AllStream {
    fn is_all(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "$all"
    }
}

impl StreamKind for RegularStream {
    fn is_all(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        self.0.as_str()
    }
}

impl StreamPositionTypeSelector for RegularStream {
    type Value = u64;

    fn select(&self, value: RevisionOrPosition) -> Self::Value {
        match value {
            RevisionOrPosition::Revision(r) => r,
            _ => unreachable!(),
        }
    }
}

impl StreamPositionTypeSelector for BothTypeOfStream {
    type Value = RevisionOrPosition;

    fn select(&self, value: RevisionOrPosition) -> Self::Value {
        value
    }
}

pub async fn list_all_persistent_subscriptions(
    connection: &GrpcClient,
    http_client: &reqwest::Client,
    op_options: &ListPersistentSubscriptionsOptions,
) -> crate::Result<Vec<PersistentSubscriptionInfo<RevisionOrPosition>>> {
    use crate::event_store::generated::persistent::list_req;

    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::PERSISTENT_SUBSCRIPTION_MANAGEMENT) {
        return crate::http::persistent_subscriptions::list_all_persistent_subscriptions(
            &handle,
            http_client,
            connection.connection_settings(),
            op_options,
        )
        .await;
    }

    let list_option = list_req::options::ListOption::ListAllSubscriptions(Empty {});

    internal_list_persistent_subscriptions(
        connection.connection_settings(),
        handle,
        op_options,
        BothTypeOfStream,
        list_option,
    )
    .await
}

pub(crate) async fn list_persistent_subscriptions_for_stream<StreamName>(
    connection: &GrpcClient,
    http_client: &reqwest::Client,
    stream_name: StreamName,
    op_options: &ListPersistentSubscriptionsOptions,
) -> crate::Result<Vec<PersistentSubscriptionInfo<<StreamName as StreamPositionTypeSelector>::Value>>>
where
    StreamName: StreamKind + StreamPositionTypeSelector,
{
    use crate::event_store::generated::persistent::list_req;

    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::PERSISTENT_SUBSCRIPTION_MANAGEMENT) {
        if stream_name.is_all()
            && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL)
        {
            return Err(crate::Error::UnsupportedFeature);
        }

        return crate::http::persistent_subscriptions::list_persistent_subscriptions_for_stream(
            &handle,
            http_client,
            connection.connection_settings(),
            stream_name,
            op_options,
        )
        .await;
    }

    let stream_option = if stream_name.is_all() {
        list_req::stream_option::StreamOption::All(Empty {})
    } else {
        list_req::stream_option::StreamOption::Stream(StreamIdentifier {
            stream_name: stream_name.name().to_string().into_bytes(),
        })
    };

    let list_option = list_req::options::ListOption::ListForStream(list_req::StreamOption {
        stream_option: Some(stream_option),
    });

    internal_list_persistent_subscriptions(
        connection.connection_settings(),
        handle,
        op_options,
        stream_name,
        list_option,
    )
    .await
}

async fn internal_list_persistent_subscriptions<Selector>(
    settings: &ClientSettings,
    handle: Handle,
    op_options: &ListPersistentSubscriptionsOptions,
    selector: Selector,
    list_option: crate::event_store::generated::persistent::list_req::options::ListOption,
) -> crate::Result<Vec<PersistentSubscriptionInfo<<Selector as StreamPositionTypeSelector>::Value>>>
where
    Selector: StreamPositionTypeSelector,
{
    use crate::event_store::generated::persistent::{list_req, ListReq};

    let options = list_req::Options {
        list_option: Some(list_option),
    };
    let req = ListReq {
        options: Some(options),
    };

    let req = new_request(settings, op_options, req);
    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel.clone());

    match client.list(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(handle.sender(), id, &e).await;

            Err(e)
        }

        Ok(resp) => {
            let resp = resp.into_inner();
            let mut infos = Vec::with_capacity(resp.subscriptions.capacity());

            for sub in resp.subscriptions {
                infos.push(subscription_info_from_wire(&selector, sub)?);
            }

            Ok(infos)
        }
    }
}

fn parse_position_start_from(input: &str) -> nom::IResult<&str, Position> {
    use nom::bytes::complete::tag;

    let (input, _) = tag("C:")(input)?;
    let (input, commit) = nom::character::complete::u64(input)?;
    let (input, _) = tag("/P:")(input)?;
    let (input, prepare) = nom::character::complete::u64(input)?;

    Ok((input, Position { commit, prepare }))
}

pub(crate) fn parse_revision_or_position(input: &str) -> crate::Result<RevisionOrPosition> {
    if let Ok(v) = input.parse::<u64>() {
        Ok(RevisionOrPosition::Revision(v))
    } else if let Ok(pos) = parse_position(input) {
        Ok(RevisionOrPosition::Position(pos))
    } else {
        Err(crate::Error::InternalParsingError(format!(
            "Failed to parse either a stream revision or a transaction log position: '{}'",
            input
        )))
    }
}

pub(crate) fn parse_position(input: &str) -> crate::Result<Position> {
    if let Ok((_, pos)) = nom::combinator::complete(parse_position_start_from)(input) {
        Ok(pos)
    } else {
        Err(crate::Error::InternalParsingError(format!(
            "Failed to parse a transaction log position: '{}'",
            input
        )))
    }
}

pub(crate) fn parse_stream_position(
    input: &str,
) -> crate::Result<StreamPosition<RevisionOrPosition>> {
    if input == "-1" || input == "C:-1/P:-1" {
        return Ok(StreamPosition::End);
    } else if input == "0" || input == "C:0/P:0" {
        return Ok(StreamPosition::Start);
    }

    Ok(StreamPosition::Position(parse_revision_or_position(input)?))
}

fn subscription_info_from_wire<Selector>(
    selector: &Selector,
    info: crate::event_store::generated::persistent::SubscriptionInfo,
) -> crate::Result<PersistentSubscriptionInfo<<Selector as StreamPositionTypeSelector>::Value>>
where
    Selector: StreamPositionTypeSelector,
{
    let named_consumer_strategy = SystemConsumerStrategy::from_string(info.named_consumer_strategy);
    let connections = info
        .connections
        .into_iter()
        .map(connection_from_wire)
        .collect();
    let status = info.status;
    let mut last_known_position = None;
    let mut last_known_event_number = None;
    let mut last_processed_event_number = None;
    let mut last_processed_position = None;

    if let Ok(value) = parse_revision_or_position(info.last_known_event_position.as_str()) {
        match value {
            RevisionOrPosition::Position(p) => last_known_position = Some(p),
            RevisionOrPosition::Revision(r) => last_known_event_number = Some(r),
        }
    }

    if let Ok(value) = parse_revision_or_position(info.last_checkpointed_event_position.as_str()) {
        match value {
            RevisionOrPosition::Position(p) => last_processed_position = Some(p),
            RevisionOrPosition::Revision(r) => last_processed_event_number = Some(r),
        }
    }
    let start_from = if let Ok(pos) = parse_stream_position(info.start_from.as_str()) {
        pos.map(|p| selector.select(p))
    } else {
        return Err(crate::Error::InternalParsingError(format!(
            "Can't parse stream position: {}",
            info.start_from
        )));
    };

    let stats = PersistentSubscriptionStats {
        average_per_second: info.average_per_second as f64,
        total_items: info.total_items as usize,
        count_since_last_measurement: info.count_since_last_measurement as usize,
        last_checkpointed_event_revision: last_processed_event_number,
        last_known_event_revision: last_known_event_number,
        last_checkpointed_position: last_processed_position,
        last_known_position,
        read_buffer_count: info.read_buffer_count as usize,
        live_buffer_count: info.live_buffer_count as usize,
        retry_buffer_count: info.retry_buffer_count as usize,
        total_in_flight_messages: info.total_in_flight_messages as usize,
        outstanding_messages_count: info.outstanding_messages_count as usize,
        parked_message_count: info.parked_message_count as usize,
    };

    Ok(PersistentSubscriptionInfo {
        event_source: info.event_source,
        group_name: info.group_name,
        status,
        connections,
        settings: Some(PersistentSubscriptionSettings {
            resolve_link_tos: info.resolve_link_tos,
            start_from,
            extra_statistics: info.extra_statistics,
            message_timeout: Duration::from_millis(info.message_timeout_milliseconds as u64),
            max_retry_count: info.max_retry_count,
            live_buffer_size: info.live_buffer_size,
            read_batch_size: info.read_batch_size,
            history_buffer_size: info.buffer_size,
            checkpoint_after: Duration::from_millis(info.check_point_after_milliseconds as u64),
            checkpoint_lower_bound: info.min_check_point_count,
            checkpoint_upper_bound: info.max_check_point_count,
            max_subscriber_count: info.max_subscriber_count,
            consumer_strategy_name: named_consumer_strategy,
        }),
        stats,
    })
}

fn connection_from_wire(
    conn: crate::event_store::generated::persistent::subscription_info::ConnectionInfo,
) -> PersistentSubscriptionConnectionInfo {
    PersistentSubscriptionConnectionInfo {
        from: conn.from,
        username: conn.username,
        average_items_per_second: conn.average_items_per_second as f64,
        total_items: conn.total_items as usize,
        count_since_last_measurement: conn.count_since_last_measurement as usize,
        available_slots: conn.available_slots as usize,
        in_flight_messages: conn.in_flight_messages as usize,
        connection_name: conn.connection_name,
        extra_statistics: PersistentSubscriptionMeasurements(
            conn.observed_measurements
                .into_iter()
                .map(|m| (m.key, m.value))
                .collect(),
        ),
    }
}

pub(crate) async fn replay_parked_messages<StreamName>(
    connection: &GrpcClient,
    http_client: &reqwest::Client,
    stream_name: StreamName,
    group_name: impl AsRef<str>,
    op_options: &ReplayParkedMessagesOptions,
) -> crate::Result<()>
where
    StreamName: StreamKind,
{
    use crate::event_store::generated::persistent::{replay_parked_req, ReplayParkedReq};

    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::PERSISTENT_SUBSCRIPTION_MANAGEMENT) {
        if stream_name.is_all()
            && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL)
        {
            return Err(crate::Error::UnsupportedFeature);
        }

        return crate::http::persistent_subscriptions::replay_parked_messages(
            &handle,
            http_client,
            connection.connection_settings(),
            stream_name.name(),
            group_name,
            op_options,
        )
        .await;
    }

    let stream_option = if stream_name.is_all() {
        replay_parked_req::options::StreamOption::All(Empty {})
    } else {
        replay_parked_req::options::StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_name.name().to_string().into_bytes(),
        })
    };

    let stop_at_option = if let Some(value) = op_options.stop_at {
        replay_parked_req::options::StopAtOption::StopAt(value as i64)
    } else {
        replay_parked_req::options::StopAtOption::NoLimit(Empty {})
    };

    let options = replay_parked_req::Options {
        group_name: group_name.as_ref().to_string(),
        stream_option: Some(stream_option),
        stop_at_option: Some(stop_at_option),
    };

    let req = ReplayParkedReq {
        options: Some(options),
    };

    let req = new_request(connection.connection_settings(), op_options, req);
    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);
    if let Err(e) = client.replay_parked(req).await {
        let e = crate::Error::from_grpc(e);
        handle_error(&connection.sender, id, &e).await;

        return Err(e);
    }

    Ok(())
}

pub(crate) async fn get_persistent_subscription_info<StreamName>(
    connection: &GrpcClient,
    http_client: &reqwest::Client,
    stream_name: StreamName,
    group_name: impl AsRef<str>,
    op_options: &GetPersistentSubscriptionInfoOptions,
) -> crate::Result<PersistentSubscriptionInfo<<StreamName as StreamPositionTypeSelector>::Value>>
where
    StreamName: StreamKind + StreamPositionTypeSelector,
{
    use crate::event_store::generated::persistent::{get_info_req, GetInfoReq};
    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::PERSISTENT_SUBSCRIPTION_MANAGEMENT) {
        if stream_name.is_all()
            && !handle.supports_feature(Features::PERSISTENT_SUBSCRIPITON_TO_ALL)
        {
            return Err(crate::Error::UnsupportedFeature);
        }

        return crate::http::persistent_subscriptions::get_persistent_subscription_info(
            &handle,
            http_client,
            connection.connection_settings(),
            stream_name,
            group_name,
            op_options,
        )
        .await;
    }

    let stream_option = if stream_name.is_all() {
        get_info_req::options::StreamOption::All(Empty {})
    } else {
        get_info_req::options::StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_name.name().to_string().into_bytes(),
        })
    };
    let options = get_info_req::Options {
        group_name: group_name.as_ref().to_string(),
        stream_option: Some(stream_option),
    };
    let req = GetInfoReq {
        options: Some(options),
    };
    let req = new_request(connection.connection_settings(), op_options, req);

    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);

    match client.get_info(req).await {
        Err(e) => {
            let e = crate::Error::from_grpc(e);
            handle_error(&connection.sender, id, &e).await;

            Err(e)
        }

        Ok(resp) => {
            let resp = resp.into_inner();

            if let Some(info) = resp.subscription_info {
                subscription_info_from_wire(&stream_name, info)
            } else {
                Err(crate::Error::ResourceNotFound)
            }
        }
    }
}

pub async fn restart_persistent_subscription_subsystem(
    connection: &GrpcClient,
    http_client: &reqwest::Client,
    op_options: &RestartPersistentSubscriptionSubsystem,
) -> crate::Result<()> {
    let handle = connection.current_selected_node().await?;

    if !handle.supports_feature(Features::PERSISTENT_SUBSCRIPTION_MANAGEMENT) {
        return crate::http::persistent_subscriptions::restart_persistent_subscription_subsystem(
            &handle,
            http_client,
            connection.connection_settings(),
            op_options,
        )
        .await;
    }

    let id = handle.id();
    let mut client = PersistentSubscriptionsClient::new(handle.channel);
    let req = new_request(connection.connection_settings(), op_options, Empty {});

    if let Err(e) = client.restart_subsystem(req).await {
        let e = crate::Error::from_grpc(e);
        handle_error(&connection.sender, id, &e).await;

        return Err(e);
    }

    Ok(())
}
