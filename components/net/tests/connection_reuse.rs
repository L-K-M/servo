/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A request sent on a pooled connection that the server, or the network, has already closed
//! fails although nothing answered it. These servers answer the first request on each
//! connection and then misbehave below HTTP on later requests, as a server whose idle timeout
//! races the next request does. Requests that may safely go out again are sent once more on a
//! new connection; the others fail.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use http::Method;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::request::{Referrer, Request, RequestBuilder};
use net_traits::response::{Response, ResponseBody};
use parking_lot::Mutex;
use rustls::crypto::aws_lc_rs;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use servo_base::id::TEST_WEBVIEW_ID;
use servo_url::ServoUrl;

use crate::{fetch_with_context, new_fetch_context};

static CERTIFICATE: &[u8] = include_bytes!("self_signed_certificate_for_testing.crt");
static PRIVATE_KEY: &[u8] = include_bytes!("privatekey_for_testing.key");

/// What the server does with the second and later requests on a connection.
#[derive(Clone, Copy)]
enum LaterRequests {
    /// Close the connection after reading the request, without an answer or a GOAWAY.
    Close,
    /// Reset the HTTP/2 stream with `REFUSED_STREAM`: the request was not processed.
    RefuseStream,
    /// Reset the HTTP/2 stream with `INTERNAL_ERROR`: the request failed on the server.
    ResetStream,
    /// Send GOAWAY without error naming the first stream as the last one processed, then close.
    GoAway,
}

/// What a test server saw, in order: "<connection> <method> <path>" for each answered request,
/// with " unanswered" appended for the requests it did not answer.
type ServerLog = Arc<Mutex<Vec<String>>>;

/// Serves every accepted connection on its own thread with `serve`, which gets the connection's
/// index (0 for the first one).
fn spawn_server(serve: impl Fn(TcpStream, usize) + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve = Arc::new(serve);
    thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else {
                return;
            };
            let serve = serve.clone();
            thread::spawn(move || serve(stream, index));
        }
    });
    port
}

/// Reads an HTTP/1.1 request head and returns its method and path.
fn read_http1_request(reader: &mut impl BufRead) -> Option<String> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    Some(format!("{} {}", parts.next()?, parts.next()?))
}

/// An HTTP/1.1 server that answers the first request on each connection and keeps the
/// connection open, then closes it after reading the next request.
fn spawn_http1_server(log: ServerLog) -> ServoUrl {
    let port = spawn_server(move |stream, connection| {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let Some(request) = read_http1_request(&mut reader) else {
            return;
        };
        log.lock().push(format!("{connection} {request}"));
        writer
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();

        if let Some(request) = read_http1_request(&mut reader) {
            log.lock()
                .push(format!("{connection} {request} unanswered"));
        }
        let _ = writer.shutdown(Shutdown::Both);
    });
    ServoUrl::parse(&format!("http://127.0.0.1:{port}/")).unwrap()
}

const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_GOAWAY: u8 = 0x7;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const ERROR_NO_ERROR: u32 = 0x0;
const ERROR_INTERNAL_ERROR: u32 = 0x2;
const ERROR_REFUSED_STREAM: u32 = 0x7;
/// HPACK: the static table's ":status: 200" (RFC 7541, appendix A).
const HPACK_STATUS_200: u8 = 0x88;
const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn write_frame(stream: &mut impl Write, kind: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let length = (payload.len() as u32).to_be_bytes();
    let mut frame = vec![length[1], length[2], length[3], kind, flags];
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

/// Reads one HTTP/2 frame: its type, flags and stream id. The payload is skipped.
fn read_frame(stream: &mut impl Read) -> Option<(u8, u8, u32)> {
    let mut header = [0; 9];
    stream.read_exact(&mut header).ok()?;
    let length = u32::from_be_bytes([0, header[0], header[1], header[2]]);
    let stream_id = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff;
    let mut payload = vec![0; length as usize];
    stream.read_exact(&mut payload).ok()?;
    Some((header[3], header[4], stream_id))
}

fn tls_server_config() -> (Arc<ServerConfig>, Vec<CertificateDer<'static>>) {
    let certificates = CertificateDer::pem_slice_iter(CERTIFICATE)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(PRIVATE_KEY).unwrap();
    let mut config = ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certificates.clone(), key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    (Arc::new(config), certificates)
}

/// An HTTP/2 server over TLS that answers the first stream on each connection and keeps the
/// connection open, then treats later streams as `later` says. Requests are recorded as
/// "<connection> <stream id>". Hand-written frames let it close a connection, reset a stream
/// or send a GOAWAY at an exact point of an exchange.
fn spawn_http2_server(
    later: LaterRequests,
    log: ServerLog,
) -> (ServoUrl, Vec<CertificateDer<'static>>) {
    let (config, certificates) = tls_server_config();
    let port = spawn_server(move |stream, connection| {
        let mut stream = StreamOwned::new(ServerConnection::new(config.clone()).unwrap(), stream);
        let mut preface = [0; CLIENT_PREFACE.len()];
        if stream.read_exact(&mut preface).is_err() || preface != CLIENT_PREFACE {
            return;
        }
        write_frame(&mut stream, FRAME_SETTINGS, 0, 0, &[]);

        let mut first_stream = None;
        while let Some((kind, flags, stream_id)) = read_frame(&mut stream) {
            match kind {
                FRAME_SETTINGS if flags & FLAG_ACK == 0 => {
                    write_frame(&mut stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
                },
                FRAME_HEADERS if first_stream.is_none() => {
                    first_stream = Some(stream_id);
                    log.lock().push(format!("{connection} stream {stream_id}"));
                    let headers = [HPACK_STATUS_200];
                    write_frame(
                        &mut stream,
                        FRAME_HEADERS,
                        FLAG_END_HEADERS,
                        stream_id,
                        &headers,
                    );
                    write_frame(&mut stream, FRAME_DATA, FLAG_END_STREAM, stream_id, b"ok");
                },
                FRAME_HEADERS => {
                    log.lock()
                        .push(format!("{connection} stream {stream_id} unanswered"));
                    match later {
                        LaterRequests::Close => break,
                        LaterRequests::RefuseStream => {
                            let code = ERROR_REFUSED_STREAM.to_be_bytes();
                            write_frame(&mut stream, FRAME_RST_STREAM, 0, stream_id, &code);
                        },
                        LaterRequests::ResetStream => {
                            let code = ERROR_INTERNAL_ERROR.to_be_bytes();
                            write_frame(&mut stream, FRAME_RST_STREAM, 0, stream_id, &code);
                        },
                        LaterRequests::GoAway => {
                            let mut payload = first_stream.unwrap().to_be_bytes().to_vec();
                            payload.extend_from_slice(&ERROR_NO_ERROR.to_be_bytes());
                            write_frame(&mut stream, FRAME_GOAWAY, 0, 0, &payload);
                            break;
                        },
                    }
                },
                _ => {},
            }
        }
        stream.conn.send_close_notify();
        let _ = stream.flush();
        let _ = stream.sock.shutdown(Shutdown::Both);
    });
    let url = ServoUrl::parse(&format!("https://127.0.0.1:{port}/")).unwrap();
    (url, certificates)
}

fn request(url: &ServoUrl, method: Method) -> Request {
    let url = UrlWithBlobClaim::new(url.clone(), None);
    RequestBuilder::new(Some(TEST_WEBVIEW_ID), url.clone(), Referrer::NoReferrer)
        .method(method)
        .origin(url.origin())
        .policy_container(Default::default())
        .build()
}

fn body_text(response: &Response) -> String {
    match &*response.actual_response().body.lock() {
        ResponseBody::Done(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => panic!("the response body did not finish"),
    }
}

/// Fetches `url` with GET so that the server answers on a connection the client keeps, then
/// fetches it again with `method`, which goes out on the same connection.
fn fetch_twice(
    url: &ServoUrl,
    certificates: &[CertificateDer<'static>],
    method: Method,
) -> Response {
    let mut context = new_fetch_context(None, None);
    for certificate in certificates {
        context.state.override_manager.add_override(certificate);
    }

    let first = fetch_with_context(request(url, Method::GET), &mut context);
    assert!(!first.is_network_error(), "{:?}", first.get_network_error());
    assert_eq!(body_text(&first), "ok");

    // An HTTP/1.1 connection goes back to the pool once hyper has seen the end of the response,
    // which can be just after the fetch completes.
    thread::sleep(Duration::from_millis(200));
    fetch_with_context(request(url, method), &mut context)
}

fn assert_sent_again(response: &Response, log: &ServerLog, expected: &[&str]) {
    assert!(
        !response.is_network_error(),
        "{:?}",
        response.get_network_error()
    );
    assert_eq!(body_text(response), "ok");
    assert_eq!(*log.lock(), expected);
}

fn assert_not_sent_again(response: &Response, log: &ServerLog, expected: &[&str]) {
    assert!(response.is_network_error());
    assert_eq!(*log.lock(), expected);
}

#[test]
fn test_get_is_sent_again_after_a_reused_http1_connection_closes() {
    let log = ServerLog::default();
    let url = spawn_http1_server(log.clone());

    let response = fetch_twice(&url, &[], Method::GET);
    assert_sent_again(
        &response,
        &log,
        &["0 GET /", "0 GET / unanswered", "1 GET /"],
    );
}

#[test]
fn test_post_is_not_sent_again_after_a_reused_http1_connection_closes() {
    // The server may have acted on the POST before the connection closed.
    let log = ServerLog::default();
    let url = spawn_http1_server(log.clone());

    let response = fetch_twice(&url, &[], Method::POST);
    assert_not_sent_again(&response, &log, &["0 GET /", "0 POST / unanswered"]);
}

#[test]
fn test_get_is_sent_again_after_a_reused_http2_connection_closes() {
    let log = ServerLog::default();
    let (url, certificates) = spawn_http2_server(LaterRequests::Close, log.clone());

    let response = fetch_twice(&url, &certificates, Method::GET);
    assert_sent_again(
        &response,
        &log,
        &["0 stream 1", "0 stream 3 unanswered", "1 stream 1"],
    );
}

#[test]
fn test_post_is_sent_again_after_an_http2_server_refuses_its_stream() {
    // REFUSED_STREAM guarantees that the server did not process the request (RFC 9113, 8.7).
    let log = ServerLog::default();
    let (url, certificates) = spawn_http2_server(LaterRequests::RefuseStream, log.clone());

    let response = fetch_twice(&url, &certificates, Method::POST);
    assert_sent_again(
        &response,
        &log,
        &["0 stream 1", "0 stream 3 unanswered", "1 stream 1"],
    );
}

#[test]
fn test_post_is_sent_again_after_an_http2_goaway_before_its_stream() {
    // A GOAWAY whose last stream id is below the request's stream guarantees that the server
    // did not process the request (RFC 9113, 6.8).
    let log = ServerLog::default();
    let (url, certificates) = spawn_http2_server(LaterRequests::GoAway, log.clone());

    let response = fetch_twice(&url, &certificates, Method::POST);
    assert_sent_again(
        &response,
        &log,
        &["0 stream 1", "0 stream 3 unanswered", "1 stream 1"],
    );
}

#[test]
fn test_get_is_not_sent_again_after_an_http2_stream_error() {
    // A stream reset for another reason than a refusal is the server's answer, not a lost
    // connection: sending the request again would only repeat it.
    let log = ServerLog::default();
    let (url, certificates) = spawn_http2_server(LaterRequests::ResetStream, log.clone());

    let response = fetch_twice(&url, &certificates, Method::GET);
    assert_not_sent_again(&response, &log, &["0 stream 1", "0 stream 3 unanswered"]);
}
