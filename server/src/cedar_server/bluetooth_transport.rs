// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc,
    },
    time::{Duration, Instant},
};

use bluer::{
    rfcomm::{Profile, Role},
    Session, Uuid,
};
use cedar_elements::thread_name::ThreadName;
use futures::StreamExt;
use hyper::server::conn::Http;
use log::{error, info, warn};
use tonic::transport::server::Routes;
use tonic_web::GrpcWebService;
use tower_http::cors::Cors;

use super::multiplex_service::MultiplexService;
use crate::bonding_helper::{reset_hci_controller, ResetOutcome};
use super::{ClientEntry, ConnectionCounters, ConnectionKey};

// RFCOMM UUID for gRPC over Bluetooth. The same UUID must be used in Cedar Aim.
const BT_CONTROL_UUID: &str = "4e5d4c88-2965-423f-9111-28a506720760";

// Maximum time without a successful write to the RFCOMM stream on an
// active BT connection before the watchdog concludes the transport is
// silently wedged and aborts the connection. Chosen to be well beyond
// any expected pause under normal client polling (~10 Hz getFrame), so
// firing is a strong signal that hyper's write path has stopped making
// progress. Aborting triggers our post-session recovery path.
const BT_WRITE_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

/// Extension marker for requests coming over Bluetooth.
#[derive(Clone, Copy, Debug)]
pub(super) struct BluetoothRequest;

/// Middleware that marks requests as coming from Bluetooth, and attaches
/// the connection's key so `get_frame`/`get_frames` can look up (and
/// update) its map entry.
#[derive(Clone)]
struct BluetoothMarkingMiddleware<S> {
    inner: S,
    key: bluer::rfcomm::SocketAddr,
}

impl<S> tower::Service<hyper::Request<hyper::Body>>
    for BluetoothMarkingMiddleware<S>
where
    S: tower::Service<hyper::Request<hyper::Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: hyper::Request<hyper::Body>) -> Self::Future {
        req.extensions_mut().insert(BluetoothRequest);
        req.extensions_mut()
            .insert(ConnectionKeyExtension(ConnectionKey::Bluetooth(self.key)));
        self.inner.call(req)
    }
}

/// AsyncRead+AsyncWrite adapter that increments a shared counter each time
/// bytes are successfully written to the wrapped stream. Used by the BT
/// watchdog to detect when hyper's write path has stopped making progress
/// (e.g., because the BCM43430A1 has silently wedged).
struct ActivityTrackingStream<S> {
    inner: S,
    activity: Arc<AtomicU64>,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead
    for ActivityTrackingStream<S>
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite
    for ActivityTrackingStream<S>
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &poll {
            if *n > 0 {
                self.activity.fetch_add(*n as u64, AtomicOrdering::Relaxed);
            }
        }
        poll
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// Target SO_SNDBUF for gRPC/HTTP client sockets. Small enough that our
// drop-oldest logic in get_frames sees WiFi backpressure quickly instead
// of frames piling up in a large kernel send buffer, but large enough to
// absorb one biggish JPEG frame without stalling the TCP stream.
const TARGET_SNDBUF: i32 = 64 * 1024;

/// Wrap a TcpListener as a hyper `Accept`, applying SO_SNDBUF to each
/// accepted socket before yielding it. Reducing the send buffer forces
/// backpressure to appear at the tonic write path (instead of hiding in
/// the kernel), so get_frames' drop-oldest slot can discard stale frames
/// under slow WiFi conditions.
pub(super) fn accept_with_sndbuf(
    listener: tokio::net::TcpListener,
) -> impl hyper::server::accept::Accept<
    Conn = tokio::net::TcpStream,
    Error = io::Error,
> {
    hyper::server::accept::poll_fn(move |cx| {
        match listener.poll_accept(cx) {
            std::task::Poll::Ready(Ok((sock, _addr))) => {
                use std::os::unix::io::AsRawFd;
                let fd = sock.as_raw_fd();
                let val = TARGET_SNDBUF;
                // Best-effort: if this fails we still return the socket.
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        &val as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&val) as libc::socklen_t,
                    );
                }
                std::task::Poll::Ready(Some(Ok(sock)))
            }
            std::task::Poll::Ready(Err(e)) => {
                std::task::Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
}

/// Extension carrying the `ConnectionKey` for a request, so `get_frame`/
/// `get_frames` can look up (and update) the connection's entry in
/// `ConnectionCounters`. Inserted by `ConnectionTrackingService` for WiFi
/// requests and by `BluetoothMarkingMiddleware` for Bluetooth requests.
#[derive(Clone, Copy, Debug)]
pub(super) struct ConnectionKeyExtension(pub(super) ConnectionKey);

/// MakeService that logs WiFi connection open/close and tracks active count.
pub(super) struct ConnectionTrackingMakeService<S> {
    inner: S,
    counters: Arc<ConnectionCounters>,
    port: u16,
}

impl<S: Clone> ConnectionTrackingMakeService<S> {
    pub(super) fn new(
        inner: S,
        counters: Arc<ConnectionCounters>,
        port: u16,
    ) -> Self {
        Self {
            inner,
            counters,
            port,
        }
    }
}

impl<'a, S> tower::Service<&'a tokio::net::TcpStream>
    for ConnectionTrackingMakeService<S>
where
    S: Clone,
{
    type Response = ConnectionTrackingService<S>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: &'a tokio::net::TcpStream) -> Self::Future {
        // The peer address is always known at accept time for a TCP
        // connection; fall back to a dummy address in the (essentially
        // impossible) case the socket was already gone.
        let addr = target
            .peer_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        self.counters
            .cedar_wifi_clients
            .lock()
            .unwrap()
            .insert(addr, ClientEntry::default());
        let count = self.counters.cedar_wifi_clients.lock().unwrap().len();
        info!(
            "WiFi connection opened on port {} ({} active)",
            self.port, count
        );
        std::future::ready(Ok(ConnectionTrackingService {
            inner: self.inner.clone(),
            counters: self.counters.clone(),
            port: self.port,
            addr,
        }))
    }
}

/// Wrapper service that removes the connection's map entry on drop.
pub(super) struct ConnectionTrackingService<S> {
    inner: S,
    counters: Arc<ConnectionCounters>,
    port: u16,
    addr: SocketAddr,
}

impl<S> Drop for ConnectionTrackingService<S> {
    fn drop(&mut self) {
        self.counters
            .cedar_wifi_clients
            .lock()
            .unwrap()
            .remove(&self.addr);
        let count = self.counters.cedar_wifi_clients.lock().unwrap().len();
        info!(
            "WiFi connection closed on port {} ({} active)",
            self.port, count
        );
    }
}

impl<S> tower::Service<hyper::Request<hyper::Body>>
    for ConnectionTrackingService<S>
where
    S: tower::Service<hyper::Request<hyper::Body>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: hyper::Request<hyper::Body>) -> Self::Future {
        req.extensions_mut()
            .insert(ConnectionKeyExtension(ConnectionKey::Wifi(self.addr)));
        self.inner.call(req)
    }
}

pub(super) async fn serve_over_bt(
    service: MultiplexService<axum::Router, GrpcWebService<Cors<Routes>>>,
    counters: Arc<ConnectionCounters>,
) -> Result<(), Box<dyn std::error::Error + 'static>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let profile = Profile {
        uuid: Uuid::parse_str(BT_CONTROL_UUID).unwrap(),
        name: Some("Cedar Control".to_string()),
        role: Some(Role::Server),
        // Explicitly setting channel seems to be required, omitting
        // it doesn't work.
        channel: Some(15),
        require_authentication: Some(false),
        require_authorization: Some(false),
        auto_connect: Some(false),
        ..Default::default()
    };

    let mut profile_handle = session.register_profile(profile).await?;
    let ready_at = Instant::now();
    info!("Running cedar control BT channel: {}", adapter.address().await?);

    // Signal from a connection task to the outer loop that tier-1 reset
    // failed and the BT stack was hard-reset. The bluer session and profile
    // registration are invalidated by the hard reset, so the loop must exit
    // and the caller restarts serve_over_bt from scratch.
    let hard_reset_notify = Arc::new(tokio::sync::Notify::new());

    // Track spawned per-connection tasks so we can abort any that outlived
    // the outer loop when we exit. Without this, a hung serve_connection
    // (e.g., blocked on a wedged RFCOMM socket that tokio can't cancel) can
    // accumulate across successive serve_over_bt invocations, holding onto
    // stream fds and other resources.
    let mut connection_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // Track whether we've seen any successful accept since profile
    // registration, so the first one after a fresh registration logs the
    // gap between "profile ready" and "first client connect".
    let mut first_accept_since_ready = true;

    let exit = loop {
        // Prune any tasks that have already finished, so this vec doesn't
        // grow unboundedly during a long-lived serve_over_bt.
        connection_handles.retain(|h| !h.is_finished());

        let hard_reset_wait = hard_reset_notify.notified();
        tokio::pin!(hard_reset_wait);
        let req = tokio::select! {
            r = profile_handle.next() => r,
            _ = &mut hard_reset_wait => {
                info!("BT stack was hard-reset; \
                exiting serve_over_bt to restart bluer session");
                break Ok(());
            }
        };
        if req.is_none() {
            info!("Found no request, returning");
            break Ok(());
        }
        match req.unwrap().accept() {
            Ok(stream) => {
                if first_accept_since_ready {
                    info!(
                        "First BT accept since profile registration: \
                        {:?} elapsed",
                        ready_at.elapsed()
                    );
                    first_accept_since_ready = false;
                }
                // The RFCOMM peer address is always available right after
                // accept(); fall back to a dummy address in the (should be
                // impossible) case the socket was already gone.
                let bt_addr = stream
                    .peer_addr()
                    .unwrap_or_else(|_| bluer::rfcomm::SocketAddr::any());
                counters
                    .cedar_bluetooth_clients
                    .lock()
                    .unwrap()
                    .insert(bt_addr, ClientEntry::default());
                let open_count =
                    counters.cedar_bluetooth_clients.lock().unwrap().len();
                info!("BT connection opened ({} active)", open_count);
                // Byte-write inactivity watchdog. The BCM43430A1 can wedge
                // in a way that hyper's poll_write on the RFCOMM stream
                // silently stops returning progress; serve_connection then
                // never completes, and our normal recovery path (which
                // runs after serve_connection returns) never runs. Wrap
                // the stream so we can observe when write bytes stop
                // flowing, and abort the serve task if they've been
                // stalled for BT_WRITE_INACTIVITY_TIMEOUT.
                let activity = Arc::new(AtomicU64::new(0));
                let tracked_stream = ActivityTrackingStream {
                    inner: stream,
                    activity: activity.clone(),
                };
                let bt_service = BluetoothMarkingMiddleware {
                    inner: service.clone(),
                    key: bt_addr,
                };
                let counters = counters.clone();
                let hard_reset_notify = hard_reset_notify.clone();
                let serve_handle = tokio::task::spawn(async move {
                    Http::new()
                        .serve_connection(tracked_stream, bt_service)
                        .await
                });
                let watchdog_handle = {
                    let abort_serve = serve_handle.abort_handle();
                    tokio::task::spawn(async move {
                        let mut last_seen = 0u64;
                        let mut last_change = tokio::time::Instant::now();
                        loop {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let current =
                                activity.load(AtomicOrdering::Relaxed);
                            if current != last_seen {
                                last_seen = current;
                                last_change = tokio::time::Instant::now();
                            } else if last_change.elapsed()
                                > BT_WRITE_INACTIVITY_TIMEOUT
                            {
                                warn!(
                                    "BT watchdog: no RFCOMM writes for {:?}, \
                                    aborting connection",
                                    BT_WRITE_INACTIVITY_TIMEOUT
                                );
                                abort_serve.abort();
                                return;
                            }
                        }
                    })
                };
                let handle = tokio::task::spawn(async move {
                    let result = serve_handle.await;
                    watchdog_handle.abort();
                    counters
                        .cedar_bluetooth_clients
                        .lock()
                        .unwrap()
                        .remove(&bt_addr);
                    let close_count =
                        counters.cedar_bluetooth_clients.lock().unwrap().len();
                    match result {
                        Ok(Ok(())) => info!(
                            "BT connection closed ({} active)",
                            close_count
                        ),
                        Ok(Err(e))
                            if e.is_incomplete_message() || e.is_closed() =>
                        {
                            info!(
                                "BT connection closed ({} active)",
                                close_count
                            );
                        }
                        Ok(Err(e)) => warn!(
                            "BT connection closed with error ({} active): {:?}",
                            close_count, e
                        ),
                        Err(join_err) if join_err.is_cancelled() => {
                            info!(
                                "BT connection aborted by watchdog ({} active)",
                                close_count
                            );
                        }
                        Err(join_err) => warn!(
                            "BT connection task failed ({} active): {:?}",
                            close_count, join_err
                        ),
                    }
                    // Run the tiered reset on the blocking pool. Its inner
                    // work is entirely synchronous (subprocess calls plus,
                    // on hard-reset escalation, std::thread::sleep pauses),
                    // so running it on an async worker would stall it for
                    // up to a couple seconds — enough to starve the runtime
                    // when several BT connections close near simultaneously.
                    let outcome = tokio::task::spawn_blocking(|| {
                        let _name = ThreadName::new("bt-hci-reset");
                        reset_hci_controller()
                    })
                    .await
                    .unwrap_or(ResetOutcome::HardReset);
                    match outcome {
                        ResetOutcome::LightResetOk => {}
                        ResetOutcome::HardReset => {
                            // Bluer session was invalidated by our reset or
                            // a concurrent one; notify the outer loop to
                            // rebuild.
                            hard_reset_notify.notify_one();
                        }
                    }
                });
                connection_handles.push(handle);
            }
            Err(e) => {
                error!(
                    "Failed to accept BT connection \
                    ({:?} since profile ready): {:?}",
                    ready_at.elapsed(),
                    e
                );
            }
        }
    };

    // Abort any still-running connection tasks. When we exit because of a
    // hard reset, the underlying RFCOMM streams held by these tasks are
    // invalidated anyway; releasing them promptly frees fds and stops any
    // stalled serve_connection futures from hanging around across
    // serve_over_bt restarts.
    for h in &connection_handles {
        if !h.is_finished() {
            h.abort();
        }
    }
    if !connection_handles.is_empty() {
        info!(
            "serve_over_bt exiting; aborted {} connection task(s)",
            connection_handles.len()
        );
    }
    exit
}
