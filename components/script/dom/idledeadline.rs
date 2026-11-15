/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Cooperative scheduling of background tasks:
//! <https://www.w3.org/TR/requestidlecallback/>

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use crate::dom::bindings::callback::{ExceptionHandling, TracedCallback};
use crate::dom::bindings::codegen::Bindings::RequestIdleCallbackBinding::{
    IdleDeadlineMethods, IdleRequestCallback,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;

/// An idle period lasts at most 50 ms, so that the page stays responsive to new
/// input: <https://html.spec.whatwg.org/multipage/#event-loop-processing-model>
/// (computeDeadline).
const MAXIMUM_IDLE_PERIOD: Duration = Duration::from_millis(50);

/// The deadline of an idle period that started at `start`, evaluated at `now`: the
/// computeDeadline steps of
/// <https://html.spec.whatwg.org/multipage/#event-loop-processing-model>. A timer
/// or a rendering update requested in any window of the event loop during the
/// period brings its deadline forward.
fn idle_period_deadline(window: &Window, start: Instant, now: Instant) -> Instant {
    let deadline = start + MAXIMUM_IDLE_PERIOD;
    match window.script_thread().idle_period_deadline_limit(now) {
        Some(limit) => deadline.min(limit),
        None => deadline,
    }
}

/// <https://www.w3.org/TR/requestidlecallback/#the-idledeadline-interface>
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    window: Dom<Window>,
    /// The start of the idle period, or, for a callback that timed out, the time at
    /// which the timeout ran it.
    #[ignore_malloc_size_of = "Defined in std::time"]
    start: Instant,
    did_timeout: bool,
}

impl IdleDeadline {
    fn new(
        cx: &mut JSContext,
        window: &Window,
        start: Instant,
        did_timeout: bool,
    ) -> DomRoot<IdleDeadline> {
        reflect_dom_object_with_cx(
            Box::new(IdleDeadline {
                reflector_: Reflector::new(),
                window: Dom::from_ref(window),
                start,
                did_timeout,
            }),
            window,
            cx,
        )
    }

    /// <https://www.w3.org/TR/requestidlecallback/#dfn-get-deadline-time>, evaluated at
    /// `now`.
    fn deadline_at(&self, now: Instant) -> Instant {
        if self.did_timeout {
            return self.start;
        }
        idle_period_deadline(&self.window, self.start, now)
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://www.w3.org/TR/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self) -> Finite<f64> {
        let now = Instant::now();
        let remaining = self.deadline_at(now).saturating_duration_since(now);
        Finite::wrap(remaining.as_secs_f64() * 1000.)
    }

    /// <https://www.w3.org/TR/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}

type IdleCallbackList = VecDeque<(u32, TracedCallback<IdleRequestCallback>)>;

/// The idle callback state that the specification keeps on a [`Window`].
#[derive(Default, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct IdleCallbacks {
    /// <https://www.w3.org/TR/requestidlecallback/#dfn-idle-callback-identifier>
    identifier: Cell<u32>,
    /// <https://www.w3.org/TR/requestidlecallback/#dfn-list-of-idle-request-callbacks>
    idle_request_callbacks: DomRefCell<IdleCallbackList>,
    /// <https://www.w3.org/TR/requestidlecallback/#dfn-list-of-runnable-idle-callbacks>
    runnable_idle_callbacks: DomRefCell<IdleCallbackList>,
    /// The start of the latest idle period. It is in progress until its deadline.
    #[ignore_malloc_size_of = "Defined in std::time"]
    idle_period_start: Cell<Option<Instant>>,
}

impl IdleCallbacks {
    /// <https://www.w3.org/TR/requestidlecallback/#dom-window-requestidlecallback>
    pub(crate) fn request(
        &self,
        window: &Window,
        callback: TracedCallback<IdleRequestCallback>,
        timeout: u32,
    ) -> u32 {
        // Step 2. Increment the window's idle callback identifier by one.
        // Step 3. Let handle be the current value of window's idle callback identifier.
        let handle = self.identifier.get().wrapping_add(1);
        self.identifier.set(handle);

        // Step 4. Push callback to the end of window's list of idle request callbacks,
        // associated with handle.
        self.idle_request_callbacks
            .borrow_mut()
            .push_back((handle, callback));

        // Step 6. If the timeout property is present in options and has a value greater
        // than 0, then run the steps after a timeout given window, "requestIdleCallback",
        // timeout, and the step: queue a task on the idle-task task source to invoke the
        // idle callback timeout algorithm with window and handle.
        if timeout > 0 {
            window.as_global_scope().run_steps_after_a_timeout(
                DOMString::from("requestIdleCallback"),
                timeout.into(),
                move |_, global: &GlobalScope| {
                    let Some(window) = global.downcast::<Window>() else {
                        return;
                    };
                    let window = Trusted::new(window);
                    global.task_manager().idle_task_source().queue(
                        task!(invoke_idle_callback_timeout: move |cx| {
                            let window = window.root();
                            window.idle_callbacks().invoke_timeout(cx, &window, handle);
                        }),
                    );
                },
            );
        }

        // Step 5. Return handle and then continue running this algorithm asynchronously.
        handle
    }

    /// <https://www.w3.org/TR/requestidlecallback/#dom-window-cancelidlecallback>
    pub(crate) fn cancel(&self, handle: u32) {
        // Step 2. Find the entry in either the window's list of idle request callbacks or
        // list of runnable idle callbacks that is associated with the value handle.
        // Step 3. If there is such an entry, remove it from both lists.
        self.idle_request_callbacks
            .borrow_mut()
            .retain(|(entry, _)| *entry != handle);
        self.runnable_idle_callbacks
            .borrow_mut()
            .retain(|(entry, _)| *entry != handle);
    }

    /// <https://www.w3.org/TR/requestidlecallback/#start-an-idle-period-algorithm>
    ///
    /// The script thread calls this once it has no more tasks to run. An idle period
    /// lasts until its deadline, even after its callbacks ran, so callbacks requested
    /// during it wait for the next one: a callback that requests itself again runs at
    /// most once per period instead of keeping the event loop busy. A period that would
    /// end at once because a timer is due is not started; the timer wakes the event
    /// loop. Returns when the event loop must wake to start the next idle period.
    pub(crate) fn maybe_start_an_idle_period(&self, window: &Window) -> Option<Instant> {
        if self.idle_request_callbacks.borrow().is_empty() &&
            self.runnable_idle_callbacks.borrow().is_empty()
        {
            return None;
        }

        // An idle period whose deadline passed has ended, even if its task never ran
        // (it may have been cancelled with its document's other tasks).
        let now = Instant::now();
        if let Some(start) = self.idle_period_start.get() {
            let deadline = idle_period_deadline(window, start, now);
            if now < deadline {
                return Some(deadline);
            }
        }
        if idle_period_deadline(window, now, now) <= now {
            return None;
        }

        // Step 2-5. Move all entries from the list of idle request callbacks to the end
        // of the list of runnable idle callbacks, preserving their order.
        let pending = std::mem::take(&mut *self.idle_request_callbacks.borrow_mut());
        self.runnable_idle_callbacks.borrow_mut().extend(pending);

        // Step 6. Queue a task on the idle-task task source to invoke the idle callbacks
        // algorithm with window and getDeadline.
        self.idle_period_start.set(Some(now));
        Self::queue_invoke_idle_callbacks(window);
        None
    }

    fn queue_invoke_idle_callbacks(window: &Window) {
        let trusted_window = Trusted::new(window);
        window
            .as_global_scope()
            .task_manager()
            .idle_task_source()
            .queue(task!(invoke_idle_callbacks: move |cx| {
                let window = trusted_window.root();
                window.idle_callbacks().invoke_idle_callbacks(cx, &window);
            }));
    }

    /// <https://www.w3.org/TR/requestidlecallback/#invoke-idle-callbacks-algorithm>
    fn invoke_idle_callbacks(&self, cx: &mut JSContext, window: &Window) {
        let Some(start) = self.idle_period_start.get() else {
            return;
        };

        // Step 2. Let now be the current time.
        // Step 3. If now is less than the result of calling getDeadline and the window's
        // list of runnable idle callbacks is not empty:
        // Step 4. Otherwise, the idle period has ended: callbacks that did not run wait
        // for the next one.
        let now = Instant::now();
        if now >= idle_period_deadline(window, start, now) {
            return;
        }

        // Step 3.1. Pop the top callback from window's list of runnable idle callbacks.
        rooted!(&in(cx) let callback = self
            .runnable_idle_callbacks
            .borrow_mut()
            .pop_front()
            .map(|(_, callback)| callback));
        let Some(ref callback) = *callback else {
            return;
        };

        // Step 3.2. Let deadlineArg be a new IdleDeadline whose get deadline time
        // algorithm is getDeadline.
        // Step 3.3. Call callback with deadlineArg as its argument. If an uncaught
        // runtime script error occurs, then report the exception.
        let deadline_arg = IdleDeadline::new(cx, window, start, false);
        let _ = callback.Call__(cx, &deadline_arg, ExceptionHandling::Report);

        // Step 3.4. If window's list of runnable idle callbacks is not empty, queue a task
        // on the idle-task task source to invoke the idle callbacks algorithm.
        if !self.runnable_idle_callbacks.borrow().is_empty() {
            Self::queue_invoke_idle_callbacks(window);
        }
    }

    /// <https://www.w3.org/TR/requestidlecallback/#invoke-idle-callback-timeout-algorithm>
    fn invoke_timeout(&self, cx: &mut JSContext, window: &Window, handle: u32) {
        // Step 1. Let callback be the result of finding the entry in window's list of
        // idle request callbacks or list of runnable idle callbacks that is associated
        // with the value given by the handle argument.
        // Step 2. If callback is not undefined:
        // Step 2.1. Remove callback from both window's list of idle request callbacks and
        // its list of runnable idle callbacks.
        rooted!(&in(cx) let callback = Self::take(&self.idle_request_callbacks, handle)
            .or_else(|| Self::take(&self.runnable_idle_callbacks, handle)));
        let Some(ref callback) = *callback else {
            return;
        };

        // Step 2.2. Let now be the current time.
        // Step 2.3. Let deadlineArg be a new IdleDeadline whose get deadline time
        // algorithm returns now, and whose timeout is true.
        let deadline_arg = IdleDeadline::new(cx, window, Instant::now(), true);

        // Step 2.4. Call callback with deadlineArg as its argument. If an uncaught
        // runtime script error occurs, then report the exception.
        let _ = callback.Call__(cx, &deadline_arg, ExceptionHandling::Report);
    }

    /// Remove the entry associated with `handle` from `list` and return its callback.
    fn take(
        list: &DomRefCell<IdleCallbackList>,
        handle: u32,
    ) -> Option<TracedCallback<IdleRequestCallback>> {
        let mut list = list.borrow_mut();
        let index = list.iter().position(|(entry, _)| *entry == handle)?;
        list.remove(index).map(|(_, callback)| callback)
    }
}
