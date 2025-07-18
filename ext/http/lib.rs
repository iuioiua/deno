// Copyright 2018-2025 the Deno authors. MIT license.

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::min;
use std::error::Error;
use std::future::Future;
use std::future::Pending;
use std::future::pending;
use std::io;
use std::io::Write;
use std::mem::replace;
use std::mem::take;
use std::net::SocketAddr;
use std::pin::Pin;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use async_compression::Level;
use async_compression::tokio::write::BrotliEncoder;
use async_compression::tokio::write::GzipEncoder;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use cache_control::CacheControl;
use deno_core::AsyncRefCell;
use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CancelTryFuture;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::StringOrBuffer;
use deno_core::futures::FutureExt;
use deno_core::futures::StreamExt;
use deno_core::futures::TryFutureExt;
use deno_core::futures::channel::mpsc;
use deno_core::futures::channel::oneshot;
use deno_core::futures::future::Either;
use deno_core::futures::future::RemoteHandle;
use deno_core::futures::future::Shared;
use deno_core::futures::future::select;
use deno_core::futures::never::Never;
use deno_core::futures::stream::Peekable;
use deno_core::op2;
use deno_core::unsync::spawn;
use deno_error::JsErrorBox;
use deno_net::raw::NetworkStream;
use deno_telemetry::Histogram;
use deno_telemetry::MeterProvider;
use deno_telemetry::OTEL_GLOBALS;
use deno_telemetry::UpDownCounter;
use deno_websocket::ws_create_server_stream;
use flate2::Compression;
use flate2::write::GzEncoder;
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper_util::rt::TokioIo;
use hyper_v014::Body;
use hyper_v014::HeaderMap;
use hyper_v014::Request;
use hyper_v014::Response;
use hyper_v014::body::Bytes;
use hyper_v014::body::HttpBody;
use hyper_v014::body::SizeHint;
use hyper_v014::header::HeaderName;
use hyper_v014::header::HeaderValue;
use hyper_v014::server::conn::Http;
use hyper_v014::service::Service;
use once_cell::sync::OnceCell;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::network_buffered_stream::NetworkBufferedStream;
use crate::reader_stream::ExternallyAbortableReaderStream;
use crate::reader_stream::ShutdownHandle;

pub mod compressible;
mod fly_accept_encoding;
mod http_next;
mod network_buffered_stream;
mod reader_stream;
mod request_body;
mod request_properties;
mod response_body;
mod service;
mod websocket_upgrade;

use fly_accept_encoding::Encoding;
pub use http_next::HttpNextError;
pub use request_properties::DefaultHttpPropertyExtractor;
pub use request_properties::HttpConnectionProperties;
pub use request_properties::HttpListenProperties;
pub use request_properties::HttpPropertyExtractor;
pub use request_properties::HttpRequestProperties;
pub use service::UpgradeUnavailableError;
pub use websocket_upgrade::WebSocketUpgradeError;

struct OtelCollectors {
  duration: Histogram<f64>,
  active_requests: UpDownCounter<i64>,
  request_size: Histogram<u64>,
  response_size: Histogram<u64>,
}

static OTEL_COLLECTORS: OnceCell<OtelCollectors> = OnceCell::new();

#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
  /// By passing a hook function, the caller can customize various configuration
  /// options for the HTTP/2 server.
  /// See [`http2::Builder`] for what parameters can be customized.
  ///
  /// If `None`, the default configuration provided by hyper will be used. Note
  /// that the default configuration is subject to change in future versions.
  pub http2_builder_hook:
    Option<fn(http2::Builder<LocalExecutor>) -> http2::Builder<LocalExecutor>>,
  /// By passing a hook function, the caller can customize various configuration
  /// options for the HTTP/1 server.
  /// See [`http1::Builder`] for what parameters can be customized.
  ///
  /// If `None`, the default configuration provided by hyper will be used. Note
  /// that the default configuration is subject to change in future versions.
  pub http1_builder_hook: Option<fn(http1::Builder) -> http1::Builder>,

  /// If `false`, the server will abort the request when the response is dropped.
  pub no_legacy_abort: bool,
}

#[cfg(not(feature = "default_property_extractor"))]
deno_core::extension!(
  deno_http,
  deps = [deno_web, deno_net, deno_fetch, deno_websocket],
  parameters = [ HTTP: HttpPropertyExtractor ],
  ops = [
    op_http_accept,
    op_http_headers,
    op_http_serve_address_override,
    op_http_shutdown,
    op_http_upgrade_websocket,
    op_http_websocket_accept_header,
    op_http_write_headers,
    op_http_write_resource,
    op_http_write,
    http_next::op_http_close_after_finish,
    http_next::op_http_get_request_header,
    http_next::op_http_get_request_headers,
    http_next::op_http_request_on_cancel,
    http_next::op_http_get_request_method_and_url<HTTP>,
    http_next::op_http_get_request_cancelled,
    http_next::op_http_read_request_body,
    http_next::op_http_serve_on<HTTP>,
    http_next::op_http_serve<HTTP>,
    http_next::op_http_set_promise_complete,
    http_next::op_http_set_response_body_bytes,
    http_next::op_http_set_response_body_resource,
    http_next::op_http_set_response_body_text,
    http_next::op_http_set_response_header,
    http_next::op_http_set_response_headers,
    http_next::op_http_set_response_trailers,
    http_next::op_http_upgrade_websocket_next,
    http_next::op_http_upgrade_raw,
    http_next::op_raw_write_vectored,
    http_next::op_can_write_vectored,
    http_next::op_http_try_wait,
    http_next::op_http_wait,
    http_next::op_http_close,
    http_next::op_http_cancel,
    http_next::op_http_metric_handle_otel_error,
  ],
  esm = ["00_serve.ts", "01_http.js", "02_websocket.ts"],
  options = {
    options: Options,
  },
  state = |state, options| {
    state.put::<Options>(options.options);
  }
);

#[cfg(feature = "default_property_extractor")]
deno_core::extension!(
  deno_http,
  deps = [deno_web, deno_net, deno_fetch, deno_websocket],
  ops = [
    op_http_accept,
    op_http_headers,
    op_http_serve_address_override,
    op_http_shutdown,
    op_http_upgrade_websocket,
    op_http_websocket_accept_header,
    op_http_write_headers,
    op_http_write_resource,
    op_http_write,
    op_http_notify_serving,
    http_next::op_http_close_after_finish,
    http_next::op_http_get_request_header,
    http_next::op_http_get_request_headers,
    http_next::op_http_request_on_cancel,
    http_next::op_http_get_request_method_and_url<DefaultHttpPropertyExtractor>,
    http_next::op_http_get_request_cancelled,
    http_next::op_http_read_request_body,
    http_next::op_http_serve_on<DefaultHttpPropertyExtractor>,
    http_next::op_http_serve<DefaultHttpPropertyExtractor>,
    http_next::op_http_set_promise_complete,
    http_next::op_http_set_response_body_bytes,
    http_next::op_http_set_response_body_resource,
    http_next::op_http_set_response_body_text,
    http_next::op_http_set_response_header,
    http_next::op_http_set_response_headers,
    http_next::op_http_set_response_trailers,
    http_next::op_http_upgrade_websocket_next,
    http_next::op_http_upgrade_raw,
    http_next::op_raw_write_vectored,
    http_next::op_can_write_vectored,
    http_next::op_http_try_wait,
    http_next::op_http_wait,
    http_next::op_http_close,
    http_next::op_http_cancel,
    http_next::op_http_metric_handle_otel_error,
  ],
  esm = ["00_serve.ts", "01_http.js", "02_websocket.ts"],
  options = {
    options: Options,
  },
  state = |state, options| {
    state.put::<Options>(options.options);
  }
);

#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum HttpError {
  #[class(inherit)]
  #[error(transparent)]
  Resource(#[from] deno_core::error::ResourceError),
  #[class(inherit)]
  #[error(transparent)]
  Canceled(#[from] deno_core::Canceled),
  #[class("Http")]
  #[error("{0}")]
  HyperV014(#[source] Arc<hyper_v014::Error>),
  #[class(generic)]
  #[error("{0}")]
  InvalidHeaderName(#[from] hyper_v014::header::InvalidHeaderName),
  #[class(generic)]
  #[error("{0}")]
  InvalidHeaderValue(#[from] hyper_v014::header::InvalidHeaderValue),
  #[class(generic)]
  #[error("{0}")]
  Http(#[from] hyper_v014::http::Error),
  #[class("Http")]
  #[error("response headers already sent")]
  ResponseHeadersAlreadySent,
  #[class("Http")]
  #[error("connection closed while sending response")]
  ConnectionClosedWhileSendingResponse,
  #[class("Http")]
  #[error("already in use")]
  AlreadyInUse,
  #[class(inherit)]
  #[error("{0}")]
  Io(#[from] std::io::Error),
  #[class("Http")]
  #[error("no response headers")]
  NoResponseHeaders,
  #[class("Http")]
  #[error("response already completed")]
  ResponseAlreadyCompleted,
  #[class("Http")]
  #[error("cannot upgrade because request body was used")]
  UpgradeBodyUsed,
  #[class("Http")]
  #[error(transparent)]
  Other(#[from] JsErrorBox),
}

pub enum HttpSocketAddr {
  IpSocket(std::net::SocketAddr),
  #[cfg(unix)]
  UnixSocket(tokio::net::unix::SocketAddr),
}

impl From<std::net::SocketAddr> for HttpSocketAddr {
  fn from(addr: std::net::SocketAddr) -> Self {
    Self::IpSocket(addr)
  }
}

#[cfg(unix)]
impl From<tokio::net::unix::SocketAddr> for HttpSocketAddr {
  fn from(addr: tokio::net::unix::SocketAddr) -> Self {
    Self::UnixSocket(addr)
  }
}

struct OtelInfo {
  attributes: OtelInfoAttributes,
  duration: Option<std::time::Instant>,
  request_size: Option<u64>,
  response_size: Option<u64>,
}

struct OtelInfoAttributes {
  http_request_method: Cow<'static, str>,
  network_protocol_version: &'static str,
  url_scheme: Cow<'static, str>,
  server_address: Option<String>,
  server_port: Option<i64>,
  error_type: Option<&'static str>,
  http_response_status_code: Option<i64>,
}

impl OtelInfoAttributes {
  fn method(method: &http::method::Method) -> Cow<'static, str> {
    use http::method::Method;

    match *method {
      Method::GET => Cow::Borrowed("GET"),
      Method::POST => Cow::Borrowed("POST"),
      Method::PUT => Cow::Borrowed("PUT"),
      Method::DELETE => Cow::Borrowed("DELETE"),
      Method::HEAD => Cow::Borrowed("HEAD"),
      Method::OPTIONS => Cow::Borrowed("OPTIONS"),
      Method::CONNECT => Cow::Borrowed("CONNECT"),
      Method::PATCH => Cow::Borrowed("PATCH"),
      Method::TRACE => Cow::Borrowed("TRACE"),
      _ => Cow::Owned(method.to_string()),
    }
  }

  fn method_v02(method: &http_v02::method::Method) -> Cow<'static, str> {
    use http_v02::method::Method;

    match *method {
      Method::GET => Cow::Borrowed("GET"),
      Method::POST => Cow::Borrowed("POST"),
      Method::PUT => Cow::Borrowed("PUT"),
      Method::DELETE => Cow::Borrowed("DELETE"),
      Method::HEAD => Cow::Borrowed("HEAD"),
      Method::OPTIONS => Cow::Borrowed("OPTIONS"),
      Method::CONNECT => Cow::Borrowed("CONNECT"),
      Method::PATCH => Cow::Borrowed("PATCH"),
      Method::TRACE => Cow::Borrowed("TRACE"),
      _ => Cow::Owned(method.to_string()),
    }
  }

  fn version(version: http::Version) -> &'static str {
    use http::Version;

    match version {
      Version::HTTP_09 => "0.9",
      Version::HTTP_10 => "1.0",
      Version::HTTP_11 => "1.1",
      Version::HTTP_2 => "2",
      Version::HTTP_3 => "3",
      _ => unreachable!(),
    }
  }

  fn version_v02(version: http_v02::Version) -> &'static str {
    use http_v02::Version;

    match version {
      Version::HTTP_09 => "0.9",
      Version::HTTP_10 => "1.0",
      Version::HTTP_11 => "1.1",
      Version::HTTP_2 => "2",
      Version::HTTP_3 => "3",
      _ => unreachable!(),
    }
  }

  fn for_counter(&self) -> Vec<deno_telemetry::KeyValue> {
    let mut attributes = vec![
      deno_telemetry::KeyValue::new(
        "http.request.method",
        self.http_request_method.clone(),
      ),
      deno_telemetry::KeyValue::new("url.scheme", self.url_scheme.clone()),
    ];

    if let Some(address) = self.server_address.clone() {
      attributes.push(deno_telemetry::KeyValue::new("server.address", address));
    }
    if let Some(port) = self.server_port {
      attributes.push(deno_telemetry::KeyValue::new("server.port", port));
    }

    attributes
  }

  fn for_histogram(&self) -> Vec<deno_telemetry::KeyValue> {
    let mut histogram_attributes = vec![
      deno_telemetry::KeyValue::new(
        "http.request.method",
        self.http_request_method.clone(),
      ),
      deno_telemetry::KeyValue::new("url.scheme", self.url_scheme.clone()),
      deno_telemetry::KeyValue::new(
        "network.protocol.version",
        self.network_protocol_version,
      ),
    ];

    if let Some(address) = self.server_address.clone() {
      histogram_attributes
        .push(deno_telemetry::KeyValue::new("server.address", address));
    }
    if let Some(port) = self.server_port {
      histogram_attributes
        .push(deno_telemetry::KeyValue::new("server.port", port));
    }
    if let Some(status_code) = self.http_response_status_code {
      histogram_attributes.push(deno_telemetry::KeyValue::new(
        "http.response.status_code",
        status_code,
      ));
    }

    if let Some(error) = self.error_type {
      histogram_attributes
        .push(deno_telemetry::KeyValue::new("error.type", error));
    }

    histogram_attributes
  }
}

impl OtelInfo {
  fn new(
    otel: &deno_telemetry::OtelGlobals,
    instant: std::time::Instant,
    request_size: u64,
    attributes: OtelInfoAttributes,
  ) -> Self {
    let collectors = OTEL_COLLECTORS.get_or_init(|| {
      let meter = otel
        .meter_provider
        .meter_with_scope(otel.builtin_instrumentation_scope.clone());

      let duration = meter
        .f64_histogram("http.server.request.duration")
        .with_unit("s")
        .with_description("Duration of HTTP server requests.")
        .with_boundaries(vec![
          0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0,
          7.5, 10.0,
        ])
        .build();

      let active_requests = meter
        .i64_up_down_counter("http.server.active_requests")
        .with_unit("{request}")
        .with_description("Number of active HTTP server requests.")
        .build();

      let request_size = meter
        .u64_histogram("http.server.request.body.size")
        .with_unit("By")
        .with_description("Size of HTTP server request bodies.")
        .with_boundaries(vec![
          0.0,
          100.0,
          1000.0,
          10000.0,
          100000.0,
          1000000.0,
          10000000.0,
          100000000.0,
          1000000000.0,
        ])
        .build();

      let response_size = meter
        .u64_histogram("http.server.response.body.size")
        .with_unit("By")
        .with_description("Size of HTTP server response bodies.")
        .with_boundaries(vec![
          0.0,
          100.0,
          1000.0,
          10000.0,
          100000.0,
          1000000.0,
          10000000.0,
          100000000.0,
          1000000000.0,
        ])
        .build();

      OtelCollectors {
        duration,
        active_requests,
        request_size,
        response_size,
      }
    });

    collectors.active_requests.add(1, &attributes.for_counter());

    Self {
      attributes,
      duration: Some(instant),
      request_size: Some(request_size),
      response_size: Some(0),
    }
  }

  fn handle_duration_and_request_size(&mut self) {
    let collectors = OTEL_COLLECTORS.get().unwrap();
    let attributes = self.attributes.for_histogram();

    if let Some(duration) = self.duration.take() {
      let duration = duration.elapsed();
      collectors
        .duration
        .record(duration.as_secs_f64(), &attributes);
    }

    if let Some(request_size) = self.request_size.take() {
      let collectors = OTEL_COLLECTORS.get().unwrap();
      collectors.request_size.record(request_size, &attributes);
    }
  }
}

impl Drop for OtelInfo {
  fn drop(&mut self) {
    let collectors = OTEL_COLLECTORS.get().unwrap();

    self.handle_duration_and_request_size();

    collectors
      .active_requests
      .add(-1, &self.attributes.for_counter());

    if let Some(response_size) = self.response_size {
      collectors
        .response_size
        .record(response_size, &self.attributes.for_histogram());
    }
  }
}

fn handle_error_otel(
  otel: &Option<Rc<RefCell<Option<OtelInfo>>>>,
  error: &HttpError,
) {
  if let Some(otel) = otel.as_ref() {
    let mut maybe_otel_info = otel.borrow_mut();
    if let Some(otel_info) = maybe_otel_info.as_mut() {
      otel_info.attributes.error_type = Some(match error {
        HttpError::Resource(_) => "resource",
        HttpError::Canceled(_) => "canceled",
        HttpError::HyperV014(_) => "hyper",
        HttpError::InvalidHeaderName(_) => "invalid header name",
        HttpError::InvalidHeaderValue(_) => "invalid header value",
        HttpError::Http(_) => "http",
        HttpError::ResponseHeadersAlreadySent => {
          "response headers already sent"
        }
        HttpError::ConnectionClosedWhileSendingResponse => {
          "connection closed while sending response"
        }
        HttpError::AlreadyInUse => "already in use",
        HttpError::Io(_) => "io",
        HttpError::NoResponseHeaders => "no response headers",
        HttpError::ResponseAlreadyCompleted => "response already completed",
        HttpError::UpgradeBodyUsed => "upgrade body used",
        HttpError::Other(_) => "unknown",
      });
    }
  }
}

struct HttpConnResource {
  addr: HttpSocketAddr,
  scheme: &'static str,
  acceptors_tx: mpsc::UnboundedSender<HttpAcceptor>,
  closed_fut: Shared<RemoteHandle<Result<(), Arc<hyper_v014::Error>>>>,
  cancel_handle: Rc<CancelHandle>, // Closes gracefully and cancels accept ops.
}

impl HttpConnResource {
  fn new<S>(io: S, scheme: &'static str, addr: HttpSocketAddr) -> Self
  where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
  {
    let (acceptors_tx, acceptors_rx) = mpsc::unbounded::<HttpAcceptor>();
    let service = HttpService::new(acceptors_rx);

    let conn_fut = Http::new()
      .with_executor(LocalExecutor)
      .serve_connection(io, service)
      .with_upgrades();

    // When the cancel handle is used, the connection shuts down gracefully.
    // No new HTTP streams will be accepted, but existing streams will be able
    // to continue operating and eventually shut down cleanly.
    let cancel_handle = CancelHandle::new_rc();
    let shutdown_fut = never().or_cancel(&cancel_handle).fuse();

    // A local task that polls the hyper connection future to completion.
    let task_fut = async move {
      let conn_fut = pin!(conn_fut);
      let shutdown_fut = pin!(shutdown_fut);
      let result = match select(conn_fut, shutdown_fut).await {
        Either::Left((result, _)) => result,
        Either::Right((_, mut conn_fut)) => {
          conn_fut.as_mut().graceful_shutdown();
          conn_fut.await
        }
      };
      filter_enotconn(result).map_err(Arc::from)
    };
    let (task_fut, closed_fut) = task_fut.remote_handle();
    let closed_fut = closed_fut.shared();
    spawn(task_fut);

    Self {
      addr,
      scheme,
      acceptors_tx,
      closed_fut,
      cancel_handle,
    }
  }

  // Accepts a new incoming HTTP request.
  async fn accept(
    self: &Rc<Self>,
  ) -> Result<
    Option<(
      HttpStreamReadResource,
      HttpStreamWriteResource,
      String,
      String,
    )>,
    HttpError,
  > {
    let fut = async {
      let (request_tx, request_rx) = oneshot::channel();
      let (response_tx, response_rx) = oneshot::channel();

      let otel_instant = OTEL_GLOBALS
        .get()
        .filter(|o| o.has_metrics())
        .map(|_| std::time::Instant::now());

      let acceptor = HttpAcceptor::new(request_tx, response_rx);
      self.acceptors_tx.unbounded_send(acceptor).ok()?;

      let request = request_rx.await.ok()?;
      let accept_encoding = {
        let encodings =
          fly_accept_encoding::encodings_iter_http_02(request.headers())
            .filter(|r| {
              matches!(r, Ok((Some(Encoding::Brotli | Encoding::Gzip), _)))
            });

        fly_accept_encoding::preferred(encodings)
          .ok()
          .flatten()
          .unwrap_or(Encoding::Identity)
      };

      let otel_info =
        OTEL_GLOBALS.get().filter(|o| o.has_metrics()).map(|otel| {
          let size_hint = request.size_hint();
          Rc::new(RefCell::new(Some(OtelInfo::new(
            otel,
            otel_instant.unwrap(),
            size_hint.upper().unwrap_or(size_hint.lower()),
            OtelInfoAttributes {
              http_request_method: OtelInfoAttributes::method_v02(
                request.method(),
              ),
              url_scheme: Cow::Borrowed(self.scheme),
              network_protocol_version: OtelInfoAttributes::version_v02(
                request.version(),
              ),
              server_address: request.uri().host().map(|host| host.to_string()),
              server_port: request.uri().port_u16().map(|port| port as i64),
              error_type: Default::default(),
              http_response_status_code: Default::default(),
            },
          ))))
        });

      let method = request.method().to_string();
      let url = req_url(&request, self.scheme, &self.addr);
      let read_stream =
        HttpStreamReadResource::new(self, request, otel_info.clone());
      let write_stream = HttpStreamWriteResource::new(
        self,
        response_tx,
        accept_encoding,
        otel_info,
      );
      Some((read_stream, write_stream, method, url))
    };

    async {
      match fut.await {
        Some(stream) => Ok(Some(stream)),
        // Return the connection error, if any.
        None => self.closed().map_ok(|_| None).await,
      }
    }
    .try_or_cancel(&self.cancel_handle)
    .await
  }

  /// A future that completes when this HTTP connection is closed or errors.
  async fn closed(&self) -> Result<(), HttpError> {
    self.closed_fut.clone().map_err(HttpError::HyperV014).await
  }
}

impl Resource for HttpConnResource {
  fn name(&self) -> Cow<str> {
    "httpConn".into()
  }

  fn close(self: Rc<Self>) {
    self.cancel_handle.cancel();
  }
}

/// Creates a new HttpConn resource which uses `io` as its transport.
pub fn http_create_conn_resource<S, A>(
  state: &mut OpState,
  io: S,
  addr: A,
  scheme: &'static str,
) -> ResourceId
where
  S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
  A: Into<HttpSocketAddr>,
{
  let conn = HttpConnResource::new(io, scheme, addr.into());
  state.resource_table.add(conn)
}

/// An object that implements the `hyper::Service` trait, through which Hyper
/// delivers incoming HTTP requests.
struct HttpService {
  acceptors_rx: Peekable<mpsc::UnboundedReceiver<HttpAcceptor>>,
}

impl HttpService {
  fn new(acceptors_rx: mpsc::UnboundedReceiver<HttpAcceptor>) -> Self {
    let acceptors_rx = acceptors_rx.peekable();
    Self { acceptors_rx }
  }
}

impl Service<Request<Body>> for HttpService {
  type Response = Response<Body>;
  type Error = oneshot::Canceled;
  type Future = oneshot::Receiver<Response<Body>>;

  fn poll_ready(
    &mut self,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), Self::Error>> {
    let acceptors_rx = Pin::new(&mut self.acceptors_rx);
    let result = ready!(acceptors_rx.poll_peek(cx))
      .map(|_| ())
      .ok_or(oneshot::Canceled);
    Poll::Ready(result)
  }

  fn call(&mut self, request: Request<Body>) -> Self::Future {
    let acceptor = self.acceptors_rx.next().now_or_never().flatten().unwrap();
    acceptor.call(request)
  }
}

/// A pair of one-shot channels which first transfer a HTTP request from the
/// Hyper service to the HttpConn resource, and then take the Response back to
/// the service.
struct HttpAcceptor {
  request_tx: oneshot::Sender<Request<Body>>,
  response_rx: oneshot::Receiver<Response<Body>>,
}

impl HttpAcceptor {
  fn new(
    request_tx: oneshot::Sender<Request<Body>>,
    response_rx: oneshot::Receiver<Response<Body>>,
  ) -> Self {
    Self {
      request_tx,
      response_rx,
    }
  }

  fn call(self, request: Request<Body>) -> oneshot::Receiver<Response<Body>> {
    let Self {
      request_tx,
      response_rx,
    } = self;
    request_tx
      .send(request)
      .map(|_| response_rx)
      .unwrap_or_else(|_| oneshot::channel().1) // Make new canceled receiver.
  }
}

pub struct HttpStreamReadResource {
  _conn: Rc<HttpConnResource>,
  pub rd: AsyncRefCell<HttpRequestReader>,
  cancel_handle: CancelHandle,
  size: SizeHint,
  otel_info: Option<Rc<RefCell<Option<OtelInfo>>>>,
}

pub struct HttpStreamWriteResource {
  conn: Rc<HttpConnResource>,
  wr: AsyncRefCell<HttpResponseWriter>,
  accept_encoding: Encoding,
  otel_info: Option<Rc<RefCell<Option<OtelInfo>>>>,
}

impl HttpStreamReadResource {
  fn new(
    conn: &Rc<HttpConnResource>,
    request: Request<Body>,
    otel_info: Option<Rc<RefCell<Option<OtelInfo>>>>,
  ) -> Self {
    let size = request.body().size_hint();
    Self {
      _conn: conn.clone(),
      rd: HttpRequestReader::Headers(request).into(),
      size,
      cancel_handle: CancelHandle::new(),
      otel_info,
    }
  }
}

impl Resource for HttpStreamReadResource {
  fn name(&self) -> Cow<str> {
    "httpReadStream".into()
  }

  fn read(self: Rc<Self>, limit: usize) -> AsyncResult<BufView> {
    Box::pin(async move {
      let mut rd = RcRef::map(&self, |r| &r.rd).borrow_mut().await;

      let body = loop {
        match &mut *rd {
          HttpRequestReader::Headers(_) => {}
          HttpRequestReader::Body(_, body) => break body,
          HttpRequestReader::Closed => return Ok(BufView::empty()),
        }
        match take(&mut *rd) {
          HttpRequestReader::Headers(request) => {
            let (parts, body) = request.into_parts();
            *rd = HttpRequestReader::Body(parts.headers, body.peekable());
          }
          _ => unreachable!(),
        };
      };

      let fut = async {
        let mut body = Pin::new(body);
        loop {
          match body.as_mut().peek_mut().await {
            Some(Ok(chunk)) if !chunk.is_empty() => {
              let len = min(limit, chunk.len());
              let buf = chunk.split_to(len);
              let view = BufView::from(buf);
              break Ok(view);
            }
            // This unwrap is safe because `peek_mut()` returned `Some`, and thus
            // currently has a peeked value that can be synchronously returned
            // from `next()`.
            //
            // The future returned from `next()` is always ready, so we can
            // safely call `await` on it without creating a race condition.
            Some(_) => match body.as_mut().next().await.unwrap() {
              Ok(chunk) => assert!(chunk.is_empty()),
              Err(err) => {
                break Err(JsErrorBox::from_err(HttpError::HyperV014(
                  Arc::new(err),
                )));
              }
            },
            None => break Ok(BufView::empty()),
          }
        }
      };

      let cancel_handle = RcRef::map(&self, |r| &r.cancel_handle);
      fut.try_or_cancel(cancel_handle).await
    })
  }

  fn close(self: Rc<Self>) {
    self.cancel_handle.cancel();
  }

  fn size_hint(&self) -> (u64, Option<u64>) {
    (self.size.lower(), self.size.upper())
  }
}

impl HttpStreamWriteResource {
  fn new(
    conn: &Rc<HttpConnResource>,
    response_tx: oneshot::Sender<Response<Body>>,
    accept_encoding: Encoding,
    otel_info: Option<Rc<RefCell<Option<OtelInfo>>>>,
  ) -> Self {
    Self {
      conn: conn.clone(),
      wr: HttpResponseWriter::Headers(response_tx).into(),
      accept_encoding,
      otel_info,
    }
  }
}

impl Resource for HttpStreamWriteResource {
  fn name(&self) -> Cow<str> {
    "httpWriteStream".into()
  }
}

/// The read half of an HTTP stream.
pub enum HttpRequestReader {
  Headers(Request<Body>),
  Body(HeaderMap<HeaderValue>, Peekable<Body>),
  Closed,
}

impl Default for HttpRequestReader {
  fn default() -> Self {
    Self::Closed
  }
}

/// The write half of an HTTP stream.
enum HttpResponseWriter {
  Headers(oneshot::Sender<Response<Body>>),
  Body {
    writer: Pin<Box<dyn tokio::io::AsyncWrite>>,
    shutdown_handle: ShutdownHandle,
  },
  BodyUncompressed(BodyUncompressedSender),
  Closed,
}

impl Default for HttpResponseWriter {
  fn default() -> Self {
    Self::Closed
  }
}

struct BodyUncompressedSender(Option<hyper_v014::body::Sender>);

impl BodyUncompressedSender {
  fn sender(&mut self) -> &mut hyper_v014::body::Sender {
    // This is safe because we only ever take the sender out of the option
    // inside of the shutdown method.
    self.0.as_mut().unwrap()
  }

  fn shutdown(mut self) {
    // take the sender out of self so that when self is dropped at the end of
    // this block, it doesn't get aborted
    self.0.take();
  }
}

impl From<hyper_v014::body::Sender> for BodyUncompressedSender {
  fn from(sender: hyper_v014::body::Sender) -> Self {
    BodyUncompressedSender(Some(sender))
  }
}

impl Drop for BodyUncompressedSender {
  fn drop(&mut self) {
    if let Some(sender) = self.0.take() {
      sender.abort();
    }
  }
}

// We use a tuple instead of struct to avoid serialization overhead of the keys.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextRequestResponse(
  // read_stream_rid:
  ResourceId,
  // write_stream_rid:
  ResourceId,
  // method:
  // This is a String rather than a ByteString because reqwest will only return
  // the method as a str which is guaranteed to be ASCII-only.
  String,
  // url:
  String,
);

#[op2(async)]
#[serde]
async fn op_http_accept(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<Option<NextRequestResponse>, HttpError> {
  let conn = state.borrow().resource_table.get::<HttpConnResource>(rid)?;

  match conn.accept().await {
    Ok(Some((read_stream, write_stream, method, url))) => {
      let read_stream_rid = state
        .borrow_mut()
        .resource_table
        .add_rc(Rc::new(read_stream));
      let write_stream_rid = state
        .borrow_mut()
        .resource_table
        .add_rc(Rc::new(write_stream));
      let r =
        NextRequestResponse(read_stream_rid, write_stream_rid, method, url);
      Ok(Some(r))
    }
    Ok(None) => Ok(None),
    Err(err) => Err(err),
  }
}

fn req_url(
  req: &hyper_v014::Request<hyper_v014::Body>,
  scheme: &'static str,
  addr: &HttpSocketAddr,
) -> String {
  let host: Cow<str> = match addr {
    HttpSocketAddr::IpSocket(addr) => {
      if let Some(auth) = req.uri().authority() {
        match addr.port() {
          443 if scheme == "https" => Cow::Borrowed(auth.host()),
          80 if scheme == "http" => Cow::Borrowed(auth.host()),
          _ => Cow::Borrowed(auth.as_str()), // Includes port number.
        }
      } else if let Some(host) = req.uri().host() {
        Cow::Borrowed(host)
      } else if let Some(host) = req.headers().get("HOST") {
        match host.to_str() {
          Ok(host) => Cow::Borrowed(host),
          Err(_) => Cow::Owned(
            host
              .as_bytes()
              .iter()
              .cloned()
              .map(char::from)
              .collect::<String>(),
          ),
        }
      } else {
        Cow::Owned(addr.to_string())
      }
    }
    // There is no standard way for unix domain socket URLs
    // nginx and nodejs request use http://unix:[socket_path]:/ but it is not a valid URL
    // httpie uses http+unix://[percent_encoding_of_path]/ which we follow
    #[cfg(unix)]
    HttpSocketAddr::UnixSocket(addr) => Cow::Owned(
      percent_encoding::percent_encode(
        addr
          .as_pathname()
          .and_then(|x| x.to_str())
          .unwrap_or_default()
          .as_bytes(),
        percent_encoding::NON_ALPHANUMERIC,
      )
      .to_string(),
    ),
  };
  let path = req
    .uri()
    .path_and_query()
    .map(|p| p.as_str())
    .unwrap_or("/");
  [scheme, "://", &host, path].concat()
}

fn req_headers(
  header_map: &HeaderMap<HeaderValue>,
) -> Vec<(ByteString, ByteString)> {
  // We treat cookies specially, because we don't want them to get them
  // mangled by the `Headers` object in JS. What we do is take all cookie
  // headers and concat them into a single cookie header, separated by
  // semicolons.
  let cookie_sep = "; ".as_bytes();
  let mut cookies = vec![];

  let mut headers = Vec::with_capacity(header_map.len());
  for (name, value) in header_map.iter() {
    if name == hyper_v014::header::COOKIE {
      cookies.push(value.as_bytes());
    } else {
      let name: &[u8] = name.as_ref();
      let value = value.as_bytes();
      headers.push((name.into(), value.into()));
    }
  }

  if !cookies.is_empty() {
    headers.push(("cookie".into(), cookies.join(cookie_sep).into()));
  }

  headers
}

#[op2(async)]
async fn op_http_write_headers(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: u32,
  #[smi] status: u16,
  #[serde] headers: Vec<(ByteString, ByteString)>,
  #[serde] data: Option<StringOrBuffer>,
) -> Result<(), HttpError> {
  let stream = state
    .borrow_mut()
    .resource_table
    .get::<HttpStreamWriteResource>(rid)?;

  // Track supported encoding
  let encoding = stream.accept_encoding;

  let mut builder = Response::builder();
  // SAFETY: can not fail, since a fresh Builder is non-errored
  let hmap = unsafe { builder.headers_mut().unwrap_unchecked() };

  // Add headers
  hmap.reserve(headers.len() + 2);
  for (k, v) in headers.into_iter() {
    let v: Vec<u8> = v.into();
    hmap.append(
      HeaderName::try_from(k.as_slice())?,
      HeaderValue::try_from(v)?,
    );
  }
  ensure_vary_accept_encoding(hmap);

  let accepts_compression =
    matches!(encoding, Encoding::Brotli | Encoding::Gzip);
  let compressing = accepts_compression
    && (matches!(data, Some(ref data) if data.len() > 20) || data.is_none())
    && should_compress(hmap);

  if compressing {
    weaken_etag(hmap);
    // Drop 'content-length' header. Hyper will update it using compressed body.
    hmap.remove(hyper_v014::header::CONTENT_LENGTH);
    // Content-Encoding header
    hmap.insert(
      hyper_v014::header::CONTENT_ENCODING,
      HeaderValue::from_static(match encoding {
        Encoding::Brotli => "br",
        Encoding::Gzip => "gzip",
        _ => unreachable!(), // Forbidden by accepts_compression
      }),
    );
  }

  let (new_wr, body) = http_response(data, compressing, encoding)?;
  let body = builder.status(status).body(body)?;

  if let Some(otel) = stream.otel_info.as_ref() {
    let mut otel = otel.borrow_mut();
    if let Some(otel_info) = otel.as_mut() {
      otel_info.attributes.http_response_status_code = Some(status as _);
      otel_info.handle_duration_and_request_size();
    }
  }

  let mut old_wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;
  let response_tx = match replace(&mut *old_wr, new_wr) {
    HttpResponseWriter::Headers(response_tx) => response_tx,
    _ => return Err(HttpError::ResponseHeadersAlreadySent),
  };

  match response_tx.send(body) {
    Ok(_) => Ok(()),
    Err(_) => {
      stream.conn.closed().await?;
      Err(HttpError::ConnectionClosedWhileSendingResponse)
    }
  }
}

#[op2]
#[serde]
fn op_http_headers(
  state: &mut OpState,
  #[smi] rid: u32,
) -> Result<Vec<(ByteString, ByteString)>, HttpError> {
  let stream = state.resource_table.get::<HttpStreamReadResource>(rid)?;
  let rd = RcRef::map(&stream, |r| &r.rd)
    .try_borrow()
    .ok_or(HttpError::AlreadyInUse)
    .inspect_err(|e| handle_error_otel(&stream.otel_info, e))?;
  match &*rd {
    HttpRequestReader::Headers(request) => Ok(req_headers(request.headers())),
    HttpRequestReader::Body(headers, _) => Ok(req_headers(headers)),
    _ => unreachable!(),
  }
}

fn http_response(
  data: Option<StringOrBuffer>,
  compressing: bool,
  encoding: Encoding,
) -> Result<(HttpResponseWriter, hyper_v014::Body), HttpError> {
  // Gzip, after level 1, doesn't produce significant size difference.
  // This default matches nginx default gzip compression level (1):
  // https://nginx.org/en/docs/http/ngx_http_gzip_module.html#gzip_comp_level
  const GZIP_DEFAULT_COMPRESSION_LEVEL: u8 = 1;

  match data {
    Some(data) if compressing => match encoding {
      Encoding::Brotli => {
        // quality level 6 is based on google's nginx default value for
        // on-the-fly compression
        // https://github.com/google/ngx_brotli#brotli_comp_level
        // lgwin 22 is equivalent to brotli window size of (2**22)-16 bytes
        // (~4MB)
        let mut writer = brotli::CompressorWriter::new(Vec::new(), 4096, 6, 22);
        writer.write_all(&data)?;
        Ok((HttpResponseWriter::Closed, writer.into_inner().into()))
      }
      Encoding::Gzip => {
        let mut writer = GzEncoder::new(
          Vec::new(),
          Compression::new(GZIP_DEFAULT_COMPRESSION_LEVEL.into()),
        );
        writer.write_all(&data)?;
        Ok((HttpResponseWriter::Closed, writer.finish()?.into()))
      }
      _ => unreachable!(), // forbidden by accepts_compression
    },
    Some(data) => {
      // If a buffer was passed, but isn't compressible, we use it to
      // construct a response body.
      Ok((HttpResponseWriter::Closed, data.to_vec().into()))
    }
    None if compressing => {
      // Create a one way pipe that implements tokio's async io traits. To do
      // this we create a [tokio::io::DuplexStream], but then throw away one
      // of the directions to create a one way pipe.
      let (a, b) = tokio::io::duplex(64 * 1024);
      let (reader, _) = tokio::io::split(a);
      let (_, writer) = tokio::io::split(b);
      let writer: Pin<Box<dyn tokio::io::AsyncWrite>> = match encoding {
        Encoding::Brotli => {
          Box::pin(BrotliEncoder::with_quality(writer, Level::Fastest))
        }
        Encoding::Gzip => Box::pin(GzipEncoder::with_quality(
          writer,
          Level::Precise(GZIP_DEFAULT_COMPRESSION_LEVEL.into()),
        )),
        _ => unreachable!(), // forbidden by accepts_compression
      };
      let (stream, shutdown_handle) =
        ExternallyAbortableReaderStream::new(reader);
      Ok((
        HttpResponseWriter::Body {
          writer,
          shutdown_handle,
        },
        Body::wrap_stream(stream),
      ))
    }
    None => {
      let (body_tx, body_rx) = Body::channel();
      Ok((
        HttpResponseWriter::BodyUncompressed(body_tx.into()),
        body_rx,
      ))
    }
  }
}

// If user provided a ETag header for uncompressed data, we need to
// ensure it is a Weak Etag header ("W/").
fn weaken_etag(hmap: &mut hyper_v014::HeaderMap) {
  if let Some(etag) = hmap.get_mut(hyper_v014::header::ETAG) {
    if !etag.as_bytes().starts_with(b"W/") {
      let mut v = Vec::with_capacity(etag.as_bytes().len() + 2);
      v.extend(b"W/");
      v.extend(etag.as_bytes());
      *etag = v.try_into().unwrap();
    }
  }
}

// Set Vary: Accept-Encoding header for direct body response.
// Note: we set the header irrespective of whether or not we compress the data
// to make sure cache services do not serve uncompressed data to clients that
// support compression.
fn ensure_vary_accept_encoding(hmap: &mut hyper_v014::HeaderMap) {
  if let Some(v) = hmap.get_mut(hyper_v014::header::VARY) {
    if let Ok(s) = v.to_str() {
      if !s.to_lowercase().contains("accept-encoding") {
        *v = format!("Accept-Encoding, {s}").try_into().unwrap()
      }
      return;
    }
  }
  hmap.insert(
    hyper_v014::header::VARY,
    HeaderValue::from_static("Accept-Encoding"),
  );
}

fn should_compress(headers: &hyper_v014::HeaderMap) -> bool {
  // skip compression if the cache-control header value is set to "no-transform" or not utf8
  fn cache_control_no_transform(
    headers: &hyper_v014::HeaderMap,
  ) -> Option<bool> {
    let v = headers.get(hyper_v014::header::CACHE_CONTROL)?;
    let s = match std::str::from_utf8(v.as_bytes()) {
      Ok(s) => s,
      Err(_) => return Some(true),
    };
    let c = CacheControl::from_value(s)?;
    Some(c.no_transform)
  }
  // we skip compression if the `content-range` header value is set, as it
  // indicates the contents of the body were negotiated based directly
  // with the user code and we can't compress the response
  let content_range = headers.contains_key(hyper_v014::header::CONTENT_RANGE);
  // assume body is already compressed if Content-Encoding header present, thus avoid recompressing
  let is_precompressed =
    headers.contains_key(hyper_v014::header::CONTENT_ENCODING);

  !content_range
    && !is_precompressed
    && !cache_control_no_transform(headers).unwrap_or_default()
    && headers
      .get(hyper_v014::header::CONTENT_TYPE)
      .map(compressible::is_content_compressible)
      .unwrap_or_default()
}

#[op2(async)]
async fn op_http_write_resource(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[smi] stream: ResourceId,
) -> Result<(), HttpError> {
  let http_stream = state
    .borrow()
    .resource_table
    .get::<HttpStreamWriteResource>(rid)?;
  let mut wr = RcRef::map(&http_stream, |r| &r.wr).borrow_mut().await;
  let resource = state.borrow().resource_table.get_any(stream)?;
  loop {
    match *wr {
      HttpResponseWriter::Headers(_) => {
        return Err(HttpError::NoResponseHeaders);
      }
      HttpResponseWriter::Closed => {
        return Err(HttpError::ResponseAlreadyCompleted);
      }
      _ => {}
    };

    let view = resource.clone().read(64 * 1024).await?; // 64KB
    if view.is_empty() {
      break;
    }

    match &mut *wr {
      HttpResponseWriter::Body { writer, .. } => {
        let mut result = writer.write_all(&view).await;
        if result.is_ok() {
          result = writer.flush().await;
        }
        if let Err(err) = result {
          assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
          // Don't return "broken pipe", that's an implementation detail.
          // Pull up the failure associated with the transport connection instead.
          http_stream.conn.closed().await?;
          // If there was no connection error, drop body_tx.
          *wr = HttpResponseWriter::Closed;
        }
      }
      HttpResponseWriter::BodyUncompressed(body) => {
        let bytes = view.to_vec().into();
        if let Err(err) = body.sender().send_data(bytes).await {
          assert!(err.is_closed());
          // Pull up the failure associated with the transport connection instead.
          http_stream.conn.closed().await?;
          // If there was no connection error, drop body_tx.
          *wr = HttpResponseWriter::Closed;
        }
      }
      _ => unreachable!(),
    };
  }
  Ok(())
}

#[op2(async)]
async fn op_http_write(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[buffer] buf: JsBuffer,
) -> Result<(), HttpError> {
  let stream = state
    .borrow()
    .resource_table
    .get::<HttpStreamWriteResource>(rid)?;
  let mut wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;

  if let Some(otel) = stream.otel_info.as_ref() {
    let mut maybe_otel_info = otel.borrow_mut();
    if let Some(otel_info) = maybe_otel_info.as_mut() {
      if let Some(response_size) = otel_info.response_size.as_mut() {
        *response_size += buf.len() as u64;
      }
    }
  }

  match &mut *wr {
    HttpResponseWriter::Headers(_) => Err(HttpError::NoResponseHeaders),
    HttpResponseWriter::Closed => Err(HttpError::ResponseAlreadyCompleted),
    HttpResponseWriter::Body { writer, .. } => {
      let mut result = writer.write_all(&buf).await;
      if result.is_ok() {
        result = writer.flush().await;
      }
      match result {
        Ok(_) => Ok(()),
        Err(err) => {
          assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
          // Don't return "broken pipe", that's an implementation detail.
          // Pull up the failure associated with the transport connection instead.
          stream.conn.closed().await?;
          // If there was no connection error, drop body_tx.
          *wr = HttpResponseWriter::Closed;
          Err(HttpError::ResponseAlreadyCompleted)
        }
      }
    }
    HttpResponseWriter::BodyUncompressed(body) => {
      let bytes = Bytes::from(buf.to_vec());
      match body.sender().send_data(bytes).await {
        Ok(_) => Ok(()),
        Err(err) => {
          assert!(err.is_closed());
          // Pull up the failure associated with the transport connection instead.
          stream.conn.closed().await?;
          // If there was no connection error, drop body_tx.
          *wr = HttpResponseWriter::Closed;
          Err(HttpError::ResponseAlreadyCompleted)
        }
      }
    }
  }
}

/// Gracefully closes the write half of the HTTP stream. Note that this does not
/// remove the HTTP stream resource from the resource table; it still has to be
/// closed with `Deno.core.close()`.
#[op2(async)]
async fn op_http_shutdown(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<(), HttpError> {
  let stream = state
    .borrow()
    .resource_table
    .get::<HttpStreamWriteResource>(rid)?;
  let mut wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;
  let wr = take(&mut *wr);
  match wr {
    HttpResponseWriter::Body {
      mut writer,
      shutdown_handle,
    } => {
      shutdown_handle.shutdown();
      match writer.shutdown().await {
        Ok(_) => {}
        Err(err) => {
          assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
          // Don't return "broken pipe", that's an implementation detail.
          // Pull up the failure associated with the transport connection instead.
          stream.conn.closed().await?;
        }
      }
    }
    HttpResponseWriter::BodyUncompressed(body) => {
      body.shutdown();
    }
    _ => {}
  };
  Ok(())
}

#[op2]
#[string]
fn op_http_websocket_accept_header(#[string] key: String) -> String {
  let digest = aws_lc_rs::digest::digest(
    &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
    format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
  );
  BASE64_STANDARD.encode(digest)
}

#[op2(async)]
#[smi]
async fn op_http_upgrade_websocket(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<ResourceId, HttpError> {
  let stream = state
    .borrow_mut()
    .resource_table
    .get::<HttpStreamReadResource>(rid)?;
  let mut rd = RcRef::map(&stream, |r| &r.rd).borrow_mut().await;

  let request = match &mut *rd {
    HttpRequestReader::Headers(request) => request,
    _ => {
      return Err(HttpError::UpgradeBodyUsed)
        .inspect_err(|e| handle_error_otel(&stream.otel_info, e));
    }
  };

  let (transport, bytes) = extract_network_stream(
    hyper_v014::upgrade::on(request)
      .await
      .map_err(|err| HttpError::HyperV014(Arc::new(err)))
      .inspect_err(|e| handle_error_otel(&stream.otel_info, e))?,
  );
  Ok(ws_create_server_stream(
    &mut state.borrow_mut(),
    transport,
    bytes,
  ))
}

// Needed so hyper can use non Send futures
#[derive(Clone)]
pub struct LocalExecutor;

impl<Fut> hyper_v014::rt::Executor<Fut> for LocalExecutor
where
  Fut: Future + 'static,
  Fut::Output: 'static,
{
  fn execute(&self, fut: Fut) {
    deno_core::unsync::spawn(fut);
  }
}

impl<Fut> hyper::rt::Executor<Fut> for LocalExecutor
where
  Fut: Future + 'static,
  Fut::Output: 'static,
{
  fn execute(&self, fut: Fut) {
    deno_core::unsync::spawn(fut);
  }
}

/// Filters out the ever-surprising 'shutdown ENOTCONN' errors.
fn filter_enotconn(
  result: Result<(), hyper_v014::Error>,
) -> Result<(), hyper_v014::Error> {
  if result
    .as_ref()
    .err()
    .and_then(|err| err.source())
    .and_then(|err| err.downcast_ref::<io::Error>())
    .filter(|err| err.kind() == io::ErrorKind::NotConnected)
    .is_some()
  {
    Ok(())
  } else {
    result
  }
}

/// Create a future that is forever pending.
fn never() -> Pending<Never> {
  pending()
}

trait CanDowncastUpgrade: Sized {
  fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
    self,
  ) -> Result<(T, Bytes), Self>;
}

impl CanDowncastUpgrade for hyper::upgrade::Upgraded {
  fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
    self,
  ) -> Result<(T, Bytes), Self> {
    let hyper::upgrade::Parts { io, read_buf, .. } =
      self.downcast::<TokioIo<T>>()?;
    Ok((io.into_inner(), read_buf))
  }
}

impl CanDowncastUpgrade for hyper_v014::upgrade::Upgraded {
  fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
    self,
  ) -> Result<(T, Bytes), Self> {
    let hyper_v014::upgrade::Parts { io, read_buf, .. } = self.downcast()?;
    Ok((io, read_buf))
  }
}

fn maybe_extract_network_stream<
  T: Into<NetworkStream> + AsyncRead + AsyncWrite + Unpin + 'static,
  U: CanDowncastUpgrade,
>(
  upgraded: U,
) -> Result<(NetworkStream, Bytes), U> {
  let upgraded = match upgraded.downcast::<T>() {
    Ok((stream, bytes)) => return Ok((stream.into(), bytes)),
    Err(x) => x,
  };

  match upgraded.downcast::<NetworkBufferedStream<T>>() {
    Ok((stream, upgraded_bytes)) => {
      // Both the upgrade and the stream might have unread bytes
      let (io, stream_bytes) = stream.into_inner();
      let bytes = match (stream_bytes.is_empty(), upgraded_bytes.is_empty()) {
        (false, false) => Bytes::default(),
        (true, false) => upgraded_bytes,
        (false, true) => stream_bytes,
        (true, true) => {
          // The upgraded bytes come first as they have already been read
          let mut v = upgraded_bytes.to_vec();
          v.append(&mut stream_bytes.to_vec());
          Bytes::from(v)
        }
      };
      Ok((io.into(), bytes))
    }
    Err(x) => Err(x),
  }
}

fn extract_network_stream<U: CanDowncastUpgrade>(
  upgraded: U,
) -> (NetworkStream, Bytes) {
  let upgraded =
    match maybe_extract_network_stream::<tokio::net::TcpStream, _>(upgraded) {
      Ok(res) => return res,
      Err(x) => x,
    };
  let upgraded =
    match maybe_extract_network_stream::<deno_net::ops_tls::TlsStream, _>(
      upgraded,
    ) {
      Ok(res) => return res,
      Err(x) => x,
    };
  #[cfg(unix)]
  let upgraded =
    match maybe_extract_network_stream::<tokio::net::UnixStream, _>(upgraded) {
      Ok(res) => return res,
      Err(x) => x,
    };
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  let upgraded =
    match maybe_extract_network_stream::<tokio_vsock::VsockStream, _>(upgraded)
    {
      Ok(res) => return res,
      Err(x) => x,
    };
  let upgraded =
    match maybe_extract_network_stream::<deno_net::tunnel::TunnelStream, _>(
      upgraded,
    ) {
      Ok(res) => return res,
      Err(x) => x,
    };
  let upgraded =
    match maybe_extract_network_stream::<NetworkStream, _>(upgraded) {
      Ok(res) => return res,
      Err(x) => x,
    };

  // TODO(mmastrac): HTTP/2 websockets may yield an un-downgradable type
  drop(upgraded);
  unreachable!("unexpected stream type");
}

#[op2]
#[serde]
pub fn op_http_serve_address_override() -> (u8, String, u32, bool) {
  if let Ok(val) = std::env::var("DENO_SERVE_ADDRESS") {
    return parse_serve_address(&val);
  };

  if deno_net::tunnel::get_tunnel().is_some() {
    return (4, String::new(), 0, true);
  }

  (0, String::new(), 0, false)
}

fn parse_serve_address(input: &str) -> (u8, String, u32, bool) {
  let (input, duplicate) = match input.strip_prefix("duplicate,") {
    Some(input) => (input, true),
    None => (input, false),
  };
  match input.split_once(':') {
    Some(("tcp", addr)) => {
      // TCP address
      match addr.parse::<SocketAddr>() {
        Ok(addr) => {
          let hostname = match addr {
            SocketAddr::V4(v4) => v4.ip().to_string(),
            SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
          };
          (1, hostname, addr.port() as u32, duplicate)
        }
        Err(_) => {
          log::error!("DENO_SERVE_ADDRESS: invalid TCP address: {}", addr);
          (0, String::new(), 0, false)
        }
      }
    }
    Some(("unix", addr)) => {
      // Unix socket path
      if addr.is_empty() {
        log::error!("DENO_SERVE_ADDRESS: empty unix socket path");
        return (0, String::new(), 0, duplicate);
      }
      (2, addr.to_string(), 0, duplicate)
    }
    Some(("vsock", addr)) => {
      // Vsock address
      match addr.split_once(':') {
        Some((cid, port)) => {
          let cid = if cid == "-1" {
            "-1".to_string()
          } else {
            match cid.parse::<u32>() {
              Ok(cid) => cid.to_string(),
              Err(_) => {
                log::error!("DENO_SERVE_ADDRESS: invalid vsock CID: {}", cid);
                return (0, String::new(), 0, false);
              }
            }
          };
          let port = match port.parse::<u32>() {
            Ok(port) => port,
            Err(_) => {
              log::error!("DENO_SERVE_ADDRESS: invalid vsock port: {}", port);
              return (0, String::new(), 0, false);
            }
          };
          (3, cid, port, duplicate)
        }
        None => (0, String::new(), 0, false),
      }
    }
    Some(("tunnel", _)) => (4, String::new(), 0, duplicate),
    Some((_, _)) | None => {
      log::error!("DENO_SERVE_ADDRESS: invalid address format: {}", input);
      (0, String::new(), 0, false)
    }
  }
}

pub static SERVE_NOTIFIER: Notify = Notify::const_new();

#[op2(fast)]
fn op_http_notify_serving() {
  static ONCE: AtomicBool = AtomicBool::new(false);

  if ONCE
    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
    .is_ok()
  {
    SERVE_NOTIFIER.notify_one();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_serve_address() {
    assert_eq!(
      parse_serve_address("tcp:127.0.0.1:8080"),
      (1, "127.0.0.1".to_string(), 8080, false)
    );
    assert_eq!(
      parse_serve_address("tcp:[::1]:9000"),
      (1, "[::1]".to_string(), 9000, false)
    );
    assert_eq!(
      parse_serve_address("duplicate,tcp:[::1]:9000"),
      (1, "[::1]".to_string(), 9000, true)
    );

    assert_eq!(
      parse_serve_address("unix:/var/run/socket.sock"),
      (2, "/var/run/socket.sock".to_string(), 0, false)
    );
    assert_eq!(
      parse_serve_address("duplicate,unix:/var/run/socket.sock"),
      (2, "/var/run/socket.sock".to_string(), 0, true)
    );

    assert_eq!(
      parse_serve_address("vsock:1234:5678"),
      (3, "1234".to_string(), 5678, false)
    );
    assert_eq!(
      parse_serve_address("vsock:-1:5678"),
      (3, "-1".to_string(), 5678, false)
    );
    assert_eq!(
      parse_serve_address("duplicate,vsock:-1:5678"),
      (3, "-1".to_string(), 5678, true)
    );

    assert_eq!(parse_serve_address("tcp:"), (0, String::new(), 0, false));
    assert_eq!(parse_serve_address("unix:"), (0, String::new(), 0, false));
    assert_eq!(parse_serve_address("vsock:"), (0, String::new(), 0, false));
    assert_eq!(parse_serve_address("foo:"), (0, String::new(), 0, false));
    assert_eq!(parse_serve_address("bar"), (0, String::new(), 0, false));
  }
}
