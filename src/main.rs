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
#[macro_use]
extern crate failure;
extern crate base64;

use actix::prelude::*;
use actix_web::client::{ClientRequest, ClientRequestBuilder, SendRequest};
use actix_web::error::{Error as ActixError, ErrorInternalServerError};
use actix_web::http::header;
use actix_web::{middleware, server, ws, App, HttpRequest, HttpResponse};
use futures::prelude::{async, await};
use futures::Future;
use futures::Stream;

mod utils;

// TODO: add better logging

// this creates and starts the Forwarder Actor
// it uses a modified "start" function which increases the default size of websockets messages
fn ws_index(
    req: HttpRequest,
    sender_addr: Addr<Sender>,
    stem_url: String,
    default_bauth: String,
) -> Result<HttpResponse, ActixError> {
    println!("Request received: {:?}", req);
    Forwarder::with_request(req.clone(), sender_addr, stem_url, default_bauth).and_then(move |actor| {
        utils::websockets::start(&req, actor, |stream| stream.max_size(10 * (1 << 20)))
    })
}

// the Forwarder Actor
pub struct Forwarder {
    initial_request: HttpRequest, // this will be useful when connecting to stem because it contains authorization headers
    callback_url: Option<url::Url>,
    reader: Option<ws::ClientReader>,
    writer: Option<ws::ClientWriter>,
    sender: Addr<Sender>,
    close_from_stem: bool,
    stem_url: String,
    bauth: String,
}

// "with_request" will be used to start the Forwarder Actor
// although the callback_url, reader, and writer will be populated later - after the first websockets text message is received
impl Forwarder {
    pub fn with_request(
        req: HttpRequest,
        sender_addr: Addr<Sender>,
        stem_url: String,
        default_bauth: String,
    ) -> Result<Self, ActixError> {
        let result = Self {
            initial_request: req.clone(),
            callback_url: None,
            reader: None,
            writer: None,
            sender: sender_addr,
            close_from_stem: false,
            stem_url: stem_url,
            bauth: default_bauth,
        };

        Ok(result)
    }
}

impl Actor for Forwarder {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        println!("Forwarder Actor started called.");
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        println!("Forwarder Actor stopping called.");
        println!("close_from_stem: {}", self.close_from_stem);

        if self.close_from_stem == false && self.writer.is_none() {
            println!("Actor wants to stop even though stem is connected and hasn't sent a close request, will attempt to reconnect to stem.");
            connect_to_stem_act(self.stem_url.clone(), self.bauth.clone(), self, ctx);
            return Running::Continue;
        }

        println!("Stopping the Actor");
        ctx.close(Some(ws::CloseReason {
            // TODO: this shows up as an error to the client, but it's not...
            code: ws::CloseCode::Protocol,
            description: Some("Job Finished".to_string()),
        }));
        Running::Stop
    }
}

#[derive(Deserialize, Debug)]
struct InitialMessage {
    callback: String,
    content_type: String,
    username: Option<String>,
    password: Option<String>,
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
                        if result.username.is_some() && result.password.is_some() {
                            self.bauth = utils::basic_auth::BasicAuthentication::from(
                                    (
                                        result.username.unwrap(),
                                        result.password.unwrap(),
                                    )
                                )
                                .to_string();
                        }

                        let callback_url_option = url::Url::parse(&result.callback);
                        match callback_url_option {
                            Ok(callback_url) => {
                                self.callback_url = Some(callback_url);

                                println!("Will attempt to connect to stem now.");
                                connect_to_stem_act(self.stem_url.clone(), self.bauth.clone(), self, ctx);
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
                println!("close message received from client");
                // close connection to stem
                if let Some(writer) = self.writer.as_mut() {
                    writer.close(reason.clone());
                }
                // close connection with client
                ctx.close(reason.clone());
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
                println!("ws::Message was Text - will send it to the callback");
                match self.callback_url.clone() {
                    Some(url) => {
                        self.sender.do_send(SendToCallback {
                            callback_url: url.clone(),
                            body: text.into_bytes(),
                        });
                    }
                    None => {
                        println!("callback_url is None, but we are connected to stem - this should never happen");
                    }
                }
            }
            ws::Message::Binary(bin) => {
                println!(
                    "ws::Message was Binary: {:?} - will not do anything with it...",
                    bin
                );
            }
            ws::Message::Close(reason) => {
                println!("ws::Message was Close, setting close_from_stem state variable.");
                self.close_from_stem = true;
            }
            _ => {
                println!("Unhandled ws::Message (most likely a ping or a pong)");
            }
        }
    }
}

// helper functions to connect to stem
pub fn connect_to_stem_act(
    stem_url: String,
    bauth: String,
    forwarder: &mut Forwarder,
    ctx: &mut ws::WebsocketContext<Forwarder>,
) {
    connect_to_stem(stem_url, bauth)
        .into_actor(forwarder)
        .map(|(reader, writer), act, ctx| {
            act.reader = Some(reader);
            act.writer = Some(writer);
            ctx.add_stream(act.reader.take().unwrap().map(FromStem));
        })
        .map_err(|err, act, ctx| {
            ctx.close(Some(ws::CloseReason {
                code: ws::CloseCode::Protocol,
                description: Some("Failed to connect to stem.".to_string()),
            }));
        })
        .wait(ctx);
}

pub fn connect_to_stem(stem_url: String, bauth: String) -> ws::ClientHandshake {
    let mut client = ws::Client::new(stem_url);
    client = client.header(
        header::AUTHORIZATION,
        bauth,
    );
    client.connect()
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
pub struct Sender;

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

#[derive(Clone)]
pub struct Config {
    stem_url: Option<String>,
    vonageproxy_url: Option<String>,
    default_bauth: Option<String>,
}

fn main() {
    ::std::env::set_var("RUST_LOG", "actix_web=trace");
    env_logger::init();

    // get some config info from the environment
    let mut config = Config {
        stem_url: None,
        vonageproxy_url: None,
        default_bauth: None,
    };
    match std::env::var("STEM_URL") {
        Ok(url) => config.stem_url = Some(url),
        Err(_) => {
            println!("Error retreiving STEM_URL.");
            std::process::exit(1);;
        }
    }
    match std::env::var("VONAGEPROXY_URL") {
        Ok(url) => config.vonageproxy_url = Some(url),
        Err(_) => {
            println!("Error retreiving VONAGEPROXY_URL.");
            std::process::exit(1);;
        }
    }
    match std::env::var("DEFAULT_BAUTH") {
        Ok(bauth) => config.default_bauth = Some(bauth),
        Err(_) => {
            println!("Error retreiving DEFAULT_BAUTH.");
            std::process::exit(1);;
        }
    }

    let sys = actix::System::new("wsforwarder");

    let sender_addr = Sender.start();
    let config_clone = config.clone();

    server::new(move || {
        let sender_addr = sender_addr.clone();
        let stem_url = config_clone.clone().stem_url.unwrap();
        let default_bauth = config_clone.clone().default_bauth.unwrap();
        App::new()
            .middleware(middleware::Logger::default())
            .resource("/wsforwarder/", move |r| {
                r.with(move |req| ws_index(req, sender_addr.clone(), stem_url.clone(), default_bauth.clone()))
            })
    })
    .bind(config.vonageproxy_url.unwrap())
    .unwrap()
    .start();

    let _ = sys.run();
}
