/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Manual regression for fallback creation failure; see DOWNSTREAM.md.
mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc::channel;

use net::test_util::{make_body, make_server};
use servo::{StopLoadingError, WebViewBuilder};

use crate::common::ServoTest;

#[test]
#[ignore = "Requires temporary fallback creation failure; see DOWNSTREAM.md"]
fn failed_initial_stop_completes() {
    let servo_test = ServoTest::new();
    let (requested, request) = channel::<()>();
    let (release, gate) = channel::<()>();
    let gate = Mutex::new(gate);
    let (server, url) = make_server(move |_, response| {
        let _ = requested.send(());
        let _ = gate.lock().unwrap().recv();
        *response.body_mut() = make_body(b"cancelled initial response".to_vec());
    });
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .url(url.as_url().clone())
        .build();
    servo_test.spin(|| request.try_recv().is_err());

    let result = Rc::new(RefCell::new(None));
    let reply = result.clone();
    webview
        .stop_loading(move |value| *reply.borrow_mut() = Some(value))
        .unwrap();
    servo_test.spin(|| result.borrow().is_none());
    assert_eq!(
        *result.borrow(),
        Some(Err(StopLoadingError::DocumentUnavailable))
    );

    release.send(()).unwrap();
    server.close();
}
