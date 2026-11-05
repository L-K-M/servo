/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Panic reports for embedders that install their own logger.

use std::thread;

use log::{Level, Log, Record, warn};
use servo_constellation::FromEmbedderLogger;

/// Tells Servo about a panic on the current thread, as the logger that
/// [`Servo::setup_logging`](crate::Servo::setup_logging) installs does when a panic hook logs
/// an error.
///
/// On such a report Servo closes the panicking script thread's pipelines, calls
/// [`WebViewDelegate::notify_crashed`](crate::WebViewDelegate::notify_crashed) for each affected
/// `WebView` and loads a crash page in their place. Without one, Servo notices the dead thread
/// only if a later message to it fails to send, which may never happen: the page stays on screen
/// but no longer runs script or responds to input, scrolling included.
///
/// Embedders that install their own global logger cannot call `setup_logging`. They get a
/// reporter from [`Servo::panic_reporter`](crate::Servo::panic_reporter) and call
/// [`Self::report_panic`] from their panic hook. A reporter is `Send` and `Sync`, so the hook
/// can use it on any thread.
///
/// With [`Opts::hard_fail`](crate::Opts::hard_fail), which `Opts::default()` sets for test
/// runners, the constellation exits the process on a reported panic instead; embedders that want
/// crash pages turn it off.
#[derive(Clone)]
pub struct PanicReporter(FromEmbedderLogger);

impl PanicReporter {
    pub(crate) fn new(logger: FromEmbedderLogger) -> Self {
        Self(logger)
    }

    /// Reports the panic that the current thread is handling, with `message` as its reason.
    ///
    /// Call this from a panic hook, and not for panics that Servo catches and recovers from
    /// itself (see [`should_panic_hook_suppress_termination`](crate::should_panic_hook_suppress_termination)).
    /// A panic on a thread that runs no script event loop crashes no page. Outside a panic the
    /// call reports nothing.
    pub fn report_panic(&self, message: &str) {
        if !thread::panicking() {
            return warn!("PanicReporter::report_panic called outside a panic");
        }

        // `FromEmbedderLogger` turns an error logged while the thread panics into a panic
        // report for the event loop that the current thread runs, if any.
        self.0.log(
            &Record::builder()
                .level(Level::Error)
                .target("servo::panic_reporter")
                .args(format_args!("{message}"))
                .build(),
        );
    }
}
