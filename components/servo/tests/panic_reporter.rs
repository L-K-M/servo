/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `PanicReporter` for embedders that install their own logger. The panic hook and the
//! logger are process-wide, so these tests keep their own binary.
mod common;

use std::panic::PanicHookInfo;
use std::rc::Rc;
use std::time::{Duration, Instant};

use log::{LevelFilter, Log, Metadata, Record};
use servo::{
    DevicePoint, MouseButton, Opts, PanicReporter, WebViewBuilder,
    should_panic_hook_suppress_termination,
};
use servo_config::prefs::Preferences;
use url::Url;

use crate::common::{
    ServoTest, WebViewDelegateImpl, click_at_point, show_webview_and_wait_for_rendering_to_be_ready,
};

/// Stands in for an embedder's own logger (Android's, for example): it keeps every record
/// to itself, so nothing reaches Servo's constellation through logging.
struct EmbedderLogger;

impl Log for EmbedderLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, _: &Record) {}

    fn flush(&self) {}
}

fn panic_message<'a>(info: &'a PanicHookInfo) -> &'a str {
    match info.payload().downcast_ref::<&'static str>() {
        Some(string) => string,
        None => match info.payload().downcast_ref::<String>() {
            Some(string) => string,
            None => "Box<Any>",
        },
    }
}

#[test]
fn test_panic_reporter_reports_script_thread_panic_as_crash() {
    let servo_test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_servo_helpers_enabled = true;
        let mut opts = Opts::default();
        opts.hard_fail = false;
        builder.preferences(preferences).opts(opts)
    });

    // The embedder's own logger takes the place of `Servo::setup_logging`, so only the
    // reporter can tell Servo about the panic.
    log::set_boxed_logger(Box::new(EmbedderLogger)).expect("no logger is installed yet");
    log::set_max_level(LevelFilter::Trace);

    let reporter: PanicReporter = servo_test.servo().panic_reporter();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo| {
        log::error!("{}", panic_message(info));
        if !should_panic_hook_suppress_termination() {
            reporter.report_panic(panic_message(info));
        }
    }));

    let url = Url::parse(
        "data:text/html,<button style='width: 500px; height: 500px' \
         onclick='ServoTestUtils.panic()'>crash</button>",
    )
    .unwrap();

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .url(url)
        .delegate(delegate.clone())
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    click_at_point(
        &webview,
        DevicePoint::new(100.0, 100.0),
        MouseButton::Primary,
    );

    // Without a report Servo never hears of the panic, so the wait needs a bound.
    let deadline = Instant::now() + Duration::from_secs(30);
    let delegate_clone = delegate.clone();
    servo_test.spin(move || delegate_clone.crashes.get() == 0 && Instant::now() < deadline);

    assert_eq!(
        delegate.crashes.get(),
        1,
        "a reported script thread panic crashes the page exactly once"
    );
}
