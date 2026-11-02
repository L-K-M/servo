/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Tests for `WebView::stop_loading`.

mod common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::Duration;

use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{make_body, make_server};
use servo::{
    JSValue, LoadStatus, NavigationRequest, PipelineId, StopLoadingError, WebView, WebViewBuilder,
    WebViewDelegate,
};

use crate::common::{ServoTest, WebViewDelegateImpl};

/// Evaluate `script` in the `WebView`'s active document and wait for the result.
/// Unlike `common::evaluate_javascript`, this does not first wait for
/// [`LoadStatus::Complete`], which may never arrive once loading is stopped.
fn evaluate(servo_test: &ServoTest, webview: &WebView, script: &str) -> JSValue {
    let result = Rc::new(RefCell::new(None));
    let callback_result = result.clone();
    webview.evaluate_javascript(script.to_string(), move |value| {
        *callback_result.borrow_mut() = Some(value.expect("JavaScript evaluation should succeed"));
    });
    let spin_result = result.clone();
    servo_test.spin(move || spin_result.borrow().is_none());
    result.borrow_mut().take().unwrap()
}

/// Call [`WebView::stop_loading`] and wait for the completion callback.
fn stop_loading(servo_test: &ServoTest, webview: &WebView) -> Result<PipelineId, StopLoadingError> {
    let result = Rc::new(RefCell::new(None));
    let callback_result = result.clone();
    webview
        .stop_loading(move |value| {
            *callback_result.borrow_mut() = Some(value);
        })
        .expect("stop_loading should not fail synchronously");
    let spin_result = result.clone();
    servo_test.spin(move || spin_result.borrow().is_none());
    result.borrow_mut().take().unwrap()
}

/// A partially loaded document must survive `stop_loading`: the parser is
/// aborted, its pending fetch is cancelled, and delivering the blocked
/// resource later must not execute it.
#[test]
fn test_stop_loading_aborts_blocked_script_and_keeps_document() {
    let servo_test = ServoTest::new();

    let (script_requested, script_request) = channel::<()>();
    let (release_script, release_gate) = channel::<()>();
    let handler =
        move |request: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            if request.uri().path() == "/blocked.js" {
                let _ = script_requested.send(());
                // Hold the script response until the test releases it.
                let _ = release_gate.recv();
                *response.body_mut() = make_body(b"window.__blockedScriptRan = true;".to_vec());
            } else {
                *response.body_mut() = make_body(
                    b"<html><body>\
                    <article id=\"partial\"></article>\
                    <script src=\"/blocked.js\"></script>\
                    <div id=\"after\"></div>\
                </body></html>"
                        .to_vec(),
                );
            }
        };
    let (server, url) = make_server(handler);

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.as_url().clone())
        .build();

    // Wait until the parser-blocking script request has been made. At this point
    // the document is committed and its parser is stalled on the blocked fetch.
    servo_test.spin(|| script_request.try_recv().is_err());

    let pipeline_id = stop_loading(&servo_test, &webview).expect("stop_loading should succeed");
    assert_eq!(webview.active_document_id(), Some(pipeline_id));

    // The committed part of the document is preserved, while content after the
    // blocked script was never parsed.
    assert_eq!(
        evaluate(
            &servo_test,
            &webview,
            "document.getElementById('partial') != null"
        ),
        JSValue::Boolean(true)
    );
    assert_eq!(
        evaluate(
            &servo_test,
            &webview,
            "document.getElementById('after') != null"
        ),
        JSValue::Boolean(false)
    );

    // Releasing the blocked response must not execute the cancelled script.
    release_script
        .send(())
        .expect("Should release the blocked script");
    std::thread::sleep(Duration::from_millis(200));
    servo_test.servo().spin_event_loop();
    assert_eq!(
        evaluate(&servo_test, &webview, "window.__blockedScriptRan"),
        JSValue::Undefined
    );

    server.close();
}

/// A navigation that has not committed yet must be cancelled by
/// `stop_loading` and must never enter session history. A subsequent
/// navigation must still succeed.
#[test]
fn test_stop_loading_cancels_pending_navigation() {
    let servo_test = ServoTest::new();

    let (server_a, url_a) = make_server(|_, response| {
        *response.body_mut() = make_body(b"page a".to_vec());
    });
    let (navigation_requested, navigation_request) = channel::<()>();
    let (release_navigation, navigation_gate) = channel::<()>();
    let (server_b, url_b) = make_server(move |_, response| {
        let _ = navigation_requested.send(());
        // Hold the response until the test releases it.
        let _ = navigation_gate.recv();
        *response.body_mut() = make_body(b"page b".to_vec());
    });
    let (server_c, url_c) = make_server(|_, response| {
        *response.body_mut() = make_body(b"page c".to_vec());
    });

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url_a.as_url().clone())
        .build();
    servo_test.spin(move || !delegate.load_status_changed.get());
    assert_eq!(webview.url(), Some(url_a.as_url().clone()));

    // Start a navigation and wait for its fetch to start, so that `stop_loading`
    // runs while the navigation is committed nowhere yet.
    webview.load(url_b.as_url().clone());
    servo_test.spin(|| navigation_request.try_recv().is_err());

    let pipeline_id = stop_loading(&servo_test, &webview).expect("stop_loading should succeed");
    assert_eq!(webview.active_document_id(), Some(pipeline_id));
    assert_eq!(webview.url(), Some(url_a.as_url().clone()));
    assert!(!webview.can_go_back());

    // Releasing the pending response changes nothing.
    release_navigation
        .send(())
        .expect("Should release the pending navigation");
    std::thread::sleep(Duration::from_millis(200));
    servo_test.servo().spin_event_loop();
    assert_eq!(webview.url(), Some(url_a.as_url().clone()));
    assert!(!webview.can_go_back());

    // A subsequent navigation succeeds normally.
    delegate.reset();
    webview.load(url_c.as_url().clone());
    servo_test.spin(move || !delegate.load_status_changed.get());
    assert_eq!(webview.url(), Some(url_c.as_url().clone()));
    assert!(webview.can_go_back());

    server_a.close();
    server_b.close();
    server_c.close();
}

/// Stopping before the first document has committed must leave the `WebView`
/// with an empty but usable initial document.
#[test]
fn test_stop_loading_before_first_commit_leaves_reusable_view() {
    let servo_test = ServoTest::new();

    let (initial_requested, initial_request) = channel::<()>();
    let (release_initial, initial_gate) = channel::<()>();
    let (server_a, url_a) = make_server(move |_, response| {
        let _ = initial_requested.send(());
        // Hold the response until the test releases it.
        let _ = initial_gate.recv();
        *response.body_mut() = make_body(b"page a".to_vec());
    });
    let (server_b, url_b) = make_server(|_, response| {
        *response.body_mut() = make_body(b"page b".to_vec());
    });

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url_a.as_url().clone())
        .build();

    // Wait until the initial load's fetch has started, then stop it.
    servo_test.spin(|| initial_request.try_recv().is_err());

    let pipeline_id = stop_loading(&servo_test, &webview).expect("stop_loading should succeed");

    // The WebView now shows an empty initial document that can still be used.
    assert_eq!(webview.active_document_id(), Some(pipeline_id));
    assert_eq!(webview.url(), Some(url::Url::parse("about:blank").unwrap()));

    // Releasing the cancelled load changes nothing.
    release_initial
        .send(())
        .expect("Should release the initial load");
    std::thread::sleep(Duration::from_millis(200));
    servo_test.servo().spin_event_loop();
    assert_eq!(webview.url(), Some(url::Url::parse("about:blank").unwrap()));

    // A subsequent navigation succeeds.
    delegate.reset();
    webview.load(url_b.as_url().clone());
    servo_test.spin(move || !delegate.load_status_changed.get());
    assert_eq!(webview.url(), Some(url_b.as_url().clone()));
    assert!(webview.can_go_back());

    server_a.close();
    server_b.close();
}

/// A delegate that holds script-initiated navigation approvals until the test
/// responds to them.
#[derive(Default)]
struct DelayedApprovalDelegate {
    url_changed: Cell<bool>,
    load_status_changed: Cell<bool>,
    pending_navigations: RefCell<Vec<NavigationRequest>>,
}

impl WebViewDelegate for DelayedApprovalDelegate {
    fn notify_url_changed(&self, _webview: WebView, _url: url::Url) {
        self.url_changed.set(true);
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if status == LoadStatus::Complete {
            self.load_status_changed.set(true);
        }
    }

    fn request_navigation(&self, _webview: WebView, navigation_request: NavigationRequest) {
        self.pending_navigations
            .borrow_mut()
            .push(navigation_request);
    }
}

/// A navigation waiting on embedder approval is cancelled by `stop_loading`;
/// a late approval must not resurrect it.
#[test]
fn test_stop_loading_cancels_navigation_pending_approval() {
    let servo_test = ServoTest::new();

    let (server_a, url_a) = make_server(|_, response| {
        *response.body_mut() = make_body(b"page a".to_vec());
    });
    let (server_b, url_b) = make_server(|_, response| {
        *response.body_mut() = make_body(b"page b".to_vec());
    });

    let delegate = Rc::new(DelayedApprovalDelegate::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url_a.as_url().clone())
        .build();
    servo_test.spin(move || !delegate.load_status_changed.get());
    assert_eq!(webview.url(), Some(url_a.as_url().clone()));

    // Trigger a script navigation; the delegate captures the approval request
    // and holds it.
    webview.evaluate_javascript(
        format!("window.location.href = '{}';", url_b.as_url()),
        |_| {},
    );
    servo_test.spin(|| delegate.pending_navigations.borrow().is_empty());

    let pipeline_id = stop_loading(&servo_test, &webview).expect("stop_loading should succeed");
    assert_eq!(webview.active_document_id(), Some(pipeline_id));

    // A late approval must not resurrect the cancelled navigation.
    delegate
        .pending_navigations
        .borrow_mut()
        .drain(..)
        .for_each(|request| request.allow());
    std::thread::sleep(Duration::from_millis(200));
    servo_test.servo().spin_event_loop();
    assert_eq!(webview.url(), Some(url_a.as_url().clone()));
    assert!(!webview.can_go_back());

    server_a.close();
    server_b.close();
}

/// When the `WebView` is closed while a `stop_loading` result is pending, the
/// callback still runs exactly once with [`StopLoadingError::WebViewClosed`].
#[test]
fn test_stop_loading_reports_webview_closed() {
    let servo_test = ServoTest::new();

    let (initial_requested, initial_request) = channel::<()>();
    let (release_initial, initial_gate) = channel::<()>();
    let (server, url) = make_server(move |_, response| {
        let _ = initial_requested.send(());
        // Hold the response until the test releases it.
        let _ = initial_gate.recv();
        *response.body_mut() = make_body(b"page a".to_vec());
    });

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.as_url().clone())
        .build();

    // Wait until the initial load's fetch has started, then stop it and
    // immediately close the `WebView` while the result is still pending.
    servo_test.spin(|| initial_request.try_recv().is_err());

    let result = Rc::new(RefCell::new(None));
    let callback_result = result.clone();
    webview
        .stop_loading(move |value| {
            *callback_result.borrow_mut() = Some(value);
        })
        .expect("stop_loading should not fail synchronously");
    drop(webview);

    let spin_result = result.clone();
    servo_test.spin(move || spin_result.borrow().is_none());
    assert_eq!(
        result.borrow_mut().take().unwrap(),
        Err(StopLoadingError::WebViewClosed)
    );

    release_initial
        .send(())
        .expect("Should release the initial load");
    server.close();
}
