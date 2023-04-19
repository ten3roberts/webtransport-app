use std::{
    io, mem,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use futures::{future::poll_fn, ready, AsyncRead, AsyncWrite, Future, FutureExt};
use js_sys::Uint8Array;
use thiserror::Error;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WritableStream, WritableStreamDefaultWriter,
};

use crate::reader::{ReadError, StreamReader};

pub struct RecvStream {
    buf: Bytes,
    stream: StreamReader<Uint8Array>,
}

impl RecvStream {
    pub(crate) fn new(stream: ReadableStream) -> Self {
        Self {
            buf: Bytes::new(),
            stream: StreamReader::new(stream),
        }
    }

    /// Read the next chunk of data from the stream.
    pub fn poll_read_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<&mut Bytes, ReadError>>> {
        if self.buf.has_remaining() {
            return Poll::Ready(Some(Ok(&mut self.buf)));
        }

        let data = ready!(self.stream.poll_next(cx)?);
        if let Some(data) = data {
            self.buf = data.to_vec().into();
            Poll::Ready(Some(Ok(&mut self.buf)))
        } else {
            Poll::Ready(None)
        }
    }

    pub fn read_chunk(&mut self) -> ReadChunk {
        ReadChunk { stream: self }
    }
}

impl AsyncRead for RecvStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        match ready!(self.poll_read_chunk(cx)) {
            Some(Ok(bytes)) => {
                let len = buf.len().min(bytes.len());

                buf[..len].copy_from_slice(&bytes[..len]);
                bytes.advance(len);

                Poll::Ready(Ok(len))
            }
            Some(Err(err)) => Poll::Ready(Err(err.into())),
            None => Poll::Ready(Ok(0)),
        }
    }
}

#[derive(Error, Debug)]
pub enum RecvError {
    #[error("Failed to read from stream: {0}")]
    ReadError(String),
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum SendError {
    #[error("Failed to send data to stream: {0}")]
    SendFailed(String),
    #[error("Failed to close the stream: {0}")]
    CloseFailed(String),
}

pub struct SendStream {
    fut: Option<JsFuture>,
    stream: WritableStream,
    writer: Option<WritableStreamDefaultWriter>,
}

impl Drop for SendStream {
    fn drop(&mut self) {
        tracing::info!("Dropping SendStream");
        self.stop();
    }
}

impl From<SendError> for io::Error {
    fn from(value: SendError) -> Self {
        io::Error::new(io::ErrorKind::Other, value)
    }
}

impl SendStream {
    pub(crate) fn new(stream: WritableStream) -> Self {
        let writer = stream.get_writer().unwrap();

        SendStream {
            fut: None,
            stream,
            writer: Some(writer),
        }
    }

    /// Closes the sending stream
    pub fn stop(&mut self) {
        if let Some(writer) = self.writer.take() {
            writer.release_lock();
            let _ = self.stream.close();
        }
    }

    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError>> {
        if let Some(fut) = &mut self.fut {
            ready!(fut.poll_unpin(cx).map_err(|err| {
                tracing::error!("Sending failed: {err:?}");
                SendError::SendFailed(format!("{err:?}"))
            }))?;
            self.fut = None;

            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    pub fn send_chunk(&mut self, buf: &[u8]) {
        if self.fut.is_some() {
            panic!("Send not ready");
        }

        let writer = self.writer.as_mut().expect("Stream is closed");

        let chunk = Uint8Array::from(buf);
        self.fut = Some(JsFuture::from(writer.write_with_chunk(&chunk)));
    }
}

impl AsyncWrite for SendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        ready!(self.poll_ready(cx))?;

        let len = buf.len();
        self.send_chunk(buf);

        Poll::Ready(Ok(len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_ready(cx).map_err(Into::into)
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Self::stop(&mut self);

        Poll::Ready(Ok(()))
    }
}

pub struct ReadChunk<'a> {
    stream: &'a mut RecvStream,
}

impl<'a> Future for ReadChunk<'a> {
    type Output = Option<Result<Bytes, ReadError>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let data = ready!(self.stream.poll_read_chunk(cx)?);
        tracing::info!("Data: {data:?}");

        if let Some(data) = data {
            Poll::Ready(Some(Ok(mem::take(data))))
        } else {
            Poll::Ready(None)
        }
    }
}
