#![allow(unused_variables)]
#![feature(generators)]
extern crate actix;
extern crate actix_web;
extern crate futures_await as futures;
#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate serde_json;
extern crate url;

use actix::prelude::*;
use actix_web::client::{ClientRequest, ClientRequestBuilder, SendRequest};
use actix_web::error::{Error as ActixError, ErrorInternalServerError};
use actix_web::http::header;
use actix_web::{middleware, server, ws, App, HttpRequest, HttpResponse};
use futures::prelude::{async, await};
use futures::Future;
use futures::Stream;
use std::sync::Arc;

mod utils;

// TODO: add better logging
// TODO: should I add some heartbeat code?
// TODO: I could theoretically reconnect to stem if the websockets connection to stem breaks, but it would require
// that I keep around an aggregate of the websocket messages received from the client so that I can continue to receive
// websocket messages from the client, but I would have to send all aggregate messages first before sending new messages...
// I hope this doesn't break the current logic too much

// this creates and starts the Forwarder Actor
// it uses a modified "start" function which increases the default size of websockets messages
fn ws_index(
    req: HttpRequest,
    sender_addr: Arc<Addr<Sender>>,
) -> Box<Future<Item = HttpResponse, Error = ActixError>> {
    println!("Request received: {:?}", req);
    Box::new(
        Forwarder::with_request(req.clone(), sender_addr).and_then(move |actor| {
            utils::websockets::start(&req, actor, |stream| stream.max_size(10 * (1 << 20)))
        }),
    )
}

// the Forwarder Actor
struct Forwarder {
    initial_request: HttpRequest, // this will be useful when connecting to stem because it contains authorization headers
    callback_url: Option<url::Url>,
    reader: Option<ws::ClientReader>,
    writer: Option<ws::ClientWriter>,
    response_body: Vec<u8>,
    sender: Arc<Addr<Sender>>,
}

// "with_request" will be used to start the Forwarder Actor
// although the callback_url, reader, and writer will be populated later - after the first websockets text message is received
impl Forwarder {
    #[async] // TODO: there must be a way to not make this async - I am likely confused about the function signature
    pub fn with_request(
        req: HttpRequest,
        sender_addr: Arc<Addr<Sender>>,
    ) -> Result<Self, ActixError> {
        let result = Self {
            initial_request: req.clone(),
            callback_url: None,
            reader: None,
            writer: None,
            response_body: Vec::new(),
            sender: sender_addr.clone(),
        };

        Ok(result)
    }
}

impl Actor for Forwarder {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        println!("Forwarder Actor started.");
    }
}

#[derive(Deserialize, Debug)]
struct InitialMessage {
    callback: String,
}

impl StreamHandler<ws::Message, ws::ProtocolError> for Forwarder {
    fn handle(&mut self, msg: ws::Message, ctx: &mut Self::Context) {
        // println!("ws::Message received: {:?}", msg);
        match msg {
            ws::Message::Text(text) => {
                println!("Text received - will attempt to interpret as initial json object specifying callback and content-type.");

                let initial_message: Result<InitialMessage, serde_json::Error> =
                    serde_json::from_str(&text);
                match initial_message {
                    Ok(result) => {
                        println!("The text received was deserialized into the initial_message json object: {:#?}", result);
                        let callback_url_option = url::Url::parse(&result.callback);
                        match callback_url_option {
                            Ok(callback_url) => {
                                self.callback_url = Some(callback_url);

                                println!("Will attempt to connect to stem now.");
                                let stem_connection = connect_to_stem().wait();
                                match stem_connection {
                                    Ok((reader, writer)) => {
                                        self.reader = Some(reader);
                                        self.writer = Some(writer);
                                        ctx.add_stream(self.reader.take().unwrap().map(FromStem));
                                    }
                                    Err(_) => {
                                        ctx.close(Some(ws::CloseReason {
                                            code: ws::CloseCode::Protocol,
                                            description: Some(
                                                "Failed to connect to stem.".to_string(),
                                            ),
                                        }));
                                    }
                                }
                            }
                            Err(_) => {
                                ctx.close(Some(ws::CloseReason {
                                    code: ws::CloseCode::Protocol,
                                    description: Some("Failed to parse callback url.".to_string()),
                                }));
                            }
                        }
                    }
                    Err(_) => {
                        ctx.close(Some(ws::CloseReason {
                            code: ws::CloseCode::Protocol,
                            description: Some(
                                "Failed to parse websockets text as json.".to_string(),
                            ),
                        }));
                    }
                }
            }
            ws::Message::Binary(bin) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.binary(bin);
                }
            }
            ws::Message::Ping(msg) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.ping(&msg);
                }
            }
            ws::Message::Pong(msg) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.pong(&msg);
                }
            }
            ws::Message::Close(reason) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.close(reason);
                }
            }
        }
    }
}

// this let's Forwarder handle messages coming FROM stem (i.e. the websocket stream to accumulate as the final response to be sent to the callback)
struct FromStem(ws::Message);

impl FromStem {
    pub fn into_inner(self) -> ws::Message {
        self.0
    }
}

impl Message for FromStem {
    type Result = ();
}

impl StreamHandler<FromStem, ws::ProtocolError> for Forwarder {
    fn handle(&mut self, msg: FromStem, ctx: &mut Self::Context) {
        println!("ws::Message received from stem");
        match msg.into_inner() {
            ws::Message::Text(text) => {
                println!("ws::Message was Text - appending it to the return message");
                self.response_body.append(&mut text.into_bytes());
            }
            ws::Message::Binary(bin) => {
                println!(
                    "ws::Message was Binary: {:?} - will not do anything with it...",
                    bin
                );
            }
            ws::Message::Close(reason) => {
                println!("ws::Message was Close - will send the return message to an Actor to submit to the callback, and then close the websockets connection");
                // println!("Response_body: {:?}", self.response_body);
                match self.callback_url.clone() {
                    Some(url) => {
                        self.sender.do_send(SendToCallback {
                            callback_url: url.clone(),
                            body: self.response_body.clone(),
                        });
                        ctx.close(reason);
                    }
                    None => {
                        ctx.close(Some(ws::CloseReason {
                            code: ws::CloseCode::Protocol,
                            description: Some(
                                "Close message received before callback url was specified."
                                    .to_string(),
                            ),
                        }));
                    }
                }
            }
            _ => {
                println!("Unhandled ws::Message (most likely a ping or a pong)");
            }
        }
    }
}

// helper function to connect to stem
#[async]
pub fn connect_to_stem() -> Result<(ws::ClientReader, ws::ClientWriter), ActixError> {
    let fut_reader_writer = await!({
        let mut client = ws::Client::new("ws://localhost:8083/v2/listen/stream"); // TODO: don't hardcode this - get it from a toml or env
        client = client.header(
            header::AUTHORIZATION,
            "Basic bmlrb2xhQGRlZXBncmFtLmNvbTpwd2Q=".to_string(),
        ); // TODO: have this function take the headers as input, and have it get the headers from the initial http request
        client.connect().map_err(|e| {
            println!("Error: {}", e);
            ()
        })
    });

    if fut_reader_writer.as_ref().is_err() {
        return Err(ErrorInternalServerError("Failed to connect to stem."));
    }

    let (reader, writer) = fut_reader_writer.unwrap();

    Ok((reader, writer))
}

// helper function to send return message to the callback - used by the Sender Actor
#[async]
pub fn send_to_callback(
    mut callback_url: url::Url,
    response_body: Vec<u8>,
) -> Result<(), ActixError> {
    let mut client_req: ClientRequestBuilder = ClientRequest::post(callback_url.clone()); // TODO: parse the url and send authorization headers

    let fut: SendRequest = client_req
        .timeout(std::time::Duration::from_secs(600))
        .body(response_body)
        .map_err(|e| {
            println!("Error: {}", e);
            ErrorInternalServerError("Failed to set callback request body.")
        })?
        .send()
        .timeout(std::time::Duration::from_secs(600));

    let response = await!(fut).map_err(|e| {
        println!("Error: {}", e);
        ErrorInternalServerError("Failed to sent callback request.")
    })?;

    Ok(())
}

// Sender will be the Actor which takes the aggregated body from the websockets Actor (Forwarder)
// as a message and sends it to the callback url
struct Sender;

impl Actor for Sender {
    type Context = Context<Self>;
}

struct SendToCallback {
    callback_url: url::Url,
    body: Vec<u8>,
}

impl Message for SendToCallback {
    type Result = Result<(), ActixError>;
}

impl Handler<SendToCallback> for Sender {
    type Result = Box<Future<Item = (), Error = ActixError>>;

    fn handle(&mut self, msg: SendToCallback, _: &mut Self::Context) -> Self::Result {
        println!("Sender received message SendToCallback.");
        Box::new(send_to_callback(msg.callback_url, msg.body))
    }
}

fn main() {
    ::std::env::set_var("RUST_LOG", "actix_web=trace");
    env_logger::init();
    let sys = actix::System::new("wsforwarder");

    server::new(|| {
        let sender_addr = Arc::new(Sender.start()); // using Arc here to get around lifetime and ownership shenanigans
        let sender_addr_cloned = sender_addr.clone();
        App::new()
            .middleware(middleware::Logger::default())
            .resource("/wsforwarder/", move |r| {
                r.with(move |req| ws_index(req, sender_addr_cloned.clone()))
            })
    })
    .bind("127.0.0.1:8080") // TODO: don't hardcode this - get it from a toml or env
    .unwrap()
    .start();

    let _ = sys.run();
}
