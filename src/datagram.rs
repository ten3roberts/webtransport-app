use std::{pin::Pin, task::Poll};

use bytes::Bytes;
use futures::{FutureExt, Stream};
use js_sys::{Boolean, JsString, Reflect, Uint8Array};
use thiserror::Error;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStream, ReadableStreamDefaultReader};

/// Cancellation safe datagram reader
pub struct Datagrams {
    // Pending read
    fut: Option<JsFuture>,
    stream: ReadableStream,
    reader: Option<ReadableStreamDefaultReader>,
}

impl Drop for Datagrams {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Datagrams {
    pub fn new(stream: ReadableStream) -> Self {
        let reader = stream.get_reader().dyn_into().unwrap();

        Datagrams {
            fut: None,
            stream,
            reader: Some(reader),
        }
    }

    pub fn stop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.release_lock();
            let _ = self.stream.cancel();
        }
    }
}

#[derive(Error, Debug)]
pub enum ReadDatagramError {
    #[error("Failed to read from datagram stream: {0:?}")]
    ReadError(String),
}

impl Stream for Datagrams {
    type Item = Result<Bytes, ReadDatagramError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(fut) = &mut self.fut {
                let result = futures::ready!(fut.poll_unpin(cx));

                self.fut = None;

                let result =
                    result.map_err(|err| ReadDatagramError::ReadError(format!("{err:?}")))?;

                let done = Reflect::get(&result, &JsString::from("done"))
                    .unwrap()
                    .unchecked_into::<Boolean>();

                if done.is_truthy() {
                    return Poll::Ready(None);
                } else {
                    let bytes: Uint8Array = Reflect::get(&result, &JsString::from("value"))
                        .unwrap()
                        .unchecked_into();

                    let bytes: Bytes = bytes.to_vec().into();

                    return Poll::Ready(Some(Ok(bytes)));
                };
            } else {
                tracing::info!("Reading next datagram");
                // Start a new read
                self.fut = Some(JsFuture::from(
                    self.reader.as_mut().expect("Reader is closed").read(),
                ));
            }
        }
    }
}