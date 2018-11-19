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
extern crate openssl_probe;
#[macro_use]
extern crate log;
extern crate chrono;
extern crate env_logger;

use actix::prelude::*;
use actix_web::client::{ClientRequest, ClientRequestBuilder, SendRequest};
use actix_web::error::{Error as ActixError, ErrorBadRequest, ErrorInternalServerError};
use actix_web::http::header;
use actix_web::{middleware, server, ws, App, HttpRequest, HttpResponse};
use futures::prelude::{async, await};
use futures::Future;
use futures::Stream;
use std::time::Duration;

mod utils;
mod logging;

// TODO: add better error messages
// TODO: separate websocket Actors for connection to client and connection to stem

// this creates and starts the Forwarder Actor
// it uses a modified "start" function which increases the default size of websockets messages
fn ws_index(
    req: HttpRequest,
    sender_addr: Addr<Sender>,
    stem_url: String,
    default_bauth: String,
) -> Result<HttpResponse, ActixError> {
    info!("Request received: {:?}", req);
    Forwarder::with_request(&req, sender_addr, stem_url, default_bauth).and_then(move |actor| {
        utils::websockets::start(&req, actor, |stream| stream.max_size(10 * (1 << 20)))
    })
}

// the Forwarder Actor
pub struct Forwarder {
    callback_url: Option<url::Url>,
    reader: Option<ws::ClientReader>,
    writer: Option<ws::ClientWriter>,
    sender: Addr<Sender>,
    stem_url: String,
    bauth: String,
}

// "with_request" will be used to start the Forwarder Actor
// although the callback_url, reader, and writer will be populated later - after the first websockets text message is received
impl Forwarder {
    pub fn with_request(
        req: &HttpRequest,
        sender_addr: Addr<Sender>,
        stem_url: String,
        default_bauth: String,
    ) -> Result<Self, ActixError> {
        let result = Self {
            callback_url: None,
            reader: None,
            writer: None,
            sender: sender_addr,
            stem_url,
            bauth: default_bauth,
        };

        Ok(result)
    }
}

impl Actor for Forwarder {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!("Forwarder Actor started called.");
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        info!("Forwarder Actor stopping called.");
        info!("Stopping the Actor");
        Running::Stop
    }
}

#[derive(Deserialize, Debug)]
struct InitialMessage {
    callback: String,
    #[serde(rename = "content-type")]
    content_type: String,
    username: Option<String>,
    password: Option<String>,
}

// function to help parse audio format of the type: audio/l16;rate=16000
// currently, only l16 is supported, but any sample rate is supported
// the function returns a query string which should be appended to the stem url
pub fn parse_content_type(content_type: String) -> Result<String, ActixError> {
    // if content type was left empty, we don't need to append a query string, just assume they were uploading a file
    if content_type == "" {
        return Ok("".to_string());
    }

    let split: Vec<&str> = content_type.split(";").collect();

    if split.len() != 2 {
        error!("Failed to parse content-type.");
        return Err(ErrorBadRequest("Failed to parse content-type."));
    }

    let first = split[0].to_string();
    let second = split[1].to_string();
    let split_audio: Vec<&str> = first.split("/").collect();
    let split_rate: Vec<&str> = second.split("=").collect();

    if split_audio.len() != 2 {
        error!("Failed to parse content-type.");
        return Err(ErrorBadRequest("Failed to parse content-type."));
    }
    if split_rate.len() != 2 {
        error!("Failed to parse content-type.");
        return Err(ErrorBadRequest("Failed to parse content-type."));
    }

    let mut split_encoding = split_audio[1].to_string();
    let split_rate_value = split_rate[1].to_string();

    if split_encoding == "l16" {
        split_encoding = "linear16".to_string();
    } else {
        error!("Failed to parse content-type.");
        return Err(ErrorBadRequest("Failed to parse content-type."));
    }

    let result = format!(
        "?encoding={}&sample_rate={}&channels=1",
        split_encoding, split_rate_value
    );

    Ok(result)
}

impl StreamHandler<ws::Message, ws::ProtocolError> for Forwarder {
    fn handle(&mut self, msg: ws::Message, ctx: &mut Self::Context) {
        // info!("ws::Message received: {:?}", msg);
        match msg {
            ws::Message::Text(text) => {
                info!("Text received - will attempt to interpret as initial json object specifying callback and content-type.");

                let initial_message: Result<InitialMessage, serde_json::Error> =
                    serde_json::from_str(&text);
                match initial_message {
                    Ok(result) => {
                        info!("The text received was deserialized into an initial_message json object.");
                        info!("content_type: {}", result.content_type);
                        match parse_content_type(result.content_type) {
                            Ok(query_string) => {
                                info!("query_string: {}", query_string);
                                self.stem_url.push_str(&query_string);
                            }
                            Err(_) => {
                                error!("Failed to parse content-type.");
                                ctx.close(Some(ws::CloseReason {
                                    code: ws::CloseCode::Protocol,
                                    description: Some("Failed to parse content-type.".to_string()),
                                }));
                                ctx.stop();
                            }
                        }

                        if result.username.is_some() && result.password.is_some() {
                            self.bauth = utils::basic_auth::BasicAuthentication::from((
                                result.username.unwrap(),
                                result.password.unwrap(),
                            ))
                            .to_string();
                        }

                        let callback_url_option = url::Url::parse(&result.callback);
                        match callback_url_option {
                            Ok(callback_url) => {
                                self.callback_url = Some(callback_url);

                                info!("Will attempt to connect to stem now.");
                                connect_to_stem_act(
                                    self.stem_url.clone(),
                                    self.bauth.clone(),
                                    self,
                                    ctx,
                                );
                            }
                            Err(_) => {
                                error!("Failed to parse callbackurl.");
                                ctx.close(Some(ws::CloseReason {
                                    code: ws::CloseCode::Protocol,
                                    description: Some("Failed to parse callback url.".to_string()),
                                }));
                                ctx.stop();
                            }
                        }
                    }
                    Err(_) => {
                        error!("Failed to parse websockets text as json.");
                        ctx.close(Some(ws::CloseReason {
                            code: ws::CloseCode::Protocol,
                            description: Some(
                                "Failed to parse websockets text as json.".to_string(),
                            ),
                        }));
                        ctx.stop();
                    }
                }
            }
            ws::Message::Binary(bin) => {
                if !bin.is_empty() {
                    if let Some(writer) = self.writer.as_mut() {
                        writer.binary(bin);
                    }
                }
            }
            ws::Message::Ping(msg) => {
                info!("Received ping from client: {:?}", msg);
                info!("Will send pong back.");
                ctx.pong(&msg);
            },
            ws::Message::Pong(msg) => {
                info!("Received pong from client: {:?}", msg);
                info!("Will ignore.");
            },
            ws::Message::Close(reason) => {
                info!("Close message received from client - will close websocket connections to stem and the client.");
                info!("{:?}", reason);
                // close connection to stem
                if let Some(writer) = self.writer.as_mut() {
                    writer.close(reason.clone());
                }
                // close connection with client
                ctx.close(reason.clone());
                ctx.stop();
            }
        }
    }

    fn error(&mut self, err: ws::ProtocolError, ctx: &mut Self::Context) -> Running {
        error!("Client stream got an error... will...");
        error!("Stop.");
        Running::Stop
    }

    fn finished(&mut self, ctx: &mut Self::Context) {
        info!("Client stream handler finished.");
        ctx.stop();
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
    fn started(&mut self, ctx: &mut Self::Context) {
        info!("Stem stream handler started.");
    }

    fn handle(&mut self, msg: FromStem, ctx: &mut Self::Context) {
        info!("ws::Message received from stem");
        match msg.into_inner() {
            ws::Message::Text(text) => {
                info!("ws::Message was Text - will send it to the callback");
                match self.callback_url.clone() {
                    Some(url) => {
                        self.sender.do_send(SendToCallback {
                            callback_url: url.clone(),
                            body: text.into_bytes(),
                        });
                    }
                    None => {
                        error!("callback_url is None, but we are connected to stem - this should never happen");
                    }
                }
            }
            ws::Message::Binary(bin) => {
                info!(
                    "ws::Message was Binary: {:?} - will not do anything with it...",
                    bin
                );
            }
            ws::Message::Close(reason) => {
                info!("ws::Message was Close, closing connection with stem and client.");
                // close connection to stem
                if let Some(writer) = self.writer.as_mut() {
                    writer.close(reason.clone());
                }
                // close connection with client
                ctx.close(reason.clone());
                ctx.stop();
            }
            ws::Message::Ping(msg) => {
                info!("Received ping from stem: {:?}", msg);
                info!("Will attempt to send pong back.");
                if let Some(writer) = self.writer.as_mut() {
                    writer.pong(&msg);
                }
            }
            ws::Message::Pong(msg) => {
                info!("Received pong from stem: {:?}", msg);
                info!("Will ignore");
            }
        }
    }

    fn error(&mut self, err: ws::ProtocolError, ctx: &mut Self::Context) -> Running {
        error!("Stem stream got an error... will...");
        error!("Will attempt to reconnect to stem now.");
        connect_to_stem_act(self.stem_url.clone(), self.bauth.clone(), self, ctx);
        Running::Stop
    }

    fn finished(&mut self, ctx: &mut Self::Context) {
        info!("Stem stream handler finished.");
    }
}

// helper functions to connect to stem
pub fn connect_to_stem_act(
    stem_url: String,
    bauth: String,
    forwarder: &mut Forwarder,
    ctx: &mut ws::WebsocketContext<Forwarder>,
) {
    // TODO: make this do some loop waiting a second in between attempts or something
    connect_to_stem(stem_url, bauth)
        .into_actor(forwarder)
        .map(|(reader, writer), act, ctx| {
            act.reader = Some(reader);
            act.writer = Some(writer);
            ctx.add_stream(act.reader.take().unwrap().map(FromStem));
        })
        .map_err(|err, act, ctx| {
            error!("Failed to connect to stem. {:?}", err);
            ctx.close(Some(ws::CloseReason {
                code: ws::CloseCode::Protocol,
                description: Some("Failed to connect to stem.".to_string()),
            }));
            ctx.stop();
        })
        .wait(ctx);
}

pub fn connect_to_stem(stem_url: String, bauth: String) -> ws::ClientHandshake {
    let mut client = ws::Client::new(stem_url);
    client = client.header(header::AUTHORIZATION, bauth);
    client.connect()
}

// helper function to send return message to the callback - used by the Sender Actor
#[async]
pub fn send_to_callback(callback_url: url::Url, response_body: Vec<u8>) -> Result<(), ActixError> {
    let mut client_req: ClientRequestBuilder = ClientRequest::post(callback_url.clone());

    // check the callback_url to see if it has a username and password to use to make an authorization header
    let mut auth_header = match (callback_url.username(), callback_url.password()) {
        (username, Some(password)) if !username.is_empty() => {
            Some(utils::basic_auth::BasicAuthentication::from((username, password)).to_string())
        }
        _ => None,
    };

    if let Some(ref header) = auth_header {
        client_req.set_header("Authorization", header.clone());
    }

    let fut: SendRequest = client_req
        .timeout(std::time::Duration::from_secs(600))
        .body(response_body)
        .map_err(|e| {
            error!("Error: {}", e);
            ErrorInternalServerError("Failed to set callback request body.")
        })?
        .send()
        .timeout(std::time::Duration::from_secs(600)); // TODO: make this configurable

    let response = await!(fut).map_err(|e| {
        error!("Error: {}", e);
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
        info!("Sender received message SendToCallback.");
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
    openssl_probe::init_ssl_cert_env_vars();
    ::std::env::set_var("RUST_LOG", "actix_web=trace");

    logging::from_verbosity(
        3,
        Some(vec![
            "tokio_threadpool",
            "tokio_reactor",
            "tokio_core",
            "mio",
            "hyper",
            "trust-dns-proto",
        ]),
    );

    // get some config info from the environment
    let mut config = Config {
        stem_url: None,
        vonageproxy_url: None,
        default_bauth: None,
    };
    match std::env::var("STEM_URL") {
        Ok(url) => config.stem_url = Some(url),
        Err(_) => {
            error!("Error retreiving STEM_URL.");
            std::process::exit(1);;
        }
    }
    match std::env::var("VONAGEPROXY_URL") {
        Ok(url) => config.vonageproxy_url = Some(url),
        Err(_) => {
            error!("Error retreiving VONAGEPROXY_URL.");
            std::process::exit(1);;
        }
    }
    match std::env::var("DEFAULT_BAUTH") {
        Ok(bauth) => config.default_bauth = Some(bauth),
        Err(_) => {
            error!("Error retreiving DEFAULT_BAUTH.");
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

        let sender_addr2 = sender_addr.clone();
        let stem_url2 = config_clone.clone().stem_url.unwrap();
        let default_bauth2 = config_clone.clone().default_bauth.unwrap();

        App::new()
            .middleware(middleware::Logger::default())
            .resource("/", move |r| {
                r.with(move |req| {
                    ws_index(
                        req,
                        sender_addr.clone(),
                        stem_url.clone(),
                        default_bauth.clone(),
                    )
                })
            })
            .resource("//", move |r| {
                r.with(move |req| {
                    ws_index(
                        req,
                        sender_addr2.clone(),
                        stem_url2.clone(),
                        default_bauth2.clone(),
                    )
                })
            })
    })
    .bind(config.vonageproxy_url.unwrap())
    .unwrap()
    .start();

    let _ = sys.run();
}
