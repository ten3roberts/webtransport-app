use std::{
    borrow::BorrowMut, cell::RefCell, net::SocketAddr, ops::Deref, pin::Pin, rc::Rc, str::FromStr,
    sync::Arc, task::Poll,
};

use anyhow::anyhow;
use bytes::Bytes;
use flume::{Receiver, Sender};
use futures::{Future, Stream, StreamExt};
use itertools::Itertools;
use js_sys::{Boolean, JsString, Reflect, Uint8Array};
use parking_lot::Mutex;
use tokio::{join, select};
use tracing_subscriber::{
    fmt::time::UtcTime, prelude::__tracing_subscriber_SubscriberExt, registry,
    util::SubscriberInitExt,
};
use tracing_web::MakeConsoleWriter;
use url::Url;
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    EventTarget, HtmlInputElement, ReadableStream, ReadableStreamDefaultReader, WebTransport,
    WebTransportCloseInfo, WritableStream, WritableStreamDefaultWriter,
};
use yew::{prelude::*, suspense::use_future};
mod input;
use input::*;

#[function_component]
fn App() -> Html {
    let url = use_state(|| None);

    let set_url = url.setter();
    let on_url = Callback::from(move |v: String| {
        set_url.set(Some(Url::from_str(&v)));
    });

    let on_submit = Callback::from(|()| tracing::info!("Submitted form"));

    let client = match url.deref() {
        Some(Ok(url)) => {
            html! { <ClientView url={url.clone()}/> }
        }
        Some(Err(err)) => {
            html! { <span>{err}</span> }
        }
        None => {
            html! { <span>{"No url specified"}</span> }
        }
    };

    html! {
        <div class="content">
        <Form onsubmit={on_submit}><TextInput name="Url" onchanged={&on_url}/></Form>

            { client }
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
}

pub struct ClientInstance {
    event_rx: Receiver<Event>,
    action_tx: Sender<Action>,
}

impl ClientInstance {
    fn new(url: Url) -> Self {
        let (event_tx, event_rx) = flume::bounded::<Event>(128);
        let (action_tx, action_rx) = flume::bounded::<Action>(128);

        let run = async move {
            tracing::info!("Opening connection");
            let mut conn = Connection::connect(url).await?;

            loop {
                let res = select! {
                        Ok(action) = action_rx.recv_async() => {
                            match action {
                                Action::SendDatagram(data) => conn.send_datagram(&data[..]).await?,
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
            event_rx,
            action_tx,
        }
    }
}

#[derive(Properties, PartialEq)]
struct ClientProps {
    url: Url,
}

#[function_component]
fn ClientView(props: &ClientProps) -> Html {
    tracing::info!("ClientView");
    let url = props.url.clone();

    let messages = use_state(|| Arc::new(Mutex::new(Vec::new())));

    let on_datagram = {
        let messages = messages.clone();
        Box::new(move |bytes: Bytes| {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            tracing::info!("Received datagram from server: {s:?}");
            messages.lock().push(s);
            messages.set(messages.deref().clone());
        })
    };

    let client = use_state(|| Arc::new(ClientInstance::new(url)));

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

    let send_datagram = Callback::from(move |v: String| {
        let data: Bytes = v.into_bytes().into();

        if let Err(err) = client.action_tx.send(Action::SendDatagram(data)) {
            tracing::error!("{err:?}");
        }
    });

    html! {

        <div>
            <div>
                <span>{"Connected to "}{props.url.clone()}</span>
                <MessageBox send_datagram={send_datagram}/>
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
    send_datagram: Callback<String>,
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

    let send_datagram = props.send_datagram.clone();
    let on_submit = Callback::from(move |()| send_datagram.emit(text.deref().clone()));

    html! {
        <div>
        <Form onsubmit={on_submit}><TextInput name="Message" onchanged={on_text}/></Form>
        </div>
    }
}

pub struct Connection {
    transport: WebTransport,
    datagrams: WritableStreamDefaultWriter,
    incoming_datagrams: Datagrams,
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
        let incoming_datagrams = transport
            .datagrams()
            .readable()
            .get_reader()
            .dyn_into()
            .unwrap();

        let incoming_datagrams = Datagrams {
            fut: None,
            stream: incoming_datagrams,
        };

        Ok(Connection {
            transport,
            datagrams,
            incoming_datagrams,
        })
    }

    /// Sends data to a WebTransport connection.
    pub async fn send_datagram(&self, data: &[u8]) -> anyhow::Result<()> {
        let data = Uint8Array::from(data);
        let _stream = JsFuture::from(self.datagrams.write_with_chunk(&data))
            .await
            .map_err(|e| anyhow!("{e:?}"));

        // self.datagrams.release_lock();

        Ok(())
    }
}

/// Cancellation safe datagram reader
#[pin_project::pin_project]
pub struct Datagrams {
    // Pending read
    #[pin]
    fut: Option<JsFuture>,
    stream: ReadableStreamDefaultReader,
}

impl Stream for Datagrams {
    type Item = anyhow::Result<Bytes>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut p = self.project();
        loop {
            if let Some(fut) = p.fut.as_mut().as_pin_mut() {
                tracing::info!("Receiving datagram");
                let result = futures::ready!(fut.poll(cx));
                tracing::info!("Future finished");

                *p.fut = None;
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
                *p.fut = Some(JsFuture::from(p.stream.read()));
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
