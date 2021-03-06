//! Tests for client.

use std::str;
use std::thread;
use std::time::Duration;

extern crate bytes;
extern crate httpbis;
extern crate futures;
extern crate native_tls;
extern crate tokio_core;
extern crate tokio_tls;
#[macro_use]
extern crate log;
extern crate env_logger;

use bytes::Bytes;

use futures::Future;

mod test_misc;

use httpbis::solicit::header::*;
use httpbis::error::Error;
use httpbis::error::ErrorCode;
use httpbis::*;
use httpbis::for_test::*;

use test_misc::*;

#[test]
fn stream_count() {
    env_logger::init().ok();

    let server = HttpServerTester::new();

    let client: Client =
        Client::new("::1", server.port(), false, Default::default()).expect("connect");

    let mut server_tester = server.accept();
    server_tester.recv_preface();
    server_tester.settings_xchg();

    let state: ConnectionStateSnapshot = client.dump_state().wait().expect("state");
    assert_eq!(0, state.streams.len());

    let req = client.start_post("/foobar", "localhost", Bytes::from(&b"xxyy"[..])).collect();

    let headers = server_tester.recv_frame_headers_check(1, false);
    assert_eq!("POST", headers.get(":method"));
    assert_eq!("/foobar", headers.get(":path"));

    let data = server_tester.recv_frame_data_check(1, true);
    assert_eq!(b"xxyy", &data[..]);

    let mut resp_headers = Headers::new();
    resp_headers.add(":status", "200");
    server_tester.send_headers(1, resp_headers, false);

    server_tester.send_data(1, b"aabb", true);

    let message = req.wait().expect("r");
    assert_eq!((b"aabb"[..]).to_owned(), message.body);

    let state: ConnectionStateSnapshot = client.dump_state().wait().expect("state");
    assert_eq!(0, state.streams.len(), "{:?}", state);
}

#[test]
fn rst_is_error() {
    env_logger::init().ok();

    let server = HttpServerTester::new();

    let client: Client =
        Client::new("::1", server.port(), false, Default::default()).expect("connect");

    let mut server_tester = server.accept();
    server_tester.recv_preface();
    server_tester.settings_xchg();

    let req = client.start_get("/fgfg", "localhost").collect();

    let get = server_tester.recv_message(1);
    assert_eq!("GET", get.headers.method());

    server_tester.send_headers(1, Headers::ok_200(), false);
    server_tester.send_rst(1, ErrorCode::InadequateSecurity);

    match req.wait() {
        Ok(..) => panic!("expected error"),
        Err(Error::CodeError(ErrorCode::InadequateSecurity)) => {},
        Err(e) => panic!("wrong error: {:?}", e),
    }

    let state: ConnectionStateSnapshot = client.dump_state().wait().expect("state");
    assert_eq!(0, state.streams.len(), "{:?}", state);
}

#[test]
fn client_call_dropped() {
    env_logger::init().ok();

    let server = HttpServerTester::new();

    let client: Client =
        Client::new("::1", server.port(), false, Default::default()).expect("connect");

    let mut server_tester = server.accept();
    server_tester.recv_preface();
    server_tester.settings_xchg();

    {
        let req = client.start_get("/fgfg", "localhost");

        server_tester.recv_message(1);

        drop(req);

        server_tester.send_headers(1, Headers::ok_200(), true);
    }

    {
        let req = client.start_get("/fgfg", "localhost").collect();
        server_tester.recv_message(3);
        server_tester.send_headers(3, Headers::ok_200(), true);
        let resp = req.wait().expect("OK");
        assert_eq!(200, resp.headers.status());
    }

    let state: ConnectionStateSnapshot = client.dump_state().wait().expect("state");
    assert_eq!(0, state.streams.len(), "{:?}", state);
}

#[test]
fn reconnect_on_disconnect() {
    env_logger::init().ok();

    let server = HttpServerTester::new();

    let client: Client =
        Client::new("::1", server.port(), false, Default::default()).expect("connect");

    let mut server_tester = server.accept();
    server_tester.recv_preface();
    server_tester.settings_xchg();

    {
        let req = client.start_get("/111", "localhost").collect();
        server_tester.recv_message(1);
        server_tester.send_headers(1, Headers::ok_200(), true);
        let resp = req.wait().expect("OK");
        assert_eq!(200, resp.headers.status());
    }

    // drop server connection
    drop(server_tester);

    // waiting for client connection to die
    while let Ok(_) = client.dump_state().wait() {
        thread::sleep(Duration::from_millis(1));
    }

    {
        let req = client.start_get("/222", "localhost").collect();

        let mut server_tester = server.accept();
        server_tester.recv_preface();
        server_tester.settings_xchg_but_ack();

        server_tester.recv_message(1);
        server_tester.send_headers(1, Headers::ok_200(), true);
        let resp = req.wait().expect("OK");
        assert_eq!(200, resp.headers.status());
    }
}

#[test]
fn reconnect_on_goaway() {
    env_logger::init().ok();

    let server = HttpServerTester::new();

    let client: Client =
        Client::new("::1", server.port(), false, Default::default()).expect("connect");

    {
        let mut server_tester = server.accept();
        server_tester.recv_preface();
        server_tester.settings_xchg();

        let req = client.start_get("/111", "localhost").collect();
        server_tester.recv_message(1);
        server_tester.send_headers(1, Headers::ok_200(), true);
        let resp = req.wait().expect("OK");
        assert_eq!(200, resp.headers.status());

        server_tester.send_goaway(1);

        server_tester.recv_eof();
    }

    {
        let connect = client.wait_for_connect();

        let mut server_tester = server.accept();
        server_tester.recv_preface();
        server_tester.settings_xchg();

        connect.wait().expect("connect");

        let req = client.start_get("/111", "localhost").collect();

        server_tester.recv_message(1);
        server_tester.send_headers(1, Headers::ok_200(), true);
        let resp = req.wait().expect("OK");
        assert_eq!(200, resp.headers.status());
    }
}
