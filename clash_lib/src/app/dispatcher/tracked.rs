use std::{fmt::Debug, pin::Pin, sync::Arc, task::Poll};

use futures::{Sink, Stream};
use hyper::client::connect::{Connected, Connection};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::oneshot::{error::TryRecvError, Receiver},
};
use tracing::debug;

use crate::{
    app::router::RuleMatcher,
    proxy::{datagram::UdpPacket, AnyOutboundDatagram, ProxyStream},
    session::Session,
};

use super::statistics_manager::{Manager, ProxyChain, TrackerInfo};

pub struct Tracked(uuid::Uuid, Arc<TrackerInfo>);

impl Tracked {
    pub fn id(&self) -> uuid::Uuid {
        self.0
    }

    pub fn tracker_info(&self) -> Arc<TrackerInfo> {
        self.1.clone()
    }
}

#[async_trait::async_trait]
pub trait ChainedStream: ProxyStream {
    fn chain(&self) -> &ProxyChain;
    async fn append_to_chain(&self, name: &str);
}

impl Connection for BoxedChainedStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

pub type BoxedChainedStream = Box<dyn ChainedStream + Send + Sync>;

#[derive(Debug)]
pub struct ChainedStreamWrapper<T> {
    inner: T,
    chain: ProxyChain,
}

impl<T> ChainedStreamWrapper<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            chain: ProxyChain::default(),
        }
    }
}

#[async_trait::async_trait]
impl<T> ChainedStream for ChainedStreamWrapper<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Sync + Send,
{
    fn chain(&self) -> &ProxyChain {
        &self.chain
    }

    async fn append_to_chain(&self, name: &str) {
        self.chain.push(name.to_owned()).await;
    }
}

impl<T> AsyncRead for ChainedStreamWrapper<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for ChainedStreamWrapper<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct TrackedStream {
    inner: BoxedChainedStream,
    manager: Arc<Manager>,
    tracker: Arc<TrackerInfo>,
    close_notify: Receiver<()>,
}

impl TrackedStream {
    pub async fn new(
        inner: BoxedChainedStream,
        manager: Arc<Manager>,
        sess: Session,
        rule: Option<&Box<dyn RuleMatcher>>,
    ) -> Self {
        let uuid = uuid::Uuid::new_v4();
        let chain = inner.chain().clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let s = Self {
            inner,
            manager: manager.clone(),
            tracker: Arc::new(TrackerInfo {
                uuid,
                session_holder: sess,

                start_time: chrono::Utc::now(),
                rule: rule
                    .as_ref()
                    .map(|x| x.type_name().to_owned())
                    .unwrap_or_default(),
                rule_payload: rule.map(|x| x.payload().to_owned()).unwrap_or_default(),
                proxy_chain_holder: chain.clone(),
                ..Default::default()
            }),
            close_notify: rx,
        };

        manager.track(Tracked(uuid, s.tracker_info()), tx).await;

        s
    }

    fn id(&self) -> uuid::Uuid {
        self.tracker.uuid
    }

    fn tracker_info(&self) -> Arc<TrackerInfo> {
        self.tracker.clone()
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        debug!("untrack connection: {}", self.id());
        let _ = self.manager.untrack(self.id());
    }
}

impl AsyncRead for TrackedStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.close_notify.try_recv() {
            Ok(_) => {
                debug!("connection closed by sig: {}", self.id());
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    debug!("connection closed drop: {}", self.id());
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
            },
        }

        let v = Pin::new(self.inner.as_mut()).poll_read(cx, buf);
        let download = buf.filled().len();
        self.manager.push_downloaded(download);
        self.tracker
            .download_total
            .fetch_add(download as u64, std::sync::atomic::Ordering::Release);

        v
    }
}

impl AsyncWrite for TrackedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }

        let v = Pin::new(self.inner.as_mut()).poll_write(cx, buf);
        let upload = match v {
            Poll::Ready(Ok(n)) => n,
            _ => return v,
        };
        self.manager.push_uploaded(upload);
        self.tracker
            .upload_total
            .fetch_add(upload as u64, std::sync::atomic::Ordering::Release);

        v
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }

        Pin::new(&mut self.inner.as_mut()).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }

        Pin::new(self.inner.as_mut()).poll_shutdown(cx)
    }
}

pub struct TrackedDatagram {
    inner: AnyOutboundDatagram,
    manager: Arc<Manager>,
    tracker: Arc<TrackerInfo>,
    close_notify: Receiver<()>,
}

impl TrackedDatagram {
    pub async fn new(
        inner: AnyOutboundDatagram,
        manager: Arc<Manager>,
        sess: Session,
        rule: Option<&Box<dyn RuleMatcher>>,
    ) -> Self {
        let uuid = uuid::Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let s = Self {
            inner,
            manager: manager.clone(),
            tracker: Arc::new(TrackerInfo {
                uuid,
                session_holder: sess,

                start_time: chrono::Utc::now(),
                rule: rule
                    .as_ref()
                    .map(|x| x.type_name().to_owned())
                    .unwrap_or_default(),
                rule_payload: rule.map(|x| x.payload().to_owned()).unwrap_or_default(),
                ..Default::default()
            }),
            close_notify: rx,
        };

        manager.track(Tracked(uuid, s.tracker_info()), tx).await;

        s
    }

    pub fn id(&self) -> uuid::Uuid {
        self.tracker.uuid
    }

    pub fn tracker_info(&self) -> Arc<TrackerInfo> {
        self.tracker.clone()
    }
}

impl Drop for TrackedDatagram {
    fn drop(&mut self) {
        debug!("untrack connection: {}", self.id());
        let _ = self.manager.untrack(self.id());
    }
}

impl Stream for TrackedDatagram {
    type Item = UdpPacket;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(None),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => return Poll::Ready(None),
            },
        }

        let r = Pin::new(self.inner.as_mut()).poll_next(cx);
        if let Poll::Ready(Some(ref pkt)) = r {
            self.manager.push_downloaded(pkt.data.len());
            self.tracker
                .download_total
                .fetch_add(pkt.data.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        r
    }
}

impl Sink<UdpPacket> for TrackedDatagram {
    type Error = std::io::Error;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }
        Pin::new(self.inner.as_mut()).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: UdpPacket) -> Result<(), Self::Error> {
        match self.close_notify.try_recv() {
            Ok(_) => return Err(std::io::ErrorKind::BrokenPipe.into()),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => return Err(std::io::ErrorKind::BrokenPipe.into()),
            },
        }

        let upload = item.data.len();
        self.manager.push_uploaded(upload);
        self.tracker
            .upload_total
            .fetch_add(upload as u64, std::sync::atomic::Ordering::Relaxed);
        Pin::new(self.inner.as_mut()).start_send(item)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }

        Pin::new(self.inner.as_mut()).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.close_notify.try_recv() {
            Ok(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Err(e) => match e {
                TryRecvError::Empty => {}
                TryRecvError::Closed => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
                }
            },
        }

        Pin::new(self.inner.as_mut()).poll_close(cx)
    }
}
