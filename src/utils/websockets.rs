use actix::{Actor, StreamHandler};
use actix_web::dev::Payload;
use actix_web::error::Error;
use actix_web::ws::{handshake, Message, ProtocolError, WebsocketContext, WsStream};
use actix_web::{HttpMessage, HttpRequest, HttpResponse};

pub fn start<F, A, S>(req: &HttpRequest<S>, actor: A, map: F) -> Result<HttpResponse, Error>
where
    A: Actor<Context = WebsocketContext<A, S>> + StreamHandler<Message, ProtocolError>,
    S: 'static,
    F: FnOnce(WsStream<Payload>) -> WsStream<Payload>,
{
    let mut resp = handshake(req)?;
    let stream = map(WsStream::new(req.payload()));

    let body = WebsocketContext::create(req.clone(), actor, stream);
    Ok(resp.body(body))
}
