use std::{
    io,
    marker::PhantomData,
    task::{Context, Poll},
};

use futures::{ready, FutureExt};
use js_sys::{Boolean, Reflect};
use once_cell::sync::Lazy;
use thiserror::Error;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStream, ReadableStreamDefaultReader};

pub struct StreamReader<T> {
    fut: Option<JsFuture>,
    stream: ReadableStream,
    reader: ReadableStreamDefaultReader,
    marker: PhantomData<T>,
}

impl<T> Drop for StreamReader<T> {
    fn drop(&mut self) {
        self.reader.release_lock();

        let _ = self.stream.cancel();
    }
}

impl<T: JsCast> StreamReader<T> {
    pub fn new(stream: ReadableStream) -> Self {
        let reader = stream
            .get_reader()
            .dyn_into::<ReadableStreamDefaultReader>()
            .unwrap();
        Self {
            fut: None,
            stream,
            reader,
            marker: PhantomData,
        }
    }
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, ReadError>>> {
        loop {
            if let Some(fut) = &mut self.fut {
                let chunk = ready!(fut.poll_unpin(cx))
                    .map_err(|err| ReadError::ReadError(format!("{err:?}")))?;
                self.fut = None;

                tracing::info!("Got: {chunk:?}");
                let done = Reflect::get(&chunk, &JsValue::from_str("done"))
                    .unwrap()
                    .dyn_into::<Boolean>()
                    .unwrap();

                if done.is_truthy() {
                    return Poll::Ready(None);
                } else {
                    let value = Reflect::get(&chunk, &JsValue::from_str("value"))
                        .unwrap()
                        .dyn_into::<T>()
                        .unwrap();

                    return Poll::Ready(Some(Ok(value)));
                }
            } else {
                let fut = JsFuture::from(self.reader.read());
                self.fut = Some(fut);
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum ReadError {
    #[error("Failed to read from stream: {0:?}")]
    ReadError(String),
}

impl From<ReadError> for io::Error {
    fn from(value: ReadError) -> Self {
        io::Error::new(io::ErrorKind::Other, value)
    }
}
