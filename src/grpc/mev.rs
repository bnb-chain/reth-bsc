//! BEP-675 BidBlock gRPC ingress compatible with go-bsc.

use crate::grpc::mev_proto::{
    bid_block_service_server::{BidBlockService, BidBlockServiceServer},
    BidBlockRequest, BidBlockResponse,
};
use crate::metrics::BscMevGrpcMetrics;
use crate::node::miner::bid_block::{BidBlock, BidBlockArgs};
use crate::rpc::mev::MevApiImpl;
use alloy_primitives::{Bytes as AlloyBytes, B256};
use alloy_rlp::Decodable;
use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt;
use jsonrpsee::types::ErrorObjectOwned;
use prost::Message;
use std::any::Any;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::codegen::{http, BoxFuture, Service};
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Server;
use tonic::{Code, Request, Response, Status};
use tonic_health::ServingStatus;

/// `/mev.v1.BidBlockService/SendBidBlock`, matching go-bsc's public protobuf contract.
pub const SEND_BID_BLOCK_METHOD: &str = "/mev.v1.BidBlockService/SendBidBlock";

/// BSC `params.MaxBlockSize`, mirrored explicitly because reth has no equivalent protocol
/// constant. Keep this synchronized with go-bsc when upgrading the compatibility baseline.
const BSC_MAX_BLOCK_SIZE: usize = 8 * 1024 * 1024;
/// go-bsc allows twice `params.MaxBlockSize` for the protobuf envelope.
pub const MAX_MEV_GRPC_MESSAGE_SIZE: usize = 2 * BSC_MAX_BLOCK_SIZE;
// Small decoder headroom lets the handler translate a protobuf envelope just over the public
// limit into go-bsc's ResourceExhausted status. The transport still has a hard allocation cap.
const MEV_GRPC_DECODER_HEADROOM: usize = 4 * 1024;
const MEV_GRPC_STREAM_HEADROOM: u32 = 16;
const MEV_GRPC_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const MEV_ERROR_DOMAIN: &str = "mev.bnbchain.org";
const JSON_RPC_ERROR_DATA_KEY: &str = "json_rpc_error_data";

/// Runtime controls for the optional gRPC listener.
#[derive(Debug, Clone, Copy)]
pub struct MevGrpcConfig {
    pub concurrency: u32,
    pub request_timeout: Duration,
}

impl MevGrpcConfig {
    pub fn new(concurrency: u32, request_timeout: Duration) -> Self {
        Self {
            concurrency: if concurrency == 0 { 32 } else { concurrency },
            request_timeout: if request_timeout.is_zero() {
                Duration::from_secs(10)
            } else {
                request_timeout
            },
        }
    }
}

/// Transport-independent seam used to guarantee JSON-RPC and gRPC call the same admission code.
#[async_trait]
trait BidBlockSubmitter: Send + Sync {
    async fn submit_bid_block(&self, args: BidBlockArgs) -> Result<B256, ErrorObjectOwned>;
}

#[async_trait]
impl BidBlockSubmitter for MevApiImpl {
    async fn submit_bid_block(&self, args: BidBlockArgs) -> Result<B256, ErrorObjectOwned> {
        MevApiImpl::submit_bid_block(self, args).await
    }
}

#[derive(Clone)]
struct MevGrpcApi {
    submitter: Arc<dyn BidBlockSubmitter>,
    semaphore: Arc<Semaphore>,
    metrics: BscMevGrpcMetrics,
}

impl MevGrpcApi {
    fn new(submitter: Arc<dyn BidBlockSubmitter>, concurrency: u32) -> Self {
        Self {
            submitter,
            semaphore: Arc::new(Semaphore::new(concurrency as usize)),
            metrics: BscMevGrpcMetrics::default(),
        }
    }

    fn acquire(&self) -> Result<Arc<ActiveRequest>, Status> {
        let permit = Arc::clone(&self.semaphore).try_acquire_owned().map_err(|_| {
            self.metrics.rejected_total.increment(1);
            Status::resource_exhausted("concurrency limit reached")
        })?;
        self.metrics.active_requests.increment(1);
        Ok(Arc::new(ActiveRequest { _permit: permit, metrics: self.metrics.clone() }))
    }
}

/// Admission runs as a service interceptor, before tonic reads or decodes the protobuf body.
/// It wraps only BidBlockService, so health checks remain available while BidBlock is saturated.
#[derive(Clone)]
struct MevGrpcAdmission {
    api: MevGrpcApi,
}

impl Interceptor for MevGrpcAdmission {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.extensions_mut().insert(self.api.acquire()?);
        Ok(request)
    }
}

/// Per-BidBlock transport safety wrapper. Admission is outside this wrapper, so its timeout starts
/// before protobuf body decoding; health is registered separately and bypasses both timeout and
/// panic accounting.
#[derive(Clone)]
struct MevGrpcSafety<S> {
    inner: S,
    timeout: Duration,
    metrics: BscMevGrpcMetrics,
}

impl<S> MevGrpcSafety<S> {
    fn new(inner: S, timeout: Duration, metrics: BscMevGrpcMetrics) -> Self {
        Self { inner, timeout, metrics }
    }
}

impl<S: tonic::server::NamedService> tonic::server::NamedService for MevGrpcSafety<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for MevGrpcSafety<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: Default + Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<ReqBody>) -> Self::Future {
        let future = match std::panic::catch_unwind(AssertUnwindSafe(|| self.inner.call(request))) {
            Ok(future) => future,
            Err(panic) => {
                let response = grpc_panic_response::<ResBody>(&self.metrics, panic);
                return Box::pin(async move { Ok(response) });
            }
        };
        let timeout = self.timeout;
        let metrics = self.metrics.clone();

        Box::pin(async move {
            match tokio::time::timeout(timeout, AssertUnwindSafe(future).catch_unwind()).await {
                Ok(Ok(result)) => result,
                Ok(Err(panic)) => Ok(grpc_panic_response::<ResBody>(&metrics, panic)),
                Err(_) => {
                    metrics.errors_total.increment(1);
                    Ok(Status::deadline_exceeded("request timeout").into_http::<ResBody>())
                }
            }
        })
    }
}

fn grpc_panic_response<B: Default>(
    metrics: &BscMevGrpcMetrics,
    panic: Box<dyn Any + Send + 'static>,
) -> http::Response<B> {
    metrics.errors_total.increment(1);
    let panic = if let Some(message) = panic.downcast_ref::<String>() {
        message.as_str()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        message
    } else {
        "non-string panic payload"
    };
    tracing::error!(
        method = SEND_BID_BLOCK_METHOD,
        panic,
        backtrace = %std::backtrace::Backtrace::force_capture(),
        "panic in MEV gRPC handler"
    );
    Status::internal("internal error").into_http::<B>()
}

struct ActiveRequest {
    _permit: OwnedSemaphorePermit,
    metrics: BscMevGrpcMetrics,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.metrics.active_requests.decrement(1);
    }
}

struct HandlerTimer {
    started: Instant,
    metrics: BscMevGrpcMetrics,
}

impl Drop for HandlerTimer {
    fn drop(&mut self) {
        self.metrics.handler_duration_seconds.record(self.started.elapsed().as_secs_f64());
    }
}

#[tonic::async_trait]
impl BidBlockService for MevGrpcApi {
    async fn send_bid_block(
        &self,
        mut request: Request<BidBlockRequest>,
    ) -> Result<Response<BidBlockResponse>, Status> {
        // Direct unit calls do not traverse the transport interceptor; keep the fallback so the
        // transport-independent handler is still testable without weakening production admission.
        let _active = match request.extensions_mut().remove::<Arc<ActiveRequest>>() {
            Some(active) => active,
            None => self.acquire()?,
        };
        self.metrics.requests_total.increment(1);
        let _timer = HandlerTimer { started: Instant::now(), metrics: self.metrics.clone() };

        let request = request.into_inner();
        tracing::info!(
            transport = "grpc",
            payload_bytes = request.bid_block_rlp.len(),
            signature_bytes = request.signature.len(),
            validator_host_name = %request.validator_host_name,
            "[BID BLOCK GRPC RECEIVED]"
        );
        self.metrics.payload_size_bytes.record(request.bid_block_rlp.len() as f64);
        if request.encoded_len() > MAX_MEV_GRPC_MESSAGE_SIZE {
            self.metrics.errors_total.increment(1);
            return Err(Status::resource_exhausted("message exceeds maximum size"));
        }
        if request.bid_block_rlp.is_empty() {
            self.metrics.errors_total.increment(1);
            return Err(mev_status(-38001, "empty BidBlock RLP"));
        }

        let decode_started = Instant::now();
        let mut encoded = request.bid_block_rlp.as_slice();
        let bid_block = BidBlock::decode(&mut encoded).map_err(|_| {
            self.metrics.errors_total.increment(1);
            mev_status(-38001, "invalid BidBlock RLP")
        })?;
        self.metrics.decode_duration_seconds.record(decode_started.elapsed().as_secs_f64());
        if !encoded.is_empty() {
            self.metrics.errors_total.increment(1);
            return Err(mev_status(-38001, "invalid BidBlock RLP"));
        }

        tracing::info!(
            transport = "grpc",
            block = bid_block.header.number,
            txs = bid_block.transactions.len(),
            sidecars = bid_block.sidecars.len(),
            "[BID BLOCK GRPC DECODED]"
        );

        let block_number = bid_block.header.number;

        // validator_host_name is a sentry routing hint. Like go-bsc, a validator ignores it.
        let args = BidBlockArgs { bid_block, signature: AlloyBytes::from(request.signature) };
        let bid_hash = self.submitter.submit_bid_block(args).await.map_err(|err| {
            self.metrics.errors_total.increment(1);
            rpc_error_to_status(&err)
        })?;

        tracing::info!(
            transport = "grpc",
            block = block_number,
            bid_hash = %bid_hash,
            "[BID BLOCK GRPC ACCEPTED]"
        );

        Ok(Response::new(BidBlockResponse { bid_hash: bid_hash.as_slice().to_vec() }))
    }
}

/// Running gRPC listener. [`Self::shutdown`] drains gracefully; dropping is a startup-failure
/// safety net that signals shutdown and aborts the task so a listener can never detach silently.
pub struct MevGrpcServerHandle {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), tonic::transport::Error>>>,
}

impl MevGrpcServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(mut task) = self.task.take() {
            if tokio::time::timeout(MEV_GRPC_SHUTDOWN_TIMEOUT, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for MevGrpcServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Bind and start the optional MEV gRPC service. Binding is synchronous so address conflicts fail
/// node startup rather than surfacing later in a detached task.
pub fn start_mev_grpc_server(
    listen_addr: SocketAddr,
    config: MevGrpcConfig,
    api: Arc<MevApiImpl>,
) -> eyre::Result<MevGrpcServerHandle> {
    start_mev_grpc_server_with_submitter(listen_addr, config, api)
}

fn start_mev_grpc_server_with_submitter(
    listen_addr: SocketAddr,
    config: MevGrpcConfig,
    submitter: Arc<dyn BidBlockSubmitter>,
) -> eyre::Result<MevGrpcServerHandle> {
    let listener = std::net::TcpListener::bind(listen_addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let listener = tokio::net::TcpListener::from_std(listener)?;

    let config = MevGrpcConfig::new(config.concurrency, config.request_timeout);
    let service = MevGrpcApi::new(submitter, config.concurrency);
    let admission = MevGrpcAdmission { api: service.clone() };
    let safety_metrics = service.metrics.clone();
    let bid_block_service = BidBlockServiceServer::new(service)
        .max_decoding_message_size(MAX_MEV_GRPC_MESSAGE_SIZE + MEV_GRPC_DECODER_HEADROOM)
        .max_encoding_message_size(MAX_MEV_GRPC_MESSAGE_SIZE);
    let bid_block_service =
        MevGrpcSafety::new(bid_block_service, config.request_timeout, safety_metrics);
    let bid_block_service = InterceptedService::new(bid_block_service, admission);
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let incoming = TcpListenerStream::new(listener);
    let max_streams = config.concurrency.saturating_add(MEV_GRPC_STREAM_HEADROOM);

    let task = tokio::spawn(async move {
        health_reporter.set_service_status("", ServingStatus::Serving).await;
        health_reporter.set_serving::<BidBlockServiceServer<MevGrpcApi>>().await;
        let shutdown = async move {
            let _ = shutdown_rx.await;
            health_reporter.set_service_status("", ServingStatus::NotServing).await;
            health_reporter.set_not_serving::<BidBlockServiceServer<MevGrpcApi>>().await;
        };
        let result = Server::builder()
            .max_concurrent_streams(max_streams)
            .add_service(health_service)
            .add_service(bid_block_service)
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await;
        if let Err(error) = &result {
            tracing::error!(%error, "MEV gRPC server stopped unexpectedly");
        }
        result
    });

    tracing::info!(
        %local_addr,
        concurrency = config.concurrency,
        max_streams,
        timeout_ms = config.request_timeout.as_millis(),
        "MEV gRPC server started"
    );
    Ok(MevGrpcServerHandle { local_addr, shutdown: Some(shutdown_tx), task: Some(task) })
}

fn rpc_error_to_status(error: &ErrorObjectOwned) -> Status {
    let code = error.code();
    let grpc_code = match code {
        -38001 | -38002 | -38007 => Code::InvalidArgument,
        -38003 => Code::Unavailable,
        -38004 => Code::ResourceExhausted,
        -38005 => Code::FailedPrecondition,
        -38006 => Code::PermissionDenied,
        -38008 => Code::DeadlineExceeded,
        _ => Code::Unknown,
    };
    let mut metadata = HashMap::new();
    if let Some(data) = error.data() {
        metadata.insert(JSON_RPC_ERROR_DATA_KEY.to_string(), data.get().to_string());
    }
    status_with_error_info(grpc_code, error.message(), code, metadata)
}

fn mev_status(json_rpc_code: i32, message: impl Into<String>) -> Status {
    let message = message.into();
    let grpc_code = match json_rpc_code {
        -38001 | -38002 | -38007 => Code::InvalidArgument,
        -38003 => Code::Unavailable,
        -38004 => Code::ResourceExhausted,
        -38005 => Code::FailedPrecondition,
        -38006 => Code::PermissionDenied,
        -38008 => Code::DeadlineExceeded,
        _ => Code::Unknown,
    };
    status_with_error_info(grpc_code, &message, json_rpc_code, HashMap::new())
}

fn status_with_error_info(
    grpc_code: Code,
    message: &str,
    json_rpc_code: i32,
    metadata: HashMap<String, String>,
) -> Status {
    let error_info = ErrorInfo {
        reason: json_rpc_code.to_string(),
        domain: MEV_ERROR_DOMAIN.to_string(),
        metadata,
    };
    let rpc_status = GoogleRpcStatus {
        code: grpc_code as i32,
        message: message.to_string(),
        details: vec![ProtoAny {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_string(),
            value: error_info.encode_to_vec(),
        }],
    };
    Status::with_details(grpc_code, message.to_string(), Bytes::from(rpc_status.encode_to_vec()))
}

#[derive(Clone, PartialEq, Message)]
struct GoogleRpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<ProtoAny>,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoAny {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct ErrorInfo {
    #[prost(string, tag = "1")]
    reason: String,
    #[prost(string, tag = "2")]
    domain: String,
    #[prost(map = "string, string", tag = "3")]
    metadata: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use std::sync::Mutex;
    use tonic_health::pb::{
        health_check_response::ServingStatus as PbServingStatus, health_client::HealthClient,
        HealthCheckRequest,
    };

    struct MockSubmitter {
        args: Mutex<Option<BidBlockArgs>>,
        result: Result<B256, ErrorObjectOwned>,
    }

    struct PanicSubmitter;

    struct SlowSubmitter {
        delay: Duration,
    }

    #[async_trait]
    impl BidBlockSubmitter for MockSubmitter {
        async fn submit_bid_block(&self, args: BidBlockArgs) -> Result<B256, ErrorObjectOwned> {
            *self.args.lock().unwrap() = Some(args);
            self.result.clone()
        }
    }

    #[async_trait]
    impl BidBlockSubmitter for PanicSubmitter {
        async fn submit_bid_block(&self, _args: BidBlockArgs) -> Result<B256, ErrorObjectOwned> {
            panic!("intentional gRPC handler panic")
        }
    }

    #[async_trait]
    impl BidBlockSubmitter for SlowSubmitter {
        async fn submit_bid_block(&self, _args: BidBlockArgs) -> Result<B256, ErrorObjectOwned> {
            tokio::time::sleep(self.delay).await;
            Ok(B256::ZERO)
        }
    }

    fn request_block() -> BidBlock {
        BidBlock {
            header: Header {
                number: 2,
                gas_limit: 140_000_000,
                gas_used: 21_000,
                ..Default::default()
            },
            transactions: vec![AlloyBytes::from_static(&[1, 2, 3])],
            sidecars: Vec::new(),
        }
    }

    #[tokio::test]
    async fn grpc_handler_decodes_rlp_and_forwards_signature() {
        let block = request_block();
        let encoded = alloy_rlp::encode(&block);
        let expected_hash = B256::repeat_byte(0x12);
        let submitter =
            Arc::new(MockSubmitter { args: Mutex::new(None), result: Ok(expected_hash) });
        let service = MevGrpcApi::new(submitter.clone(), 1);

        let response = service
            .send_bid_block(Request::new(BidBlockRequest {
                bid_block_rlp: encoded,
                signature: vec![0xaa, 0xbb],
                validator_host_name: "ignored-by-validator".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.bid_hash, expected_hash.as_slice());
        let args = submitter.args.lock().unwrap().take().unwrap();
        assert_eq!(args.bid_block, block);
        assert_eq!(args.signature.as_ref(), &[0xaa, 0xbb]);
    }

    #[tokio::test]
    async fn grpc_handler_rejects_empty_invalid_and_trailing_rlp() {
        let submitter = Arc::new(MockSubmitter { args: Mutex::new(None), result: Ok(B256::ZERO) });
        let service = MevGrpcApi::new(submitter.clone(), 1);

        for payload in [Vec::new(), vec![0xff], {
            let mut encoded = alloy_rlp::encode(&request_block());
            encoded.push(0x80);
            encoded
        }] {
            let error = service
                .send_bid_block(Request::new(BidBlockRequest {
                    bid_block_rlp: payload,
                    ..Default::default()
                }))
                .await
                .unwrap_err();
            assert_eq!(error.code(), Code::InvalidArgument);
        }
        assert!(submitter.args.lock().unwrap().is_none());
    }

    #[test]
    fn grpc_admission_rejects_before_message_decode() {
        let submitter = Arc::new(MockSubmitter { args: Mutex::new(None), result: Ok(B256::ZERO) });
        let mut admission = MevGrpcAdmission { api: MevGrpcApi::new(submitter, 1) };

        let admitted = admission.call(Request::new(())).unwrap();
        let rejected = admission.call(Request::new(())).unwrap_err();
        assert_eq!(rejected.code(), Code::ResourceExhausted);

        drop(admitted);
        assert!(admission.call(Request::new(())).is_ok());
    }

    #[tokio::test]
    async fn grpc_listener_serves_bidblock_and_health() {
        let block = request_block();
        let expected_hash = B256::repeat_byte(0x34);
        let submitter =
            Arc::new(MockSubmitter { args: Mutex::new(None), result: Ok(expected_hash) });
        let handle = start_mev_grpc_server_with_submitter(
            "127.0.0.1:0".parse().unwrap(),
            MevGrpcConfig::new(2, Duration::from_secs(1)),
            submitter,
        )
        .unwrap();
        let endpoint = format!("http://{}", handle.local_addr());

        let channel =
            tonic::transport::Endpoint::from_shared(endpoint).unwrap().connect().await.unwrap();
        let mut health = HealthClient::new(channel.clone());
        let health_response =
            health.check(HealthCheckRequest { service: String::new() }).await.unwrap().into_inner();
        assert_eq!(health_response.status, PbServingStatus::Serving as i32);

        let mut client =
            crate::grpc::mev_proto::bid_block_service_client::BidBlockServiceClient::new(channel);
        let response = client
            .send_bid_block(BidBlockRequest {
                bid_block_rlp: alloy_rlp::encode(&block),
                signature: vec![0xaa; 65],
                validator_host_name: String::new(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.bid_hash, expected_hash.as_slice());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn grpc_listener_rejects_oversized_payload_before_business_call() {
        let submitter = Arc::new(MockSubmitter { args: Mutex::new(None), result: Ok(B256::ZERO) });
        let handle = start_mev_grpc_server_with_submitter(
            "127.0.0.1:0".parse().unwrap(),
            MevGrpcConfig::new(1, Duration::from_secs(1)),
            submitter.clone(),
        )
        .unwrap();
        let endpoint = format!("http://{}", handle.local_addr());
        let channel =
            tonic::transport::Endpoint::from_shared(endpoint).unwrap().connect().await.unwrap();
        let mut client =
            crate::grpc::mev_proto::bid_block_service_client::BidBlockServiceClient::new(channel);

        let error = client
            .send_bid_block(BidBlockRequest {
                bid_block_rlp: vec![0; MAX_MEV_GRPC_MESSAGE_SIZE],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert!(submitter.args.lock().unwrap().is_none());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn grpc_listener_recovers_handler_panic_and_stays_healthy() {
        let handle = start_mev_grpc_server_with_submitter(
            "127.0.0.1:0".parse().unwrap(),
            MevGrpcConfig::new(1, Duration::from_secs(1)),
            Arc::new(PanicSubmitter),
        )
        .unwrap();
        let endpoint = format!("http://{}", handle.local_addr());
        let channel =
            tonic::transport::Endpoint::from_shared(endpoint).unwrap().connect().await.unwrap();
        let mut client =
            crate::grpc::mev_proto::bid_block_service_client::BidBlockServiceClient::new(
                channel.clone(),
            );

        let error = client
            .send_bid_block(BidBlockRequest {
                bid_block_rlp: alloy_rlp::encode(&request_block()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::Internal);

        let mut health = HealthClient::new(channel);
        let response =
            health.check(HealthCheckRequest { service: String::new() }).await.unwrap().into_inner();
        assert_eq!(response.status, PbServingStatus::Serving as i32);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn grpc_timeout_is_bidblock_only_and_releases_admission() {
        let handle = start_mev_grpc_server_with_submitter(
            "127.0.0.1:0".parse().unwrap(),
            MevGrpcConfig::new(1, Duration::from_millis(20)),
            Arc::new(SlowSubmitter { delay: Duration::from_secs(1) }),
        )
        .unwrap();
        let endpoint = format!("http://{}", handle.local_addr());
        let channel =
            tonic::transport::Endpoint::from_shared(endpoint).unwrap().connect().await.unwrap();
        let request = BidBlockRequest {
            bid_block_rlp: alloy_rlp::encode(&request_block()),
            ..Default::default()
        };
        let mut client =
            crate::grpc::mev_proto::bid_block_service_client::BidBlockServiceClient::new(
                channel.clone(),
            );

        for _ in 0..2 {
            let error = client.send_bid_block(request.clone()).await.unwrap_err();
            assert_eq!(error.code(), Code::DeadlineExceeded);
        }

        let mut health = HealthClient::new(channel);
        let response =
            health.check(HealthCheckRequest { service: String::new() }).await.unwrap().into_inner();
        assert_eq!(response.status, PbServingStatus::Serving as i32);

        handle.shutdown().await;
    }

    #[test]
    fn grpc_error_preserves_json_rpc_code_as_error_info() {
        let error =
            ErrorObjectOwned::owned(-38006, "revoked", Some(serde_json::json!({ "retry": false })));
        let status = rpc_error_to_status(&error);
        assert_eq!(status.code(), Code::PermissionDenied);

        let rpc_status = GoogleRpcStatus::decode(status.details()).unwrap();
        assert_eq!(rpc_status.code, Code::PermissionDenied as i32);
        assert_eq!(rpc_status.details.len(), 1);
        let info = ErrorInfo::decode(rpc_status.details[0].value.as_slice()).unwrap();
        assert_eq!(info.reason, "-38006");
        assert_eq!(info.domain, MEV_ERROR_DOMAIN);
        assert_eq!(info.metadata[JSON_RPC_ERROR_DATA_KEY], r#"{"retry":false}"#);
    }

    #[test]
    fn configured_zero_values_use_geth_defaults() {
        let config = MevGrpcConfig::new(0, Duration::ZERO);
        assert_eq!(config.concurrency, 32);
        assert_eq!(config.request_timeout, Duration::from_secs(10));
        assert_eq!(SEND_BID_BLOCK_METHOD, "/mev.v1.BidBlockService/SendBidBlock");
    }

    #[test]
    fn message_size_limit_matches_go_bsc_params() {
        assert_eq!(BSC_MAX_BLOCK_SIZE, 8_388_608);
        assert_eq!(MAX_MEV_GRPC_MESSAGE_SIZE, 16_777_216);
    }
}
