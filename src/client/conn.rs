use crate::body_codec::BodyImpl;
use crate::body_send::BodySender;
use crate::bw::BandwidthMonitor;
use crate::head_ext::HeaderMapExt;
use crate::params::HReqParams;
use crate::uninit::UninitBuf;
use crate::uri_ext::HostPort;
use crate::uri_ext::MethodExt;
use crate::Body;
use crate::Error;
use crate::AGENT_IDENT;
use bytes::Bytes;
use futures_util::ready;
use h2;
use h2::client::SendRequest as H2SendRequest;
use hreq_h1 as h1;
use hreq_h1::client::SendRequest as H1SendRequest;
use once_cell::sync::Lazy;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

static ID_COUNTER: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
const START_BUF_SIZE: usize = 16_384;
const MAX_BUF_SIZE: usize = 2 * 1024 * 1024;

// #[derive(Clone)]
pub struct Connection {
    id: usize,
    host_port: HostPort,
    inner: Inner,
    unfinished_reqs: Arc<()>,
    bw: Option<BandwidthMonitor>,
}

enum Inner {
    H1(H1SendRequest),
    H2(H2SendRequest<Bytes>),
}

impl Connection {
    pub(crate) fn new_h1(host_port: HostPort, conn: H1SendRequest) -> Self {
        Self::new(host_port, Inner::H1(conn), None)
    }

    pub(crate) fn new_h2(
        host_port: HostPort,
        conn: H2SendRequest<Bytes>,
        bw: BandwidthMonitor,
    ) -> Self {
        Self::new(host_port, Inner::H2(conn), Some(bw))
    }

    fn new(host_port: HostPort, inner: Inner, bw: Option<BandwidthMonitor>) -> Self {
        Connection {
            id: ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            host_port,
            inner: inner,
            unfinished_reqs: Arc::new(()),
            bw,
        }
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn host_port(&self) -> &HostPort {
        &self.host_port
    }

    pub(crate) fn is_http2(&self) -> bool {
        match self.inner {
            Inner::H1(_) => false,
            Inner::H2(_) => true,
        }
    }

    pub(crate) fn unfinished_requests(&self) -> usize {
        Arc::strong_count(&self.unfinished_reqs) - 1 // -1 for self
    }

    pub async fn send_request(
        &mut self,
        req: http::Request<Body>,
        body_buffer: &mut BodyBuf,
    ) -> Result<http::Response<Body>, Error> {
        // up the arc-counter on unfinished reqs
        let unfin = self.unfinished_reqs.clone();

        let (mut parts, mut body) = req.into_parts();

        let params = parts.extensions.get::<HReqParams>().unwrap();
        let deadline = params.deadline();

        // resolve deferred body codecs because content-encoding and content-type are settled.
        if body.is_configurable() {
            body.configure(&params, &parts.headers, false);

            // for small request bodies we try to fully buffer the incoming data.
            if params.prebuffer {
                body.attempt_prebuffer().await?;
            }
        }

        configure_request(&mut parts, &body, self.is_http2());

        let req = http::Request::from_parts(parts, body);

        trace!(
            "{} {} {} {} {:?}",
            self.inner,
            self.host_port(),
            req.method(),
            req.uri(),
            req.headers()
        );

        // every request gets their own copy to track received bytes
        let bw = self.bw.clone();

        // send request against a deadline
        let response = deadline
            .race(send_req(req, body_buffer, &self.inner, unfin, bw))
            .await?;

        Ok(response)
    }
}

/// Ensure correct content-length, transfer-encoding, user-agent, accept and content-type headers.
pub(crate) fn configure_request(parts: &mut http::request::Parts, body: &Body, is_http2: bool) {
    if let Some(len) = body.content_encoded_length() {
        // the body indicates a length (for sure).
        // we don't want to set content-length: 0 unless we know it's
        // a method that really has a body. also we never override
        // a user set content-length header.
        let user_set_length = parts.headers.get("content-length").is_some();

        if !user_set_length && (len > 0 || parts.method.indicates_body()) {
            parts.headers.set("content-length", len.to_string());
        }
    } else if !is_http2 && body.is_definitely_a_body() {
        // body does not indicate a length (like from a reader),
        // but there definitely is a body.
        if parts.headers.get("transfer-encoding").is_none() {
            parts.headers.set("transfer-encoding", "chunked");
        }
    }

    if parts.headers.get("user-agent").is_none() {
        parts.headers.set("user-agent", &*AGENT_IDENT);
    }

    if parts.headers.get("accept").is_none() {
        parts.headers.set("accept", "*/*");
    }

    if parts.headers.get("content-type").is_none() {
        if let Some(ctype) = body.content_type() {
            parts.headers.set("content-type", ctype);
        }
    }
}

async fn send_req(
    req: http::Request<Body>,
    body_buffer: &mut BodyBuf,
    proto: &Inner,
    unfin: Arc<()>,
    bw: Option<BandwidthMonitor>,
) -> Result<http::Response<Body>, Error> {
    let params = req.extensions().get::<HReqParams>().unwrap().clone();

    let (parts, mut body_read) = req.into_parts();
    let req = http::Request::from_parts(parts, ());

    let no_body = body_read.is_definitely_no_body() && body_buffer.len() == 0;

    let (mut res_fut, mut body_send) = proto.do_send(req, no_body).await?;
    let mut early_response = None;

    // this buffer should probably be less than h2 window size
    let mut buf = UninitBuf::with_capacity(START_BUF_SIZE, MAX_BUF_SIZE);

    if !no_body {
        let mut use_body_buf = true;

        loop {
            buf.clear();

            match TryOnceFuture(&mut res_fut).await {
                TryOnce::Pending => {
                    // early response did not happen, keep sending body
                }
                TryOnce::Ready(v) => {
                    // TODO: For now we assume an early response means aborting the
                    // body sending. This is not true for expect 100-continue.
                    early_response = Some(v);
                    break;
                }
            }

            let mut amount_read = 0;

            // use buffered body (from a potential earlier 307/308 redirect)
            if use_body_buf {
                let n = buf.read_from_sync(body_buffer)?;
                if n == 0 {
                    // no more buffer to use
                    use_body_buf = false;
                } else {
                    amount_read = n;
                }
            }

            // read new body data
            if !use_body_buf {
                let n = buf.read_from_async(&mut body_read).await?;

                // Append read data to the body_buffer in case of 307/308 redirect.
                // The body_buffer might be inert and no bytes are retained.break
                //
                // TODO: For bodies constructed from String, Vec, File etc, there is
                // no need to retain the bytes in a buffer. We should make something in
                // Body that allows us to reset it back to starting position when possible.
                body_buffer.append(&buf[..n]);

                amount_read = n;
            }

            if amount_read == 0 {
                break;
            }

            // Ship it to they underlying http1.1/http2 layer.
            body_send.send_data(&buf[0..amount_read]).await?;
        }

        // pass the body back with the buffer
        body_buffer.return_body = Some(body_read);

        body_send.send_end().await?;
    }

    let (mut parts, mut res_body) = if let Some(res) = early_response {
        res?
    } else {
        res_fut.await?
    };

    parts.extensions.insert(params.clone());
    res_body.set_unfinished_recs(unfin);
    res_body.set_bw_monitor(bw);
    res_body.configure(&params, &parts.headers, true);

    Ok(http::Response::from_parts(parts, res_body))
}

impl Inner {
    // Generalised sending of request
    async fn do_send(
        &self,
        req: http::Request<()>,
        no_body: bool,
    ) -> Result<(ResponseFuture, BodySender), Error> {
        Ok(match self {
            Inner::H1(h1) => {
                let mut h1 = h1.clone();
                let (fut, send_body) = h1.send_request(req, no_body)?;
                (ResponseFuture::H1(fut), BodySender::H1(send_body))
            }
            Inner::H2(h2) => {
                let mut h2 = h2.clone().ready().await?;
                let (fut, send_body) = h2.send_request(req, no_body)?;
                (ResponseFuture::H2(fut), BodySender::H2(send_body))
            }
        })
    }
}

/// Generalisation over response future
enum ResponseFuture {
    H1(h1::client::ResponseFuture),
    H2(h2::client::ResponseFuture),
}

impl Future for ResponseFuture {
    type Output = Result<(http::response::Parts, Body), Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this {
            ResponseFuture::H1(f) => {
                let (p, b) = ready!(Pin::new(f).poll(cx))?.into_parts();
                let b = Body::new(BodyImpl::Http1(b), None, false);
                Ok((p, b)).into()
            }
            ResponseFuture::H2(f) => {
                let (p, b) = ready!(Pin::new(f).poll(cx))?.into_parts();
                let b = Body::new(BodyImpl::Http2(b), None, false);
                Ok((p, b)).into()
            }
        }
    }
}

/// When polling the wrapped future will never return Poll::Pending, but instead
/// TryOnce::Pending. This is useful in an `async fn` where we don't have access
/// to the Context to do a manual poll.
struct TryOnceFuture<F>(F);

impl<F> Future for TryOnceFuture<F>
where
    Self: Unpin,
    F: Future + Unpin,
{
    type Output = TryOnce<F>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.0).poll(cx) {
            Poll::Pending => TryOnce::Pending,
            Poll::Ready(v) => TryOnce::Ready(v),
        }
        .into()
    }
}

enum TryOnce<F>
where
    F: Future,
{
    Pending,
    Ready(F::Output),
}

/// Body buffer, used to retain a sent body for cases where we want to handle 307/308 redirects.
/// The buffer is always present, but might be inert if the internal vec is `None`.
pub struct BodyBuf {
    vec: Option<Vec<u8>>,
    read_idx: usize,
    // Hack to allow us passing the original body back to the agent.
    //
    // TODO can we find some more elegant way of passing this back?
    return_body: Option<Body>,
}

impl BodyBuf {
    pub fn new(size: usize) -> BodyBuf {
        let vec = if size == 0 {
            None
        } else {
            Some(Vec::with_capacity(size))
        };
        BodyBuf {
            vec,
            read_idx: 0,
            return_body: None,
        }
    }

    /// Reset the body buffer back to 0 optionally retaining the data that has been appended.
    ///
    /// NB: Returning a Option<Body> here is a hack that allows us to pass the original body
    /// back to the Agent in case we need it for the next request.
    pub fn reset(&mut self, keep_data: bool) -> Option<Body> {
        trace!(
            "BodyBuf reset keep_data: {}, len: {}",
            keep_data,
            self.len()
        );
        self.read_idx = 0;
        if keep_data {
            self.return_body.take()
        } else {
            if let Some(vec) = &mut self.vec {
                vec.resize(0, 0);
            }
            self.return_body = None;
            None
        }
    }

    /// Append more data to this buffer. If the amount of data to append is more than the
    /// buffer capacity, the buffer is cleared and no data is retained anymore.
    fn append(&mut self, buf: &[u8]) {
        if let Some(vec) = &mut self.vec {
            let remaining = vec.capacity() - vec.len();
            if buf.len() > remaining {
                self.vec = None;
                debug!("No capacity left in BodyBuf");
                return;
            }
            vec.extend_from_slice(buf);
            trace!("BodyBuf appended: {}/{}", vec.len(), vec.capacity());
        }
    }

    /// Current amount of retained bytes.
    fn len(&self) -> usize {
        self.vec.as_ref().map(|v| v.len()).unwrap_or(0)
    }
}

impl io::Read for BodyBuf {
    /// Read from this buffer without dropping any data.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(if let Some(vec) = &mut self.vec {
            let amt = (&vec[self.read_idx..]).read(buf).unwrap();

            self.read_idx += amt;

            amt
        } else {
            0
        })
    }
}

impl fmt::Display for Inner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Inner::H1(_) => write!(f, "Http1"),
            Inner::H2(_) => write!(f, "Http2"),
        }
    }
}

impl PartialEq for Connection {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Connection {}
