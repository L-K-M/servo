/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Embedder-supplied text stays with the context menu that authorized Paste.
mod common;

use std::rc::Rc;
use std::time::{Duration, Instant};

use common::{
    ServoTest, WebViewDelegateImpl, click_at_point, evaluate_javascript,
    show_webview_and_wait_for_rendering_to_be_ready,
};
use servo::{
    ClipboardDelegate, ContextMenu, EmbedderControl, JSValue, MouseButton, StringRequest, WebView,
    WebViewBuilder,
};
use url::Url;

const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
const PASTE_SETTLE_TIME: Duration = Duration::from_millis(250);

struct NoClipboardReads;

impl ClipboardDelegate for NoClipboardReads {
    fn get_text(&self, _: WebView, _: StringRequest) {
        panic!("An explicit paste must not request shared clipboard contents");
    }
}

fn fixture(test: &ServoTest) -> (WebView, Rc<WebViewDelegateImpl>) {
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let view = WebViewBuilder::new(test.servo(), test.rendering_context.clone())
        .delegate(delegate.clone())
        .clipboard_delegate(Rc::new(NoClipboardReads))
        .url(Url::parse(
            "data:text/html,<!doctype html><div id=host></div><script>
             let root = document.getElementById('host').attachShadow({mode:'closed'});
             window.field = document.createElement('input');
             field.type = 'password'; field.value = 'AB';
             field.style.cssText = 'width:200px;height:200px';
             root.appendChild(field); field.focus(); field.setSelectionRange(1,1);
             window.pastes = 0; window.inputs = 0;
             field.onpaste = e => { window.pastes++; window.pasted = e.clipboardData.getData('text/plain');
                 if (window.cancelPaste) e.preventDefault(); };
             field.oninput = e => { window.inputs++; window.inputType = e.inputType; };
             </script>",
        ).unwrap())
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(test, &view, &delegate);
    (view, delegate)
}

fn open_menu(test: &ServoTest, view: &WebView, delegate: &WebViewDelegateImpl) -> ContextMenu {
    click_at_point(view, euclid::Point2D::new(50., 50.), MouseButton::Secondary);
    let deadline = Instant::now() + ACTION_TIMEOUT;
    test.spin(|| {
        Instant::now() < deadline
            && !delegate
                .controls_shown
                .borrow()
                .iter()
                .any(|control| matches!(control, EmbedderControl::ContextMenu(_)))
    });
    let mut controls = delegate.controls_shown.borrow_mut();
    let index = controls
        .iter()
        .position(|control| matches!(control, EmbedderControl::ContextMenu(_)))
        .expect("A secondary click on a password field must offer a menu");
    let EmbedderControl::ContextMenu(menu) = controls.remove(index) else {
        unreachable!()
    };
    menu
}

#[test]
fn explicit_paste_preserves_selection_and_paste_events_in_a_closed_shadow_field() {
    let test = ServoTest::new();
    let (view, delegate) = fixture(&test);
    open_menu(&test, &view, &delegate).paste_text("été".into());
    // Default insertion queues the input event; wait for that event as well as the value.
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let result = evaluate_javascript(
            &test,
            view.clone(),
            "field.value === 'AétéB' && pastes === 1 && pasted === 'été' && inputs === 1 && inputType === 'insertFromPaste'",
        );
        if result == Ok(JSValue::Boolean(true)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Paste did not reach its field and events: {result:?}"
        );
    }
}

#[test]
fn a_cancelled_paste_does_not_insert_text() {
    let test = ServoTest::new();
    let (view, delegate) = fixture(&test);
    evaluate_javascript(&test, view.clone(), "window.cancelPaste = true").unwrap();
    open_menu(&test, &view, &delegate).paste_text("private".into());
    assert_eq!(
        evaluate_javascript(
            &test,
            view,
            "field.value === 'AB' && pastes === 1 && inputs === 0"
        ),
        Ok(JSValue::Boolean(true)),
    );
}

#[test]
fn a_removed_field_cannot_receive_a_late_paste() {
    let test = ServoTest::new();
    let (view, delegate) = fixture(&test);
    let menu = open_menu(&test, &view, &delegate);
    evaluate_javascript(&test, view.clone(), "field.remove()").unwrap();
    menu.paste_text("private".into());
    assert_eq!(
        evaluate_javascript(
            &test,
            view,
            "field.value === 'AB' && pastes === 0 && inputs === 0"
        ),
        Ok(JSValue::Boolean(true)),
    );
}

#[test]
fn a_field_adopted_by_another_document_cannot_receive_a_late_paste() {
    let test = ServoTest::new();
    let (view, delegate) = fixture(&test);
    let menu = open_menu(&test, &view, &delegate);
    // Use a live frame: an inert document would make the stale response trivial to reject.
    evaluate_javascript(
        &test,
        view.clone(),
        "window.iframe = document.createElement('iframe'); \
         iframe.src = 'about:blank'; document.body.appendChild(iframe); void 0",
    )
    .unwrap();
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let result = evaluate_javascript(
            &test,
            view.clone(),
            "!!(iframe.contentDocument && iframe.contentDocument.body)",
        );
        if result == Ok(JSValue::Boolean(true)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "iframe document never became ready: {result:?}"
        );
    }
    evaluate_javascript(
        &test,
        view.clone(),
        "iframe.contentDocument.body.appendChild(iframe.contentDocument.adoptNode(field)); void 0",
    )
    .unwrap();
    menu.paste_text("private".into());
    // Give the retained menu's response a bounded window to reach (and be
    // rejected by) the script thread before asserting that nothing landed.
    let deadline = Instant::now() + PASTE_SETTLE_TIME;
    test.spin(|| Instant::now() < deadline);
    assert_eq!(
        evaluate_javascript(
            &test,
            view,
            "field.value === 'AB' && pastes === 0 && inputs === 0"
        ),
        Ok(JSValue::Boolean(true)),
    );
}
