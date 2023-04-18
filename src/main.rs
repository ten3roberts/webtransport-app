use core::task::{self, Context};
use std::{io, ops::Deref, pin::Pin, str::FromStr, sync::Arc, task::Poll, time::Duration};

use anyhow::anyhow;
use bytes::{Buf, Bytes};
use closure::closure;
use flume::{Receiver, Sender};
use futures::{ready, AsyncRead, AsyncWrite, AsyncWriteExt, Future, FutureExt, Stream, StreamExt};
use js_sys::{Boolean, JsString, Reflect, Uint8Array};
use parking_lot::Mutex;
use pin_project::pin_project;
use thiserror::Error;
use tokio::select;
use tracing_subscriber::{
    fmt::time::UtcTime, prelude::__tracing_subscriber_SubscriberExt, registry,
    util::SubscriberInitExt,
};
use tracing_web::MakeConsoleWriter;
use url::Url;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WritableStream, WritableStreamDefaultWriter,
};
use yew::prelude::*;
mod input;
use input::*;

#[function_component]
fn App() -> Html {
    let url = use_state(|| None);

    let set_url = url.setter();
    let on_url = Callback::from(move |v: String| {
        set_url.set(Some(Url::from_str(&v)));
    });

    let client = use_state(|| None);

    let connect = match url.deref().clone() {
        Some(Ok(url)) => {
            let connect = Callback::from(
                closure!(clone client, |_| client.set(Some(Arc::new(ClientInstance::new(url.clone()))))
                ),
            );

            html! { <button onclick={connect}>{"Connect"}</button> }
        }
        Some(Err(err)) => {
            html! { <span>{err}</span> }
        }
        None => {
            html! { <span>{"No url"}</span> }
        }
    };

    html! {
        <div class="content">
            <div class="flex">
                <form method="post">
                    <TextInput name="Url" onchanged={&on_url}/>
                </form>

                {connect}
            </div>

        if let Some(client) = &*client {
            <ClientView client={client}/>
        }

        </div>
    }
}

#[derive(Debug)]
enum Event {
    Datagram(Bytes),
    Error(anyhow::Error),
}

enum Action {
    SendDatagram(Bytes),
    SendUni(Bytes),
}

pub struct ClientInstance {
    url: Url,
    event_rx: Receiver<Event>,
    action_tx: Sender<Action>,
}

impl ClientInstance {
    fn new(url: Url) -> Self {
        let (event_tx, event_rx) = flume::bounded::<Event>(128);
        let (action_tx, action_rx) = flume::bounded::<Action>(128);

        let u = url.clone();
        let run = async move {
            tracing::info!("Opening connection");
            let mut conn = Connection::connect(u).await?;

            loop {
                let res = select! {
                        Ok(action) = action_rx.recv_async() => {
                            match action {
                                Action::SendDatagram(data) => conn.send_datagram(&data[..]).await?,
                                Action::SendUni(data) => {
                                    tracing::info!("Opening uni stream");
                                    let mut stream = conn.open_uni().await?;
                                    tracing::info!("Opened uni stream");

                                    // Write one byte at a time
                                    for byte in data {
                                        stream.write_all(&[byte]).await?;
                                        yew::platform::time::sleep(Duration::from_millis(1000)).await;
                                    }

                                    tracing::info!("Wrote all");
                                }
                            }

                            Ok(()) as anyhow::Result<_>
                        },
                        Some(data) = conn.incoming_datagrams.next() => {
                            let data = data?;
                            event_tx.send_async(Event::Datagram(data)).await.ok();
                            Ok(())
                        },
                        else => { break; }
                };

                if let Err(err) = res {
                    event_tx.send_async(Event::Error(err)).await.ok();
                }
            }

            Ok(()) as anyhow::Result<_>
        };

        wasm_bindgen_futures::spawn_local(async move {
            match run.await {
                Ok(()) => {
                    tracing::info!("Exiting connection loop");
                }
                Err(err) => {
                    tracing::error!("Client error\n\n{err:?}");
                }
            }
        });

        ClientInstance {
            url,
            event_rx,
            action_tx,
        }
    }
}

#[derive(Properties)]
struct ClientProps {
    client: Arc<ClientInstance>,
}

impl PartialEq for ClientProps {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(
            &*self.client as *const ClientInstance,
            &*other.client as *const ClientInstance,
        )
    }
}

#[function_component]
fn ClientView(props: &ClientProps) -> Html {
    tracing::info!("ClientView");

    let client = props.client.clone();

    let messages = use_state(|| Arc::new(Mutex::new(Vec::new())));

    {
        let client = client.clone();
        let messages = messages.clone();

        wasm_bindgen_futures::spawn_local(async move {
            while let Ok(event) = client.event_rx.recv_async().await {
                messages.lock().push(format!("{event:#?}"));
                messages.set(messages.deref().clone());
            }
        });
    }

    let send_datagram = Callback::from(closure!( clone client,|v: String| {
        let data: Bytes = v.into_bytes().into();

        if let Err(err) = client.action_tx.send(Action::SendDatagram(data)) {
            tracing::error!("{err:?}");
        }
    }));

    let send_uni = Callback::from(closure!(clone client, |v: String| {
        let data: Bytes = v.into_bytes().into();

        if let Err(err) = client.action_tx.send(Action::SendUni(data)) {
            tracing::error!("{err:?}");
        }
    }));

    html! {
        <div>
            <div>
                <span>{"Connected to "}{client.url.clone()}</span>
                <MessageBox senddatagram={send_datagram} senduni={send_uni}/>
            </div>

            <div class="message-view">
                <ul>
                    { messages.lock().iter().map(|v| html! {<li class="message">{v}</li>} ).collect::<Html>() }
                </ul>
            </div>
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct MessageBoxProps {
    senddatagram: Callback<String>,
    senduni: Callback<String>,
}

#[function_component]
fn MessageBox(props: &MessageBoxProps) -> Html {
    let text = use_state(String::new);

    let on_text = {
        let text = text.clone();
        Callback::from(move |v| {
            text.set(v);
        })
    };

    let senddatagram = props.senddatagram.clone();
    let senduni = props.senduni.clone();

    let send_datagram =
        Callback::from(closure!(clone text,  |_| senddatagram.emit(text.deref().clone())));
    let send_uni = Callback::from(closure!(clone text, |_| senduni.emit(text.deref().clone())));

    html! {
        <div>
            <form method="post">
                <TextInput name="Message" onchanged={on_text}/>
            </form>
            <button onclick={ send_datagram }>{"Send Datagram"}</button>
            <button onclick={ send_uni }>{"Send Uni"}</button>
        </div>
    }
}

pub struct Connection {
    transport: WebTransport,
    datagrams: WritableStreamDefaultWriter,
    incoming_datagrams: Datagrams,
    incoming_recv_streams: ReadableStreamDefaultReader,
    incoming_bi_streams: ReadableStreamDefaultReader,
}

impl Drop for Connection {
    fn drop(&mut self) {
        tracing::info!("Dropping connection");

        self.transport.close();
    }
}

impl Connection {
    /// Open a connection to `url`
    pub async fn connect(url: Url) -> anyhow::Result<Self> {
        let transport = WebTransport::new(url.as_str()).map_err(|e| anyhow!("{e:?}"))?;

        JsFuture::from(transport.ready())
            .await
            .map_err(|e| anyhow!("{e:?}"))?;

        tracing::info!("Connection ready");

        let datagrams = transport.datagrams();
        let datagrams = datagrams.writable().get_writer().unwrap();
        let incoming_datagrams = transport.datagrams().readable();

        let incoming_datagrams = Datagrams::new(incoming_datagrams);

        let incoming_recv_streams = {
            transport
                .incoming_unidirectional_streams()
                .get_reader()
                .dyn_into()
                .unwrap()
        };

        let incoming_bi_streams = {
            transport
                .incoming_bidirectional_streams()
                .get_reader()
                .dyn_into()
                .unwrap()
        };

        Ok(Connection {
            transport,
            datagrams,
            incoming_datagrams,
            incoming_recv_streams,
            incoming_bi_streams,
        })
    }

    pub async fn open_uni(&self) -> anyhow::Result<SendStream> {
        let stream = JsFuture::from(self.transport.create_unidirectional_stream())
            .await
            .map_err(|e| anyhow!("{e:?}"))?
            .dyn_into::<WritableStream>()
            .unwrap();

        let writer = stream.get_writer().unwrap();

        Ok(SendStream {
            fut: None,
            stream,
            writer: Some(writer),
        })
    }

    /// Accepts an incoming bidirectional stream
    pub async fn accept_bi(&self) -> anyhow::Result<(SendStream, RecvStream)> {
        let stream = JsFuture::from(self.incoming_bi_streams.read())
            .await
            .map_err(|e| anyhow!("{e:?}"))?
            .dyn_into::<WebTransportBidirectionalStream>()
            .unwrap();

        tracing::info!("Got bidirectional stream");

        let recv = stream.readable().dyn_into().unwrap();
        let send = stream.writable().dyn_into().unwrap();

        // Use the new methods
        Ok((SendStream::new(send), RecvStream::new(recv)))
    }

    /// Accepts an incoming unidirectional stream
    pub async fn accept_uni(&self) -> anyhow::Result<RecvStream> {
        let stream = JsFuture::from(self.incoming_recv_streams.read())
            .await
            .map_err(|e| anyhow!("{e:?}"))?
            .dyn_into::<ReadableStream>()
            .unwrap();

        tracing::info!("Got unidirectional stream");

        let reader = stream.get_reader().dyn_into().unwrap();

        Ok(RecvStream {
            fut: None,
            reader: Some(reader),
            stream,
            buf: Bytes::new(),
        })
    }

    /// Sends data to a WebTransport connection.
    pub async fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        let data = Uint8Array::from(data);
        let _stream = JsFuture::from(self.datagrams.write_with_chunk(&data))
            .await
            .map_err(|e| anyhow!("{e:?}"));

        Ok(())
    }
}

pub struct RecvStream {
    fut: Option<JsFuture>,
    buf: Bytes,
    reader: Option<ReadableStreamDefaultReader>,
    stream: ReadableStream,
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        // self.close();
    }
}

impl RecvStream {
    fn new(stream: ReadableStream) -> Self {
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
    fn new(stream: WritableStream) -> Self {
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

impl Stream for Datagrams {
    type Item = anyhow::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(fut) = &mut self.fut {
                tracing::info!("Receiving datagram");
                let result = futures::ready!(fut.poll_unpin(cx));
                tracing::info!("Future finished");

                self.fut = None;
                let result = result.map_err(|err| anyhow!("{err:?}"))?;

                let done = Reflect::get(&result, &JsString::from("done"))
                    .unwrap()
                    .unchecked_into::<Boolean>();

                if done.is_truthy() {
                    tracing::info!("Stream is done");
                    return Poll::Ready(None);
                } else {
                    let bytes: Uint8Array = Reflect::get(&result, &JsString::from("value"))
                        .unwrap()
                        .unchecked_into();

                    let bytes: Bytes = bytes.to_vec().into();
                    tracing::info!("Got bytes");

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

fn main() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false) // Only partially supported across browsers
        .with_timer(UtcTime::rfc_3339()) // std::time is not available in browsers
        .with_writer(MakeConsoleWriter); // write events to the console

    registry().with(fmt_layer).init();

    yew::Renderer::<App>::new().render();
}
