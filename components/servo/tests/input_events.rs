/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Input completion distinguishes coalescing from failed dispatch and page vetoes.

mod common;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use servo::{
    InputEvent, InputEventId, InputEventResult, LoadStatus, MouseMoveEvent, TouchEvent,
    TouchEventType, TouchId, TouchPointerType, WebView, WebViewBuilder, WebViewDelegate,
};
use webrender_api::units::DevicePoint;

use crate::common::{ServoTest, wait_for_webview_scene_to_be_up_to_date};

#[derive(Default)]
struct InputDelegate {
    results: RefCell<HashMap<InputEventId, InputEventResult>>,
}

impl WebViewDelegate for InputDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
    }

    fn notify_input_event_handled(
        &self,
        _webview: WebView,
        id: InputEventId,
        result: InputEventResult,
    ) {
        assert!(self.results.borrow_mut().insert(id, result).is_none());
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PageAction {
    Allowed,
    Prevented,
}

fn check_coalesced_moves(action: PageAction) {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(InputDelegate::default());
    let handler = match action {
        PageAction::Allowed => "",
        PageAction::Prevented => "event.preventDefault();",
    };
    let html = format!(
        "data:text/html,<!doctype html><style>body{{margin:0;height:100vh}}</style>\
        <script>addEventListener('touchmove',event=>{{{handler}}},{{passive:false}})</script>"
    );
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url::Url::parse(&html).unwrap())
        .build();
    servo_test.spin(|| webview.load_status() != LoadStatus::Complete);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);

    let touch = |phase, y| {
        webview.notify_input_event(InputEvent::Touch(TouchEvent::new(
            phase,
            TouchId(0),
            DevicePoint::new(250., y).into(),
            TouchPointerType::Touch,
        )))
    };
    let down = touch(TouchEventType::Down, 200.);
    servo_test.spin(|| !delegate.results.borrow().contains_key(&down));

    // Do not pump between moves: the first DOM reply is still pending.
    let first = touch(TouchEventType::Move, 220.);
    let coalesced = touch(TouchEventType::Move, 240.);
    servo_test.spin(|| {
        let results = delegate.results.borrow();
        !results.contains_key(&first) || !results.contains_key(&coalesced)
    });
    let results = delegate.results.borrow();
    let skipped = results[&coalesced];
    assert!(skipped.contains(InputEventResult::Consumed));
    assert!(!skipped.contains(InputEventResult::DispatchFailed));
    assert!(!skipped.contains(InputEventResult::DefaultPrevented));
    assert_eq!(
        results[&first].contains(InputEventResult::DefaultPrevented),
        action == PageAction::Prevented
    );
    drop(results);

    let up = touch(TouchEventType::Up, 240.);
    servo_test.spin(|| !delegate.results.borrow().contains_key(&up));

    // A genuinely missing hit target must retain its failure result.
    let outside = webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
        DevicePoint::new(-100., -100.).into(),
    )));
    servo_test.spin(|| !delegate.results.borrow().contains_key(&outside));
    assert!(delegate.results.borrow()[&outside].contains(InputEventResult::DispatchFailed));
}

#[test]
fn test_coalesced_moves_are_consumed_not_failed() {
    check_coalesced_moves(PageAction::Allowed);
}

#[test]
fn test_coalescing_preserves_the_dispatched_moves_page_veto() {
    check_coalesced_moves(PageAction::Prevented);
}
