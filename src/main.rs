use core::task::{self, Context};
use std::{
    fmt::{self, Debug, Display, Formatter},
    io,
    ops::Deref,
    pin::Pin,
    process::Output,
    str::FromStr,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use anyhow::anyhow;
use app::{Connection, RecvStream, SendStream};
use bytes::{Buf, Bytes};
use closure::closure;
use flume::{Receiver, Sender};
use futures::{
    ready, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Future, FutureExt, Stream, StreamExt,
};
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
    console::info, ReadableStream, ReadableStreamDefaultReader, WebTransport,
    WebTransportBidirectionalStream, WritableStream, WritableStreamDefaultWriter,
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
    UniStream(Bytes),
    BiStream(Bytes),
    Error(anyhow::Error),
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::Datagram(v) => write!(f, "Datagram {v:?}"),
            Event::UniStream(v) => write!(f, "UniStream {v:?}"),
            Event::BiStream(v) => write!(f, "BiStream {v:?}"),
            Event::Error(err) => write!(f, "Error {err:?}"),
        }
    }
}

enum Action {
    Datagram(Bytes),
    UniStream(Bytes),
    BiStream(Bytes),
}

pub struct ClientInstance {
    url: Url,
    event_rx: Receiver<Event>,
    action_tx: Sender<Action>,
}

macro_rules! log_result {
    ($expr:expr) => {
        if let Err(err) = $expr {
            tracing::error!("{err:?}");
        }
    };
}

async fn handle_incoming_bi(
    mut send: SendStream,
    mut recv: RecvStream,
    tx: Sender<Event>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();

    recv.read_to_end(&mut buf).await?;

    tx.send_async(Event::BiStream(buf.into())).await.ok();

    send.write_all(b"Hello").await?;

    Ok(())
}

async fn handle_incoming_uni(mut recv: RecvStream, tx: Sender<Event>) -> anyhow::Result<()> {
    let mut buf = Vec::new();

    recv.read_to_end(&mut buf).await?;

    tx.send_async(Event::UniStream(buf.into())).await.ok();

    Ok(())
}

async fn handle_open_bi(
    mut send: SendStream,
    mut recv: RecvStream,
    data: Bytes,
    tx: Sender<Event>,
) -> anyhow::Result<()> {
    send_bytes_chunked(&mut send, data).await?;

    drop(send);

    let mut buf = Vec::new();
    recv.read_to_end(&mut buf).await?;
    tx.send_async(Event::BiStream(buf.into())).await.ok();

    Ok(()) as anyhow::Result<()>
}

async fn handle_open_uni(mut send: SendStream, data: Bytes) -> anyhow::Result<()> {
    send_bytes_chunked(&mut send, data).await?;

    drop(send);

    Ok(()) as anyhow::Result<()>
}

async fn send_bytes_chunked(stream: &mut SendStream, data: Bytes) -> anyhow::Result<()> {
    for chunk in data.chunks(4) {
        stream.write_all(chunk).await?;
        yew::platform::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}

impl ClientInstance {
    fn new(url: Url) -> Self {
        let (event_tx, event_rx) = flume::bounded::<Event>(128);
        let (action_tx, action_rx) = flume::bounded::<Action>(128);

        let u = url.clone();
        let run = async move {
            tracing::info!("Opening connection");
            let conn = Connection::connect(u).await?;

            loop {
                let res = select! {
                        Ok(action) = action_rx.recv_async() => {
                            match action {
                                Action::Datagram(data) => conn.send_datagram(&data[..]).await?,
                                Action::BiStream(data) => {
                                    let tx = event_tx.clone();
                                    let (send, recv) = conn.open_bi().await?;

                                    tracing::info!("Opened bi stream");
                                    wasm_bindgen_futures::spawn_local(async move {
                                        log_result!( handle_open_bi(send, recv, data, tx).await)
                                    });
                                }
                                Action::UniStream(data) => {
                                    tracing::info!("Opening uni stream");
                                    let stream = conn.open_uni().await?;
                                    wasm_bindgen_futures::spawn_local(async move {
                                        log_result!( handle_open_uni(stream, data).await)
                                    });
                                }
                            }

                            Ok(()) as anyhow::Result<_>
                        },
                        Some(res) = conn.accept_bi() => {
                            let (send, recv)= res?;

                            tracing::info!("Got bidirectional stream");
                            let tx = event_tx.clone();

                            wasm_bindgen_futures::spawn_local(async move {
                                log_result!(handle_incoming_bi(send, recv, tx).await);
                            });

                            Ok(())
                        },
                        Some(stream) = conn.accept_uni() => {
                            let stream = stream?;

                            let tx = event_tx.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                log_result!(handle_incoming_uni(stream, tx).await);
                            });


                            Ok(())
                        },
                        Some(data) = conn.read_datagram() => {
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

    use_state(|| {
        let client = client.clone();
        let messages = messages.clone();

        wasm_bindgen_futures::spawn_local(async move {
            tracing::info!("Spawning message loop");
            let mut message_num = 1;
            while let Ok(event) = client.event_rx.recv_async().await {
                {
                    let mut messages = messages.lock();
                    if messages.len() >= 32 {
                        messages.remove(0);
                    }

                    let msg = format!("{message_num:>5} {event}");
                    tracing::info!("Got message: {msg:?}");
                    messages.push(msg);
                    message_num += 1;
                }
                messages.set(messages.deref().clone());
            }
        });
    });

    let send_datagram = Callback::from(closure!( clone client,|v: String| {
        let data: Bytes = v.into_bytes().into();

        if let Err(err) = client.action_tx.send(Action::Datagram(data)) {
            tracing::error!("{err:?}");
        }
    }));

    let send_uni = Callback::from(closure!(clone client, |v: String| {
        let data: Bytes = v.into_bytes().into();

        log_result!(client.action_tx.send(Action::UniStream(data)))
    }));

    let send_bi = Callback::from(closure!(clone client, |v: String| {
        let data: Bytes = v.into_bytes().into();

        log_result!(client.action_tx.send(Action::BiStream(data)))
    }));

    html! {
        <div>
            <div>
                <span>{"Connected to "}{client.url.clone()}</span>
                <MessageBox senddatagram={send_datagram} senduni={send_uni} sendbi={send_bi}/>
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
    sendbi: Callback<String>,
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
    let sendbi = props.sendbi.clone();

    let send_datagram =
        Callback::from(closure!(clone text,  |_| senddatagram.emit(text.deref().clone())));
    let send_uni = Callback::from(closure!(clone text, |_| senduni.emit(text.deref().clone())));
    let send_bi = Callback::from(closure!(clone text, |_| sendbi.emit(text.deref().clone())));

    html! {
        <div>
            <form method="post">
                <TextInput name="Message" onchanged={on_text}/>
            </form>
            <button onclick={ send_datagram }>{"Send Datagram"}</button>
            <button onclick={ send_uni }>{"Send Uni"}</button>
            <button onclick={ send_bi }>{"Send Bi"}</button>
        </div>
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
