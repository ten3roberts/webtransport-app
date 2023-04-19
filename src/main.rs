use core::task::{self, Context};
use std::{io, ops::Deref, pin::Pin, str::FromStr, sync::Arc, task::Poll, time::Duration};

use anyhow::anyhow;
use app::Connection;
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
    UniStream(Bytes),
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
                                        yew::platform::time::sleep(Duration::from_millis(100)).await;
                                    }

                                    tracing::info!("Wrote all");
                                }
                            }

                            Ok(()) as anyhow::Result<_>
                        },
                        Some(stream) = conn.accept_uni() => {
                            let mut stream = stream?;

                            let tx = event_tx.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                while let Some(data) = stream.read_chunk().await {
                                    match data {
                                        Ok(data) => {
                                            tracing::info!("Got data: {:?}", data);
                                            tx.send_async(Event::UniStream(data)).await.ok();
                                        }
                                        Err(err) => {
                                            tracing::error!("Error reading stream: {:?}", err);
                                            break;
                                        }
                                    }
                                }
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

fn main() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false) // Only partially supported across browsers
        .with_timer(UtcTime::rfc_3339()) // std::time is not available in browsers
        .with_writer(MakeConsoleWriter); // write events to the console

    registry().with(fmt_layer).init();

    yew::Renderer::<App>::new().render();
}
