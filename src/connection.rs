use std::{
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::anyhow;
use bytes::Bytes;
use futures::{ready, Future, StreamExt};
use js_sys::Uint8Array;
use parking_lot::Mutex;
use url::Url;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportReceiveStream, WritableStream, WritableStreamDefaultWriter,
};

use crate::{
    reader::{ReadError, StreamReader},
    RecvStream, SendStream,
};

pub struct Connection {
    transport: WebTransport,
    datagrams: WritableStreamDefaultWriter,
    incoming_datagrams: Mutex<StreamReader<Uint8Array>>,
    incoming_recv_streams: Mutex<StreamReader<ReadableStream>>,
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

        let incoming_datagrams = StreamReader::new(incoming_datagrams);

        let incoming_recv_streams = StreamReader::new(transport.incoming_unidirectional_streams());

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
            incoming_datagrams: Mutex::new(incoming_datagrams),
            incoming_recv_streams: Mutex::new(incoming_recv_streams),
            incoming_bi_streams,
        })
    }

    pub async fn open_uni(&self) -> anyhow::Result<SendStream> {
        let stream = JsFuture::from(self.transport.create_unidirectional_stream())
            .await
            .map_err(|e| anyhow!("{e:?}"))?
            .dyn_into::<WritableStream>()
            .unwrap();

        Ok(SendStream::new(stream))
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
    pub fn accept_uni(&self) -> AcceptUni {
        AcceptUni {
            stream: &self.incoming_recv_streams,
        }
    }

    /// Reads the next datagram from the connection
    pub fn read_datagram(&self) -> ReadDatagram<'_> {
        ReadDatagram {
            stream: &self.incoming_datagrams,
        }
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

/// Reads the next datagram from the connection
pub struct ReadDatagram<'a> {
    stream: &'a Mutex<StreamReader<Uint8Array>>,
}

impl Future for ReadDatagram<'_> {
    type Output = Option<Result<Bytes, ReadError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut datagrams = self.stream.lock();

        let data = ready!(datagrams.poll_next(cx));

        match data {
            Some(Ok(data)) => Poll::Ready(Some(Ok(data.to_vec().into()))),
            Some(Err(err)) => Poll::Ready(Some(Err(err))),
            None => Poll::Ready(None),
        }
    }
}

/// Reads the next datagram from the connection
pub struct AcceptUni<'a> {
    stream: &'a Mutex<StreamReader<ReadableStream>>,
}

impl Future for AcceptUni<'_> {
    type Output = Option<Result<RecvStream, ReadError>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut datagrams = self.stream.lock();

        let data = ready!(datagrams.poll_next(cx));

        match data {
            Some(Ok(data)) => Poll::Ready(Some(Ok(RecvStream::new(data)))),
            Some(Err(err)) => Poll::Ready(Some(Err(err))),
            None => Poll::Ready(None),
        }
    }
}
