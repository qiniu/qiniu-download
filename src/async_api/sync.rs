use super::{
    super::{
        base::download::RangeReaderBuilder as BaseRangeReaderBuilder,
        config::{with_current_qiniu_config, Config},
        sync_api::WriteSeek,
    },
    download::AsyncRangeReaderBuilder,
    retrier::AsyncRangeReaderWithRangeReader,
    RangePart,
};
use futures::{
    future::poll_fn,
    pin_mut, ready,
    task::{waker, ArcWake},
};
use log::{debug, error, trace};
use positioned_io::ReadAt;
use std::{
    future::Future,
    io::{Error as IoError, Result as IoResult},
    sync::Arc,
    task::{Context, Poll},
    thread::{current as current_thread, park as park_thread},
    thread::{Builder as ThreadBuilder, JoinHandle, Thread},
};
use tokio::{
    runtime::Builder as TokioRuntimeBuilder,
    spawn as spawn_tokio,
    sync::{
        mpsc::{unbounded_channel, UnboundedSender},
        oneshot::{channel, Sender},
    },
};

#[derive(Debug)]
pub(crate) struct RangeReaderBuilder(AsyncRangeReaderBuilder);

impl From<AsyncRangeReaderBuilder> for RangeReaderBuilder {
    fn from(builder: AsyncRangeReaderBuilder) -> Self {
        Self(builder)
    }
}

impl From<RangeReaderBuilder> for AsyncRangeReaderBuilder {
    fn from(builder: RangeReaderBuilder) -> Self {
        builder.0
    }
}

impl From<BaseRangeReaderBuilder> for RangeReaderBuilder {
    fn from(builder: BaseRangeReaderBuilder) -> Self {
        Self(AsyncRangeReaderBuilder::from(builder))
    }
}

impl From<RangeReaderBuilder> for BaseRangeReaderBuilder {
    fn from(builder: RangeReaderBuilder) -> Self {
        builder.0.into()
    }
}

impl RangeReaderBuilder {
    pub(crate) fn build(mut self) -> RangeReader {
        RangeReader {
            key: self.0.take_key(),
            handler: RangeReaderHandle::new(self),
        }
    }

    pub(crate) fn from_config(key: String, config: &Config) -> Self {
        Self(AsyncRangeReaderBuilder::from_config(key, config))
    }
}

trait BuildAsyncRangeReader: Send {
    fn build_async_range_reader(self) -> AsyncRangeReaderWithRangeReader;
}

impl BuildAsyncRangeReader for RangeReaderBuilder {
    fn build_async_range_reader(self) -> AsyncRangeReaderWithRangeReader {
        let base = BaseRangeReaderBuilder::from(self);
        let max_retry_concurrency = base.max_retry_concurrency;
        let io_tries = base.io_tries;
        let builder = AsyncRangeReaderBuilder::from(base);
        AsyncRangeReaderWithRangeReader::new(
            builder.build(),
            max_retry_concurrency.unwrap_or(5),
            io_tries,
        )
    }
}

impl BuildAsyncRangeReader for AsyncRangeReaderWithRangeReader {
    fn build_async_range_reader(self) -> AsyncRangeReaderWithRangeReader {
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RangeReader {
    handler: RangeReaderHandle,
    key: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RangeReaderHandle(Arc<RangeReaderHandleInner>);

type OneshotResponse = Sender<Response>;
type ThreadSender = UnboundedSender<(Request, OneshotResponse)>;

#[derive(Debug)]
struct RangeReaderHandleInner {
    tx: Option<ThreadSender>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum Request {
    UpdateUrls,
    IoUrls,
    ReadAt {
        key: String,
        pos: u64,
        size: u64,
    },
    ReadMultiRanges {
        key: String,
        ranges: Vec<(u64, u64)>,
    },
    Exist {
        key: String,
    },
    FileSize {
        key: String,
    },
    Download {
        key: String,
    },
    ReadLastBytes {
        key: String,
        size: u64,
    },
}

type Response = IoResult<ResponseData>;

#[derive(Debug)]
enum ResponseData {
    Strings(Vec<String>),
    Bytes(Vec<u8>),
    BytesWithSize((Vec<u8>, u64)),
    Parts(Vec<RangePart>),
    Bool(bool),
    U64(u64),
}

impl Drop for RangeReaderHandleInner {
    fn drop(&mut self) {
        let id = self
            .thread
            .as_ref()
            .map(|h| h.thread().id())
            .expect("thread not dropped yet");

        trace!("closing runtime thread ({:?})", id);
        self.tx.take();
        trace!("signaled close for runtime thread ({:?})", id);
        self.thread.take().map(|h| h.join());
        trace!("closed runtime thread ({:?})", id);
    }
}

impl RangeReaderHandle {
    fn new(builder: impl BuildAsyncRangeReader + 'static) -> Self {
        let (tx, rx) = unbounded_channel::<(Request, OneshotResponse)>();
        let (spawn_tx, spawn_rx) = channel::<IoResult<()>>();

        let join_handle = ThreadBuilder::new()
            .name("qiniu-download-internal-sync-runtime".into())
            .spawn(move || {
                let rt = match TokioRuntimeBuilder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        if let Err(e) = spawn_tx.send(Err(e)) {
                            error!("Failed to communicate runtime creation failure: {:?}", e);
                        }
                        return;
                    }
                };
                let fut = async move {
                    let range_reader = builder.build_async_range_reader();
                    if let Err(e) = spawn_tx.send(Ok(())) {
                        error!("Failed to communicate successful startup: {:?}", e);
                        return;
                    }
                    let mut rx = rx;
                    while let Some((req, req_tx)) = rx.recv().await {
                        let req_fut = req.send(range_reader.to_owned());
                        spawn_tokio(forward(req_fut, req_tx));
                    }

                    debug!("({:?}) Receiver is shutdown", current_thread().id());
                };
                trace!("({:?}) start runtime::block_on", current_thread().id());
                rt.block_on(fut);
                trace!("({:?}) end runtime::block_on", current_thread().id());
                drop(rt);
                trace!("({:?}) finished", current_thread().id());
            })
            .expect("Failed to spawn thread");

        match block_on(spawn_rx) {
            Ok(Ok(())) => Self(Arc::new(RangeReaderHandleInner {
                tx: Some(tx),
                thread: Some(join_handle),
            })),
            Ok(Err(err)) => runtime_create_error(err),
            Err(_) => event_loop_panicked(),
        }
    }

    fn execute_request(&self, request: Request) -> Response {
        let (tx, rx) = channel();
        self.0
            .tx
            .as_ref()
            .expect("core thread exited early")
            .send((request, tx))
            .expect("core thread panicked");

        match block_on(async move { rx.await.map_err::<IoError, _>(|_| event_loop_panicked()) }) {
            Ok(result) => result,
            Err(err) => Err(err),
        }
    }
}

impl RangeReader {
    pub(crate) fn from_config(key: String, config: &Config) -> Self {
        RangeReaderBuilder::from_config(key, config).build()
    }

    pub(crate) fn from_env(key: String) -> Option<Self> {
        with_current_qiniu_config(|config| {
            config.and_then(|config| {
                config.with_key(&key.to_owned(), |config| {
                    config.get_or_init_async_range_reader_inner(move || {
                        let max_retry_concurrency = config.max_retry_concurrency().unwrap_or(5);
                        let total_retries = config.retry().unwrap_or(10);
                        RangeReaderHandle::new(AsyncRangeReaderWithRangeReader::new(
                            AsyncRangeReaderBuilder::from_config(String::new(), config).build(),
                            max_retry_concurrency,
                            total_retries,
                        ))
                    })
                })
            })
        })
        .map(|handler| Self { handler, key })
    }

    pub(crate) fn update_urls(&self) -> bool {
        match self.execute(Request::UpdateUrls) {
            Ok(ResponseData::Bool(b)) => b,
            response => unexpected_response(response),
        }
    }

    pub(crate) fn io_urls(&self) -> Vec<String> {
        match self.execute(Request::IoUrls) {
            Ok(ResponseData::Strings(urls)) => urls,
            response => unexpected_response(response),
        }
    }

    pub(crate) fn read_multi_ranges(&self, ranges: &[(u64, u64)]) -> IoResult<Vec<RangePart>> {
        match self.execute(Request::ReadMultiRanges {
            key: self.key.to_owned(),
            ranges: ranges.to_vec(),
        }) {
            Ok(ResponseData::Parts(parts)) => Ok(parts),
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }

    pub(crate) fn exist(&self) -> IoResult<bool> {
        match self.execute(Request::Exist {
            key: self.key.to_owned(),
        }) {
            Ok(ResponseData::Bool(existed)) => Ok(existed),
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }

    pub(crate) fn file_size(&self) -> IoResult<u64> {
        match self.execute(Request::FileSize {
            key: self.key.to_owned(),
        }) {
            Ok(ResponseData::U64(size)) => Ok(size),
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }

    pub(crate) fn download(&self) -> IoResult<Vec<u8>> {
        match self.execute(Request::Download {
            key: self.key.to_owned(),
        }) {
            Ok(ResponseData::Bytes(bytes)) => Ok(bytes),
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }

    pub(crate) fn download_to(&self, writer: &mut dyn WriteSeek) -> IoResult<u64> {
        let bytes = self.download()?;
        writer.write_all(&bytes)?;
        Ok(bytes.len() as u64)
    }

    pub(crate) fn read_last_bytes(&self, buf: &mut [u8]) -> IoResult<(u64, u64)> {
        match self.execute(Request::ReadLastBytes {
            key: self.key.to_owned(),
            size: buf.len() as u64,
        }) {
            Ok(ResponseData::BytesWithSize((bytes, total_size))) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok((bytes.len() as u64, total_size))
            }
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }

    fn execute(&self, request: Request) -> Response {
        self.handler.execute_request(request)
    }
}

impl ReadAt for RangeReader {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> IoResult<usize> {
        match self.execute(Request::ReadAt {
            pos,
            size: buf.len() as u64,
            key: self.key.to_owned(),
        }) {
            Ok(ResponseData::Bytes(bytes)) => {
                buf.copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(err) => Err(err),
            response => unexpected_response(response),
        }
    }
}

impl Request {
    async fn send(self, range_reader: AsyncRangeReaderWithRangeReader) -> Response {
        match self {
            Self::UpdateUrls => Ok(ResponseData::Bool(range_reader.update_urls().await)),
            Self::IoUrls => Ok(ResponseData::Strings(range_reader.io_urls().await)),
            Self::ReadAt { key, pos, size } => range_reader
                .read_at(&key, pos, size)
                .await
                .map(ResponseData::Bytes),
            Self::ReadMultiRanges { key, ranges } => range_reader
                .read_multi_ranges(&key, &ranges)
                .await
                .map(ResponseData::Parts),
            Self::Exist { key } => range_reader.exist(&key).await.map(ResponseData::Bool),
            Self::FileSize { key } => range_reader.file_size(&key).await.map(ResponseData::U64),
            Self::Download { key } => range_reader.download(&key).await.map(ResponseData::Bytes),
            Self::ReadLastBytes { key, size } => range_reader
                .read_last_bytes(&key, size)
                .await
                .map(ResponseData::BytesWithSize),
        }
    }
}

async fn forward(fut: impl Future<Output = Response>, mut tx: OneshotResponse) {
    pin_mut!(fut);

    let result = poll_fn(|cx| match fut.as_mut().poll(cx) {
        Poll::Ready(result) => Poll::Ready(Some(result)),
        Poll::Pending => {
            ready!(tx.poll_closed(cx));
            Poll::Ready(None)
        }
    })
    .await;

    if let Some(result) = result {
        let _ = tx.send(result);
    }
    // else request is canceled
}

#[track_caller]
fn block_on<F: Future>(fut: F) -> F::Output {
    enter();
    let waker = waker(Arc::new(ThreadWaker(current_thread())));
    let mut cx = Context::from_waker(&waker);

    pin_mut!(fut);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        };

        debug!("({:?}) park", current_thread().id());
        park_thread();
    }

    struct ThreadWaker(Thread);

    impl ArcWake for ThreadWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            debug!(
                "({:?}) unpark by ({:?})",
                arc_self.0.id(),
                current_thread().id()
            );
            arc_self.0.unpark();
        }
    }

    fn enter() {
        // Check we aren't already in a runtime
        #[cfg(debug_assertions)]
        {
            let _enter = TokioRuntimeBuilder::new_current_thread()
                .build()
                .expect("build shell runtime")
                .enter();
        }
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn event_loop_panicked() -> ! {
    // The only possible reason there would be a Canceled error
    // is if the thread running the event loop panicked. We could return
    // an Err here, like a BrokenPipe, but the Client is not
    // recoverable. Additionally, the panic in the other thread
    // is not normal, and should likely be propagated.
    panic!("event loop thread panicked");
}

#[cold]
#[inline(never)]
#[track_caller]
fn runtime_create_error(err: IoError) -> ! {
    panic!("tokio runtime creation error: {}", err);
}

#[cold]
#[inline(never)]
#[track_caller]
fn unexpected_response(response: Response) -> ! {
    panic!("unexpected response: {:?}", response);
}

#[cfg(test)]
mod tests {
    use super::{super::super::Credential, *};
    use hyper::{
        header::{HeaderValue, CONTENT_RANGE, CONTENT_TYPE, RANGE},
        StatusCode,
    };
    use multipart::client::lazy::Multipart;
    use std::{
        io::{Cursor, Read},
        thread::spawn as spawn_thread,
        time::Duration,
    };
    use text_io::scan as scan_text;
    use tokio::task::{spawn, spawn_blocking};
    use warp::{header, path, reply::Response, Filter};

    macro_rules! starts_with_server {
        ($addr:ident, $routes:ident, $code:block) => {{
            let (tx, rx) = channel();
            let ($addr, server) =
                warp::serve($routes).bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async move {
                    rx.await.unwrap();
                });
            spawn(server);
            $code;
            tx.send(()).unwrap();
        }};
    }

    fn get_credential() -> Credential {
        Credential::new("1234567890", "abcdefghijk")
    }

    #[tokio::test]
    #[allow(clippy::needless_collect)]
    async fn test_synced_read_at() -> anyhow::Result<()> {
        env_logger::try_init().ok();

        let io_routes = path!("file" / usize)
            .and(header::value(RANGE.as_str()))
            .map(|size: usize, range: HeaderValue| {
                let from: u64;
                let to: u64;
                scan_text!(range.to_str().unwrap().bytes() => "bytes={}-{}", from, to);
                let body = vec![from as u8; size];
                Response::new(body.into())
            });

        starts_with_server!(io_addr, io_routes, {
            spawn_blocking(move || {
                let io_urls = vec![format!("http://{}", io_addr)];

                for (size, base_timeout_ms) in [(1024, 100), (1024 * 1024, 1000)] {
                    let downloader = RangeReaderBuilder::from(
                        BaseRangeReaderBuilder::new(
                            "bucket".to_owned(),
                            format!("file/{}", size),
                            get_credential(),
                            io_urls.to_owned(),
                        )
                        .use_getfile_api(false)
                        .normalize_key(true)
                        .base_timeout(Duration::from_millis(base_timeout_ms)),
                    )
                    .build();

                    let threads = (0..=255u64)
                        .map(|i| {
                            let downloader = downloader.to_owned();
                            spawn_thread(move || {
                                let mut buf = vec![0u8; size];
                                assert_eq!(downloader.read_at(i, &mut buf)?, size);
                                Ok::<_, anyhow::Error>(buf)
                            })
                        })
                        .collect::<Vec<_>>();

                    for (i, response) in threads
                        .into_iter()
                        .map(|thread| thread.join().unwrap())
                        .enumerate()
                    {
                        assert_eq!(response.unwrap(), vec![i as u8; size]);
                    }
                }
            })
            .await?;
        });

        Ok(())
    }

    #[tokio::test]
    async fn test_synced_read_last_bytes() -> anyhow::Result<()> {
        env_logger::try_init().ok();

        let io_routes =
            path!("file")
                .and(header::value(RANGE.as_str()))
                .map(|range: HeaderValue| {
                    assert_eq!(range.to_str().unwrap(), "bytes=-10");
                    let mut resp = Response::new("1234567890".into());
                    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
                    resp.headers_mut().insert(
                        CONTENT_RANGE,
                        "bytes 157286390-157286399/157286400".parse().unwrap(),
                    );
                    resp
                });

        starts_with_server!(io_addr, io_routes, {
            spawn_blocking(move || {
                let io_urls = vec![format!("http://{}", io_addr)];

                let downloader = RangeReaderBuilder::from(
                    BaseRangeReaderBuilder::new(
                        "bucket".to_owned(),
                        "file".to_owned(),
                        get_credential(),
                        io_urls,
                    )
                    .use_getfile_api(false)
                    .normalize_key(true),
                )
                .build();

                let mut buf = [0u8; 10];
                assert_eq!(
                    downloader.read_last_bytes(&mut buf).unwrap(),
                    (10, 157286400)
                );
                assert_eq!(&buf, b"1234567890");
            })
            .await?;
        });

        Ok(())
    }

    #[tokio::test]
    async fn test_synced_download() -> anyhow::Result<()> {
        env_logger::try_init().ok();

        let io_routes = { path!("file").map(|| Response::new("1234567890".into())) };
        starts_with_server!(io_addr, io_routes, {
            spawn_blocking(move || {
                let io_urls = vec![format!("http://{}", io_addr)];
                let downloader = RangeReaderBuilder::from(
                    BaseRangeReaderBuilder::new(
                        "bucket".to_owned(),
                        "file".to_owned(),
                        get_credential(),
                        io_urls,
                    )
                    .use_getfile_api(false)
                    .normalize_key(true),
                )
                .build();
                assert!(downloader.exist().unwrap());
                assert_eq!(downloader.file_size().unwrap(), 10);
                assert_eq!(&downloader.download().unwrap(), b"1234567890");
            })
            .await?;
        });

        Ok(())
    }

    #[tokio::test]
    async fn test_synced_read_multi_ranges() -> anyhow::Result<()> {
        env_logger::try_init().ok();

        let routes = {
            path!("file")
                .and(header::value(RANGE.as_str()))
                .map(move |range: HeaderValue| {
                    assert_eq!(range.to_str().unwrap(), "bytes=0-4,5-9");
                    let mut response_body = Multipart::new();
                    response_body.add_stream(
                        "",
                        Cursor::new(b"12345"),
                        None,
                        None,
                        Some("bytes 0-4/10"),
                    );
                    response_body.add_stream(
                        "",
                        Cursor::new(b"67890"),
                        None,
                        None,
                        Some("bytes 5-9/19"),
                    );
                    let mut fields = response_body.prepare().unwrap();
                    let mut buffer = Vec::new();
                    fields.read_to_end(&mut buffer).unwrap();
                    let mut response = Response::new(buffer.into());
                    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                    response.headers_mut().insert(
                        CONTENT_TYPE,
                        ("multipart/form-data; boundary=".to_owned() + fields.boundary())
                            .parse()
                            .unwrap(),
                    );
                    response
                })
        };

        starts_with_server!(addr, routes, {
            spawn_blocking(move || {
                let io_urls = vec![format!("http://{}", addr)];
                let downloader = RangeReaderBuilder::from(
                    BaseRangeReaderBuilder::new(
                        "bucket".to_owned(),
                        "file".to_owned(),
                        get_credential(),
                        io_urls,
                    )
                    .use_getfile_api(false)
                    .normalize_key(true),
                )
                .build();
                let ranges = [(0, 5), (5, 5)];
                let parts = downloader.read_multi_ranges(&ranges).unwrap();
                assert_eq!(parts.len(), 2);
                assert_eq!(&parts.get(1).unwrap().data, b"12345");
                assert_eq!(parts.get(1).unwrap().range, (0, 5));
                assert_eq!(&parts.first().unwrap().data, b"67890");
                assert_eq!(parts.first().unwrap().range, (5, 5));
            })
            .await?;
        });
        Ok(())
    }
}
