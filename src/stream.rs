use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use futures::{ready, AsyncRead, AsyncWrite, FutureExt};
use js_sys::Uint8Array;
use thiserror::Error;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WritableStream, WritableStreamDefaultWriter,
};

pub struct RecvStream {
    fut: Option<JsFuture>,
    buf: Bytes,
    reader: Option<ReadableStreamDefaultReader>,
    stream: ReadableStream,
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        self.close();
    }
}

impl RecvStream {
    pub(crate) fn new(stream: ReadableStream) -> Self {
        let reader = stream.get_reader().dyn_into().unwrap();

        Self {
            fut: None,
            reader: Some(reader),
            stream,
            buf: Bytes::new(),
        }
    }

    /// Closes the stream
    pub fn close(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.release_lock();
            let _ = self.stream.cancel();
        }
    }

    /// Read the next chunk of data from the stream.
    pub fn poll_read_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Result<&mut Bytes, RecvError>> {
        if !self.buf.is_empty() {
            return Poll::Ready(Ok(&mut self.buf));
        }

        loop {
            if let Some(fut) = &mut self.fut {
                let data = ready!(fut.poll_unpin(cx))
                    .map_err(|err| RecvError::ReadError(format!("{err:?}")))?
                    .dyn_into::<Uint8Array>()
                    .unwrap();

                self.buf = data.to_vec().into();
                return Poll::Ready(Ok(&mut self.buf));
            } else {
                let reader = self.reader.as_ref().expect("Stream is closed");
                let fut = JsFuture::from(reader.read());
                self.fut = Some(fut);
            }
        }
    }
}

impl AsyncRead for RecvStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let bytes = ready!(self.poll_read_chunk(cx))?;

        let len = buf.len().min(bytes.len());

        buf[..len].copy_from_slice(&bytes[..len]);
        bytes.advance(len);

        Poll::Ready(Ok(len))
    }
}

impl From<RecvError> for io::Error {
    fn from(value: RecvError) -> Self {
        io::Error::new(io::ErrorKind::Other, value)
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
        tracing::info!("Poll send");
        if let Some(fut) = &mut self.fut {
            ready!(fut.poll_unpin(cx).map_err(|err| {
                tracing::error!("Sending failed: {err:?}");
                SendError::SendFailed(format!("{err:?}"))
            }))?;
            self.fut = None;

            tracing::info!("ready to send");

            Poll::Ready(Ok(()))
        } else {
            tracing::info!("nothing to send");
            Poll::Ready(Ok(()))
        }
    }

    pub fn send_chunk(&mut self, buf: &[u8]) {
        tracing::info!("send_chunk {buf:?}");
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
        tracing::info!("poll_write");
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
