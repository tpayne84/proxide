use bytes::Bytes;
use futures::{prelude::*, try_join};
use h2::{
    client::{self, ResponseFuture},
    server::{self, SendResponse},
    Reason, RecvStream, SendStream,
};
use http::{HeaderMap, Request, Response};
use log::error;
use snafu::ResultExt;
use std::net::SocketAddr;
use std::sync::mpsc::Sender;
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

use super::*;

pub async fn handle<TClient, TServer>(
    mut details: ConnectionDetails,
    client_addr: SocketAddr,
    streams: Streams<TClient, TServer>,
    ui: Sender<SessionEvent>,
) -> Result<()>
where
    TClient: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    TServer: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Streams { client, server } = streams;
    details.protocol_stack.push(Protocol::Http2);

    // This is a debugging proxy so we don't need to be supporting hundreds of concurrent
    // requests. We can opt for a bit larger window size to avoid slowing down the connection.
    let mut client_connection = server::Builder::new()
        .initial_window_size(1_000_000)
        .handshake(client)
        .await
        .context(H2Error {})
        .context(ClientError {
            scenario: "client handshake",
        })?;
    let (server_stream, server_connection) = client::handshake(server)
        .await
        .context(H2Error {})
        .context(ServerError {
            scenario: "server handshake",
        })?;

    // The connection futures are responsible for driving the network communication.
    // Spawn them into a new task to take care of that.
    tokio::spawn({
        let uuid = details.uuid;
        async move {
            match server_connection.await {
                Ok(..) => {}
                Err(e) => error!("Server connection failed for connection {}; {}", uuid, e),
            }
        }
    });

    let mut server_stream =
        server_stream
            .ready()
            .await
            .context(H2Error {})
            .context(ServerError {
                scenario: "starting stream",
            })?;

    ui.send(SessionEvent::NewConnection(NewConnectionEvent {
        uuid: details.uuid,
        protocol_stack: details.protocol_stack,
        client_addr,
        timestamp: SystemTime::now(),
    }))
    .unwrap();

    // We'll wrap all of this into an `async` block to act as a try/catch for handling errors
    // at the end of the function.
    let r = {
        let ui = ui.clone();
        let client_connection = &mut client_connection;
        let server_stream = &mut server_stream;
        let uuid = details.uuid;
        let authority = details.opaque_redirect;
        async move {
            // The client_connection will produce individual HTTP request that we'll accept.
            // These requests will be handled in parallel by spawning them into their own
            // tasks.
            while let Some(request) = client_connection.accept().await {
                let (client_request, client_response) =
                    request.context(H2Error {}).context(ClientError {
                        scenario: "processing request",
                    })?;
                log::debug!("Request: {:?}", client_request);

                let request = ProxyRequest::new(
                    uuid,
                    authority.clone(),
                    client_request,
                    client_response,
                    server_stream,
                    &ui,
                )?;

                let ui = ui.clone();
                tokio::spawn(async move {
                    let ui = ui;
                    match request.execute(ui).await {
                        Ok(_) => {}
                        Err(e) => error!("Request error for request {}; {}", uuid, e),
                    }
                });
            }

            Ok(())
        }
    }
    .await;

    // Once the ´while client_connection.accept()` loop ends, the connection will close (or
    // alternatively an error happened and we'll terminate it). The final status value depends
    // on whether there was an error or not.
    ui.send(SessionEvent::ConnectionDone(ConnectionDoneEvent {
        uuid: details.uuid,
        status: match r {
            Ok(_) => Status::Succeeded,
            Err(_) => Status::Failed,
        },
        timestamp: SystemTime::now(),
    }))
    .unwrap();
    r
}

pub struct ProxyRequest
{
    uuid: Uuid,
    connection_uuid: Uuid,
    client_request: RecvStream,
    client_response: SendResponse<Bytes>,
    server_request: SendStream<Bytes>,
    server_response: ResponseFuture,
}

impl ProxyRequest
{
    pub fn new(
        connection_uuid: Uuid,
        authority: Option<String>,
        client_request: Request<RecvStream>,
        client_response: SendResponse<Bytes>,
        server_stream: &mut client::SendRequest<Bytes>,
        ui: &Sender<SessionEvent>,
    ) -> Result<ProxyRequest>
    {
        let uuid = Uuid::new_v4();
        let (mut client_head, client_request) = client_request.into_parts();

        // Check if we'll need to overwrite the authority.
        if let Some(authority) = authority {
            log::debug!(
                "{}:{} - Replacing authority in URI {} with {}",
                connection_uuid,
                uuid,
                client_head.uri,
                authority
            );
            let mut uri_parts = client_head.uri.into_parts();
            uri_parts.authority = Some(
                http::uri::Authority::from_maybe_shared(authority)
                    .context(UriError {})
                    .context(ConfigurationError {
                        reason: "invalid target server",
                    })?,
            );
            client_head.uri = http::uri::Uri::from_parts(uri_parts)
                .context(UriPartsError {})
                .context(ConfigurationError {
                    reason: "invalid target server",
                })?;
        }

        ui.send(SessionEvent::NewRequest(NewRequestEvent {
            connection_uuid,
            uuid,
            uri: client_head.uri.clone(),
            method: client_head.method.clone(),
            headers: client_head.headers.clone(),
            timestamp: SystemTime::now(),
        }))
        .unwrap();

        let server_request = Request::from_parts(client_head, ());

        // Set up a server request.
        let (server_response, server_request) = server_stream
            .send_request(server_request, client_request.is_end_stream())
            .context(H2Error {})
            .context(ServerError {
                scenario: "sending request",
            })?;

        Ok(ProxyRequest {
            uuid,
            connection_uuid,
            client_request,
            client_response,
            server_request,
            server_response,
        })
    }

    pub async fn execute(self, ui: Sender<SessionEvent>) -> Result<()>
    {
        // Acquire futures that are responsible for streaming the request and the response. These
        // are set up in their own futures to allow parallel request/response streaming to occur.

        // Set up streaming the request to the server.
        //
        // The client request might have ended already if the client didn't need to stream a
        // request body. We'll set up the future here anyway just to keep things consistent and
        // easier to manage without having to special case the is_end_stream somewhere else.
        let uuid = self.uuid;
        let client_request = self.client_request;
        let mut server_request = self.server_request;
        let ui_temp = ui.clone();
        let request_future = async move {
            if client_request.is_end_stream() {
                Ok(None)
            } else {
                let ui = ui_temp;
                let trailers = pipe_stream(
                    client_request,
                    &mut server_request,
                    ui,
                    uuid,
                    RequestPart::Request,
                )
                .await?;

                if let Some(trailers) = trailers.clone() {
                    server_request
                        .send_trailers(trailers)
                        .context(H2Error {})
                        .context(ServerError {
                            scenario: "sending trailers",
                        })?;
                }
                Ok(trailers)
            }
        }
        .then({
            let ui = ui.clone();
            move |r| notify_message_done(ui, uuid, r, RequestPart::Request)
        });

        // Set up streaming the response to the client.
        //
        // This is done in its own async block, since it's the pipe_stream async call that we'll
        // want to happen in parallel, but there's a good chance the server won't send the
        // response before the request stream has proceeded at least some. (Most likely the server
        // will require that stream to proceed in full, unless the call is some sort of a streaming
        // call.
        let mut client_response = self.client_response;
        let server_response = self.server_response;
        let connection_uuid = self.connection_uuid;
        let ui_temp = ui.clone();
        let response_future = async move {
            let ui = ui_temp;
            let response = server_response
                .await
                .context(H2Error {})
                .context(ServerError {
                    scenario: "waiting for response",
                })?;

            let (response_head, response_body) = response.into_parts();
            ui.send(SessionEvent::NewResponse(NewResponseEvent {
                uuid,
                connection_uuid,
                timestamp: SystemTime::now(),
                headers: response_head.headers.clone(),
            }))
            .unwrap();

            let response = Response::from_parts(response_head, ());

            let mut client_stream = client_response
                .send_response(response, response_body.is_end_stream())
                .context(H2Error {})
                .context(ClientError {
                    scenario: "sending response",
                })?;

            // The server might have sent all the details in the headers, at which point there is
            // no body present. Check for this scenario here.
            if response_body.is_end_stream() {
                Ok(None)
            } else {
                log::info!("{}: Server stream starting", uuid);
                let trailers = pipe_stream(
                    response_body,
                    &mut client_stream,
                    ui,
                    uuid,
                    RequestPart::Response,
                )
                .await?;
                log::info!("{}: Server stream ended", uuid);

                if let Some(trailers) = trailers.clone() {
                    client_stream
                        .send_trailers(trailers)
                        .context(H2Error {})
                        .context(ServerError {
                            scenario: "sending trailers",
                        })?;
                }

                Ok(trailers)
            }
        }
        .then({
            let ui = ui.clone();
            move |r| notify_message_done(ui, uuid, r, RequestPart::Response)
        });

        // Now handle both futures in parallel.
        let r = try_join!(request_future, response_future);
        ui.send(SessionEvent::RequestDone(RequestDoneEvent {
            uuid: self.uuid,
            status: match is_fatal_error(&r) {
                true => Status::Failed,
                false => Status::Succeeded,
            },
            timestamp: SystemTime::now(),
        }))
        .unwrap();
        r.map(|_| ())
    }
}

async fn pipe_stream(
    mut source: RecvStream,
    target: &mut SendStream<Bytes>,
    ui: Sender<SessionEvent>,
    uuid: Uuid,
    part: RequestPart,
) -> Result<Option<HeaderMap>>
{
    while let Some(data) = source.data().await {
        let b = match data {
            Ok(b) => b,
            Err(e) => {
                if let Some(reason) = e.reason() {
                    target.send_reset(reason);
                }

                return Err(e).context(H2Error {}).context(ClientError {
                    scenario: "reading content",
                });
            }
        };

        // Send a notification to the UI.
        ui.send(SessionEvent::MessageData(MessageDataEvent {
            uuid,
            data: b.clone(),
            part,
        }))
        .unwrap();

        let size = b.len();
        target
            .send_data(b, source.is_end_stream())
            .context(H2Error {})
            .context(ServerError {
                scenario: "writing content",
            })?;
        source.flow_control().release_capacity(size).unwrap();
    }

    let t = source
        .trailers()
        .await
        .context(H2Error {})
        .context(ClientError {
            scenario: "receiving trailers",
        })?;
    Ok(t)
}

async fn notify_message_done(
    ui: Sender<SessionEvent>,
    uuid: Uuid,
    r: Result<Option<HeaderMap>>,
    part: RequestPart,
) -> Result<()>
{
    match r {
        Ok(trailers) => ui
            .send(SessionEvent::MessageDone(MessageDoneEvent {
                uuid,
                part,
                status: Status::Succeeded,
                timestamp: SystemTime::now(),
                trailers,
            }))
            .unwrap(),
        Err(e) => {
            ui.send(SessionEvent::MessageDone(MessageDoneEvent {
                uuid,
                part,
                status: Status::Succeeded,
                timestamp: SystemTime::now(),
                trailers: None,
            }))
            .unwrap();
            return Err(e);
        }
    }
    Ok(())
}

fn is_fatal_error<S>(r: &Result<S, Error>) -> bool
{
    match r {
        Ok(_) => false,
        Err(e) => match e {
            Error::ServerError { source, .. } | Error::ClientError { source, .. } => match source {
                EndpointError::H2Error { source } => match source.reason() {
                    Some(Reason::NO_ERROR) => false,
                    Some(Reason::CANCEL) => false,
                    _ => true,
                },
                _ => true,
            },
            _ => true,
        },
    }
}
