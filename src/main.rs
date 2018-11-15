#![allow(unused_variables)]
#![feature(generators)]
extern crate actix;
extern crate actix_web;
extern crate futures_await as futures;

#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate serde_json;

use actix::prelude::*;
use actix_web::http::header;
use actix_web::{middleware, server, ws, App, HttpRequest, HttpResponse};
use actix_web::error::{Error as ActixError, ErrorInternalServerError};
use actix_web::client::{ClientRequestBuilder, SendRequest, ClientRequest};
use futures::prelude::{async, await};
use futures::Future;
use futures::Stream;

mod utils;

// this creates and starts the Forwarder Actor
// it uses a modified "start" function which increases the default size of websockets messages
fn ws_index(req: HttpRequest) -> Box<Future<Item = HttpResponse, Error = ActixError>> {
    println!("Request received: {:?}", req);
    Box::new(Forwarder::with_request(req.clone()).and_then(move |actor| {
        utils::websockets::start(&req, actor, |stream| stream.max_size(10 * (1 << 20)))
    }))
}

// the Forwarder Actor
struct Forwarder {
    reader: Option<ws::ClientReader>,
    writer: Option<ws::ClientWriter>,
    response_body: Vec<u8>,
}

// "with_request" will be used to start the Forwarder Actor
// although the reader and writer will be populated later - after the first websockets text message is received
impl Forwarder {
    #[async]
    pub fn with_request(req: HttpRequest) -> Result<Self, ActixError> {
        let result = Self {
            reader: None,
            writer: None,
            response_body: Vec::new(),
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
                        println!("The text received was deserialized into the initial_message json object: {:#?}", result); // TODO: use the callback in this result
                        println!("Will attempt to connect to stem now.");
                        let (reader, writer) = connect_to_stem().wait().unwrap(); // TODO: take care of the unwrap TODO: take care of the wait
                        self.reader = Some(reader);
                        self.writer = Some(writer);
                        ctx.add_stream(self.reader.take().unwrap().map(FromEcho)); // shouldn't need to take care of this unwrap
                    },
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
            },
            ws::Message::Ping(msg) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.ping(&msg);
                }
            },
            ws::Message::Pong(msg) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.pong(&msg);
                }
            },
            ws::Message::Close(reason) => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.close(reason);
                }
            }
        }
    }
}

// TODO: "FromEcho" should change to "FromStem"
// this let's Forwarder handle messages coming FROM stem
struct FromEcho(ws::Message);

impl FromEcho {
    pub fn into_inner(self) -> ws::Message {
        self.0
    }
}

impl Message for FromEcho {
    type Result = ();
}

impl StreamHandler<FromEcho, ws::ProtocolError> for Forwarder {
    fn handle(&mut self, msg: FromEcho, ctx: &mut Self::Context) {
        println!("ws::Message received from stem");
        match msg.into_inner() {
            ws::Message::Text(text) => {
                println!("ws::Message was Text - appending it to the return message");
                self.response_body.append(&mut text.into_bytes());
            },
            ws::Message::Binary(bin) => {
                println!("ws::Message was Binary: {:?} - will not do anything with it...", bin);
            },
            ws::Message::Close(reason) => {
                println!("ws::Message was Close - will send the return message to an Actor to submit to the callback, and then close the websockets connection"); // TODO: make this Actor and do the callback
                // println!("Response_body: {:?}", self.response_body);
                send_to_callback(self.response_body.clone()).wait();
                ctx.close(reason);
            },
            _ => {
                println!("Unhandled ws::Message (most likely a ping or a pong)");
            },
        }
    }
}

// helper function to connect to stem
#[async]
pub fn connect_to_stem() -> Result<(ws::ClientReader, ws::ClientWriter), ActixError> {
    let fut_reader_writer = await!({
        let mut client = ws::Client::new("ws://localhost:8083/v2/listen/stream"); // TODO: don't hardcode this
        client = client.header(
            header::AUTHORIZATION,
            "Basic bmlrb2xhQGRlZXBncmFtLmNvbTpwd2Q=".to_string(),
        ); // TODO: have this function take the headers as input, and have it get the headers from the initial http request
        client.connect().map_err(|e| {
            println!("Error: {}", e);
            ()
        })
    });

    let (reader, writer) = fut_reader_writer.unwrap(); // TODO: handle this unwrap

    Ok((reader, writer))
}

// TODO: figure out a way to not have to "wait" on this (e.g. make an Actor before this if it receives a message from "do_send")
// helper function to send return message to the callback
#[async]
pub fn send_to_callback(response_body: Vec<u8>) -> Result<(), ActixError> {

    let mut client_req: ClientRequestBuilder = ClientRequest::post("http://localhost:8090/body/"); // TODO: make this function take the callback as an argument

    let fut: SendRequest = client_req
        .timeout(std::time::Duration::from_secs(600))
        .body(response_body)
        .map_err(|e| {
            println!("Error: {}", e);
            ErrorInternalServerError("Failed") // TODO: make better error message
        })?
        .send()
        .timeout(std::time::Duration::from_secs(600));

    let response = await!(fut)
        .map_err(|e| {
            println!("Error: {}", e);
            ErrorInternalServerError("Failed") // TODO: make better error message
        })?;

    Ok(())
}

fn main() {
    ::std::env::set_var("RUST_LOG", "actix_web=trace");
    env_logger::init();
    let sys = actix::System::new("wsforwarder");

    server::new(|| {
        App::new()
            .middleware(middleware::Logger::default())
            .resource("/wsforwarder/", move |r| r.with(move |req| ws_index(req)))
    })
    .bind("127.0.0.1:8080") // TODO: don't hardcode this
    .unwrap()
    .start();

    let _ = sys.run();
}
