/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Native IME composition. A preview owns a live range separately from the collapsed caret,
//! so each update replaces that preview instead of appending another copy of the word.

use js::context::JSContext;
use keyboard_types::CompositionState;

use super::DocumentEventHandler;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::RangeBinding::RangeMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::Dom;
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::compositionevent::CompositionEvent;
use crate::dom::element::Element;
use crate::dom::event::Event;
use crate::dom::execcommand::execcommands::{DocumentExecCommandSupport, EditingCommandSource};
use crate::dom::node::Node;
use crate::dom::range::Range;
use crate::dom::types::{HTMLInputElement, HTMLTextAreaElement};
use embedder_traits::InputEventResult;

#[derive(Clone, JSTraceable, MallocSizeOf)]
pub(super) struct Composition {
    /// None is a tombstone: late updates/end cannot become legacy input in another field.
    target: Option<Dom<Element>>,
    epoch: u64,
    /// Text controls manage their preview themselves. Document editors need a live range.
    range: Option<Dom<Range>>,
    last_data: Option<DOMString>,
}

impl DocumentEventHandler {
    pub(crate) fn abandon_composition(&self) {
        self.composition_epoch
            .set(self.composition_epoch.get().wrapping_add(1));
        if let Some(session) = self.composition.borrow_mut().as_mut() {
            session.target = None;
            session.range = None;
            session.last_data = None;
        }
    }

    pub(crate) fn composition_is_current(&self, epoch: u64) -> bool {
        self.composition_epoch.get() == epoch
    }

    pub(crate) fn composition_epoch_for(&self, target: &Element) -> Option<u64> {
        self.composition
            .borrow()
            .as_ref()
            .filter(|session| {
                self.composition_is_current(session.epoch)
                    && session.target.as_deref() == Some(target)
            })
            .map(|session| session.epoch)
    }

    pub(crate) fn composition_preview_updated(&self, epoch: u64, data: DOMString) {
        if self.composition_is_current(epoch) {
            if let Some(session) = self.composition.borrow_mut().as_mut() {
                session.last_data = Some(data);
            }
        }
    }

    pub(crate) fn finish_composition_on_blur(&self, cx: &mut JSContext) {
        let ending = self
            .composition
            .borrow()
            .as_ref()
            .filter(|session| self.composition_is_current(session.epoch))
            .and_then(|session| {
                session.target.as_ref().map(|target| {
                    (
                        target.as_rooted(),
                        session.last_data.clone().unwrap_or_default(),
                    )
                })
            });
        self.abandon_composition();
        let Some((target, data)) = ending else { return };
        if let Some(input) = target.downcast::<HTMLInputElement>() {
            input.cancel_composition(cx);
        } else if let Some(textarea) = target.downcast::<HTMLTextAreaElement>() {
            textarea.cancel_composition(cx);
        }
        let event = CompositionEvent::new(
            cx,
            &self.window,
            CompositionState::End.event_type().into(),
            true,
            false,
            Some(&self.window),
            0,
            data,
        );
        let event = event.upcast::<Event>();
        event.set_composed(true);
        // The preview is already visible. Notify the page that composing ended, but do not
        // reinsert text through a form default after focus or selection has changed.
        event.mark_as_handled();
        event.fire(cx, target.upcast());
    }

    pub(super) fn dispatch_ime_composition(
        &self,
        cx: &mut JSContext,
        composition_event: keyboard_types::CompositionEvent,
    ) -> InputEventResult {
        let document = self.window.Document();
        let focused_area = document.focus_handler().focused_area();
        let state = composition_event.state;
        if state == CompositionState::Start {
            self.abandon_composition();
            *self.composition.borrow_mut() = Some(Composition {
                target: focused_area.element().map(Dom::from_ref),
                epoch: self.composition_epoch.get(),
                range: None,
                last_data: None,
            });
        }
        // A focus listener may drop the stored range while an event is firing. Keep the
        // local snapshot traced, including across compositionupdate and input callbacks.
        let mut session = RootedTraceableBox::new(self.composition.borrow().clone());
        let Some(target) = focused_area.element() else {
            if state == CompositionState::End {
                self.composition.borrow_mut().take();
            }
            return Default::default();
        };
        let data = DOMString::from(composition_event.data);
        if session.as_ref().is_some_and(|session| {
            !self.composition_is_current(session.epoch) || session.target.as_deref() != Some(target)
        }) {
            // Focus moved while the native input method still had an update queued.
            if state == CompositionState::End {
                self.composition.borrow_mut().take();
            }
            return Default::default();
        } else if state == CompositionState::Update && session.is_none() {
            return Default::default();
        }

        // The final DOM update precedes compositionend. Text controls retain their own
        // composition default action, including the legacy compositionend-only input path.
        if state == CompositionState::End {
            if let Some(session) = session.as_mut() {
                if session.range.is_some() && session.last_data.as_ref() != Some(&data) {
                    self.replace_composition(cx, session, data.clone());
                }
            }
            self.composition.borrow_mut().take();
        }

        let event = CompositionEvent::new(
            cx,
            &self.window,
            state.event_type().into(),
            true,
            state == CompositionState::Start,
            Some(&self.window),
            0,
            data.clone(),
        );
        let event = event.upcast::<Event>();
        event.set_composed(true);
        event.fire(cx, target.upcast());

        if state == CompositionState::Start {
            if session
                .as_ref()
                .is_none_or(|session| !self.composition_is_current(session.epoch))
            {
                return event.flags().into();
            }
            // A compositionstart listener can move focus or replace the editor's DOM. Take
            // the range only after it has run, and never retain a range in the next field.
            let range = if !target.is::<HTMLInputElement>()
                && !target.is::<HTMLTextAreaElement>()
                && document.focus_handler().focused_area().element() == Some(target)
            {
                document.GetSelection(cx).and_then(|selection| {
                    let range = selection.active_range(cx)?;
                    let host = target.upcast::<Node>().editing_host_of()?;
                    (range.start_container().editing_host_of().as_ref() == Some(&host)
                        && range.end_container().editing_host_of().as_ref() == Some(&host))
                    .then(|| Dom::from_ref(&*range.CloneRange(cx)))
                })
            } else {
                None
            };
            if let Some(session) = self.composition.borrow_mut().as_mut() {
                session.range = range;
            }
        } else if state == CompositionState::Update {
            if let Some(current) = session.as_mut() {
                if !self.composition_is_current(current.epoch) {
                    return event.flags().into();
                }
                if current.range.is_some() && !self.replace_composition(cx, current, data) {
                    current.range = None;
                }
                if self.composition_is_current(current.epoch) {
                    // Form defaults publish their actual preview while the update event runs.
                    current.last_data = self
                        .composition
                        .borrow()
                        .as_ref()
                        .and_then(|stored| stored.last_data.clone());
                    *self.composition.borrow_mut() = session.take();
                }
            }
        }
        event.flags().into()
    }

    /// Returns false when script has moved the selection, removed the editor or moved
    /// focus. In those cases the already visible preview stays, but no future update may
    /// delete unrelated text to resurrect a lost composition.
    fn replace_composition(
        &self,
        cx: &mut JSContext,
        session: &mut Composition,
        data: DOMString,
    ) -> bool {
        let document = self.window.Document();
        let Some(target) = session.target.as_deref() else {
            return false;
        };
        if !self.composition_is_current(session.epoch)
            || document.focus_handler().focused_area().element() != Some(target)
            || !target.upcast::<Node>().is_connected()
        {
            return false;
        }
        let Some(host) = target.upcast::<Node>().editing_host_of() else {
            return false;
        };
        let Some(range) = session.range.as_ref().map(|range| range.as_rooted()) else {
            return false;
        };
        if range.start_container().editing_host_of().as_ref() != Some(&host)
            || range.end_container().editing_host_of().as_ref() != Some(&host)
        {
            return false;
        }
        let Some(selection) = document.GetSelection(cx) else {
            return false;
        };
        let Some(current_range) = selection.active_range(cx) else {
            return false;
        };
        // After an update the caret is collapsed at the preview's end. Before the first
        // update it may instead select the text the user is replacing.
        let at_end = current_range.collapsed()
            && current_range.start_container() == range.end_container()
            && current_range.start_offset() == range.end_offset();
        let still_selected = session.last_data.is_none()
            && current_range.start_container() == range.start_container()
            && current_range.start_offset() == range.start_offset()
            && current_range.end_container() == range.end_container()
            && current_range.end_offset() == range.end_offset();
        if !at_end && !still_selected {
            return false;
        }
        // The page can obtain and mutate the selection's range. Never expose our private
        // preview range as that same object.
        let replacement = range.CloneRange(cx);
        selection.set_live_range(cx.no_gc(), Some(&replacement));
        if !document.exec_command_for_command_id(
            cx,
            DOMString::from_static("inserttext"),
            data.clone(),
            EditingCommandSource::Composition {
                data: data.clone(),
                range: range.clone(),
                epoch: session.epoch,
            },
        ) {
            return false;
        }
        session.last_data = Some(data);
        true
    }
}
