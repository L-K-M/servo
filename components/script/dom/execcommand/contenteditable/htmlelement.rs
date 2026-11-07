/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use embedder_traits::{EmbedderControlRequest, InputMethodRequest, InputMethodType};
use html5ever::local_name;
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::RangeBinding::RangeMethods;
use script_bindings::inheritance::Castable;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::properties::{LonghandId, PropertyDeclarationId, ShorthandId};

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::SelectionBinding::SelectionMethods;
use crate::dom::bindings::inheritance::{ElementTypeId, HTMLElementTypeId, NodeTypeId};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::document_embedder_controls::ControlElement;
use crate::dom::element::Element;
use crate::dom::event::Event;
use crate::dom::execcommand::basecommand::{CommandName, CssPropertyName};
use crate::dom::execcommand::contenteditable::node::move_preserving_ranges;
use crate::dom::html::form_controls::htmlinputelement::HTMLInputElement;
use crate::dom::html::htmlanchorelement::HTMLAnchorElement;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::htmlfontelement::HTMLFontElement;
use crate::dom::html::htmltextareaelement::HTMLTextAreaElement;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::node::{Node, NodeTraits};
use crate::dom::text::Text;

impl HTMLElement {
    pub(crate) fn local_name(&self) -> &str {
        self.upcast::<Element>().local_name()
    }

    fn remove_value_from_text_decoration(&self, cx: &mut JSContext, value: &str) {
        let element = self.upcast::<Element>();
        let mut original_value = String::new();
        let property;

        // Ensure that style borrow is dropped before writing new value for style
        {
            let style_attribute = element.style_attribute().borrow();
            let Some(declarations) = style_attribute.as_ref() else {
                return;
            };
            let document = element.owner_document();
            let shared_lock = document.style_shared_author_lock();
            let read_lock = shared_lock.read();
            let style = declarations.read_with(&read_lock);

            // First we need to check if text-decoration is set as shorthand.
            // If that's not the case, we should only remove underline from text-decoration-line
            if style
                .shorthand_to_css(ShorthandId::TextDecoration, &mut original_value)
                .is_ok()
            {
                property = CssPropertyName::TextDecoration;
            } else if let Some((text_decoration, _)) = style.get(PropertyDeclarationId::Longhand(
                LonghandId::TextDecorationLine,
            )) {
                if text_decoration.to_css(&mut original_value).is_ok() {
                    property = CssPropertyName::TextDecorationLine;
                } else {
                    return;
                }
            } else {
                return;
            }
        }

        let new_value = original_value
            .replace(&format!(" {value} "), " ")
            .replace(&format!(" {value}"), "")
            .replace(&format!("{value} "), "")
            .replace(value, "");
        if new_value.is_empty() {
            property.remove_from_element(cx, self);
        } else {
            property.set_for_element(cx, self, new_value.into());
        }
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#clear-the-value>
    pub(crate) fn clear_the_value(&self, cx: &mut JSContext, command: &CommandName) {
        // Step 1. Let command be the current command.
        //
        // Passed in as argument

        let node = self.upcast::<Node>();
        let element = self.upcast::<Element>();

        // Step 2. If element is not editable, return the empty list.
        if !node.is_editable() {
            return;
        }
        // Step 3. If element's specified command value for command is null,
        // return the empty list.
        if element.specified_command_value(command).is_none() {
            return;
        }
        // Step 4. If element is a simple modifiable element:
        let node_parent = node.GetParentNode().expect("Must always have a parent");
        if element.is_simple_modifiable_element() {
            // Step 4.1. Let children be the children of element.
            // Step 4.2. For each child in children, insert child into element's parent immediately before element, preserving ranges.
            for child in node.children() {
                move_preserving_ranges(cx, &child, |cx| {
                    node_parent.InsertBefore(cx, &child, Some(node))
                });
            }
            // Step 4.3. Remove element from its parent.
            node.remove_self(cx);
            // Step 4.4. Return children.
            return;
        }
        match command {
            // Step 5. If command is "strikethrough", and element has a style attribute
            // that sets "text-decoration" to some value containing "line-through",
            // delete "line-through" from the value.
            CommandName::Strikethrough => {
                self.remove_value_from_text_decoration(cx, "line-through");
            },
            // Step 6. If command is "underline", and element has a style attribute that
            // sets "text-decoration" to some value containing "underline", delete "underline" from the value.
            CommandName::Underline => {
                self.remove_value_from_text_decoration(cx, "underline");
            },
            _ => {},
        }
        // Step 7. If the relevant CSS property for command is not null,
        // unset that property of element.
        if let Some(property) = command.relevant_css_property() {
            property.remove_from_element(cx, self);
        }
        // In case we have a completely empty style attribute, we need to completely remove it.
        // Otherwise, when you call `innerHTML`, it would generate a `style=""`, which is
        // not what the tests expect. They expect the whole attribute to be removed.
        if element.has_empty_style_attribute() {
            element.remove_attribute_by_name(cx, &local_name!("style"));
        }
        // Step 8. If element is a font element:
        if self.is::<HTMLFontElement>() {
            match command {
                // Step 8.1. If command is "foreColor", unset element's color attribute, if set.
                CommandName::ForeColor => {
                    element.remove_attribute_by_name(cx, &local_name!("color"));
                },
                // Step 8.2. If command is "fontName", unset element's face attribute, if set.
                CommandName::FontName => {
                    element.remove_attribute_by_name(cx, &local_name!("face"));
                },
                // Step 8.3. If command is "fontSize", unset element's size attribute, if set.
                CommandName::FontSize => {
                    element.remove_attribute_by_name(cx, &local_name!("size"));
                },
                _ => {},
            }
        }
        // Step 9. If element is an a element and command is "createLink" or "unlink",
        // unset the href property of element.
        if self.is::<HTMLAnchorElement>() &&
            matches!(command, CommandName::CreateLink | CommandName::Unlink)
        {
            element.remove_attribute_by_name(cx, &local_name!("href"));
        }
        // Step 10. If element's specified command value for command is null,
        // return the empty list.
        if element.specified_command_value(command).is_none() {
            return;
        }
        // Step 11. Set the tag name of element to "span",
        // and return the one-node list consisting of the result.
        element.set_the_tag_name(cx, "span");
    }

    /// Asks the embedder for an input method while this editing host has focus, as text
    /// controls do, so that a virtual keyboard can open for typing into it; hides it again
    /// on blur.
    pub(crate) fn handle_focus_event_for_editing_host(&self, cx: &mut JSContext, event: &Event) {
        // Script cannot open or close a keyboard by dispatching focus events of its own.
        if !event.IsTrusted() {
            return;
        }
        if let Some(selection) = self.owner_document().selection() {
            selection.set_visible_selection_dirty();
        }
        let event_type = event.type_();
        if *event_type == *"blur" {
            self.owner_document()
                .event_handler()
                .finish_composition_on_blur(cx);
        }
        if self.is_text_control() {
            return;
        }
        if *event_type == *"blur" {
            self.owner_document()
                .embedder_controls()
                .hide_input_method(self);
        } else if *event_type == *"focus" &&
            self.is_editing_host() &&
            // A focus listener may have moved focus on: no blur would hide the input method.
            self.is_focused() &&
            !self
                .owner_document()
                .embedder_controls()
                .has_input_method(self)
        {
            self.show_input_method_for_editing_host();
        }
    }

    /// Keeps the input method of a focused element in step with whether it is an editing host
    /// when its `contenteditable` attribute changes, which fires no focus or blur event.
    pub(crate) fn editing_host_state_changed(&self) {
        if self.is_text_control() {
            return;
        }
        let document = self.owner_document();
        if let Some(selection) = document.selection() {
            selection.set_visible_selection_dirty();
        }
        if !self.is_editing_host() {
            document.embedder_controls().hide_input_method(self);
            return;
        }
        if self.is_focused() && !document.embedder_controls().has_input_method(self) {
            self.show_input_method_for_editing_host();
        }
    }

    fn is_focused(&self) -> bool {
        self.owner_document()
            .focus_handler()
            .focused_area()
            .element()
            .is_some_and(|element| element == self.upcast::<Element>())
    }

    /// Text controls ask for their own input method, with their value and caret.
    fn is_text_control(&self) -> bool {
        self.is::<HTMLInputElement>() || self.is::<HTMLTextAreaElement>()
    }

    fn show_input_method_for_editing_host(&self) {
        self.owner_document()
            .embedder_controls()
            .show_embedder_control(
                ControlElement::Ime(Dom::from_ref(self)),
                EmbedderControlRequest::InputMethod(InputMethodRequest {
                    input_method_type: InputMethodType::Text,
                    // An editing host's content is not plain text with a single caret offset, so
                    // none is reported: the embedder's input method starts without context.
                    text: String::new(),
                    insertion_point: None,
                    // Enter inserts a paragraph in an editing host.
                    multiline: true,
                    // As for text controls: like Chromium, a virtual keyboard only once the user
                    // has interacted with the page.
                    allow_virtual_keyboard: self.owner_window().has_sticky_activation(),
                }),
                None,
            );
    }

    /// There is no specification for this implementation. Instead, it is
    /// reverse-engineered based on the WPT test
    /// /selection/contenteditable/initial-selection-on-focus.tentative.html
    pub(crate) fn handle_focus_state_for_contenteditable(&self, cx: &mut JSContext) {
        if !self.is_editing_host() {
            return;
        }
        let document = self.owner_document();
        let Some(selection) = document.GetSelection(cx) else {
            return;
        };
        let range = self
            .upcast::<Element>()
            .ensure_contenteditable_selection_range(cx, &document);
        // If the current range is already associated with this contenteditable
        // element, then we shouldn't do anything. This is important when focus
        // is lost and regained, but selection was changed beforehand. In that
        // case, we should maintain the selection as it were, by not creating
        // a new range.
        if selection
            .active_range(cx)
            .is_some_and(|active| active == range)
        {
            return;
        }
        let node = self.upcast::<Node>();
        let mut selected_node = DomRoot::from_ref(node);
        let mut previous_eligible_node = DomRoot::from_ref(node);
        let mut previous_node = DomRoot::from_ref(node);
        let mut selected_offset = 0;
        for child in node.traverse_preorder(ShadowIncluding::Yes) {
            if let Some(text) = child.downcast::<Text>() {
                // Note that to consider it whitespace, it needs to take more
                // into account than simply "it has a non-whitespace" character.
                // Therefore, we need to first check if it is not a whitespace
                // node and only then can we find what the relevant character is.
                if !text.is_whitespace_node() {
                    // A node with "white-space: pre" set must select its first
                    // character, regardless if that's a whitespace character or not.
                    let is_pre_formatted_text_node = child
                        .GetParentElement()
                        .and_then(|parent| parent.style())
                        .is_some_and(|style| {
                            style.get_inherited_text().white_space_collapse ==
                                WhiteSpaceCollapse::Preserve
                        });
                    if !is_pre_formatted_text_node {
                        // If it isn't pre-formatted, then we should instead select the
                        // first non-whitespace character.
                        selected_offset = text
                            .data()
                            .find(|c: char| !c.is_whitespace())
                            .unwrap_or_default() as u32;
                    }
                    selected_node = child;
                    break;
                }
            }
            // For <input>, <textarea>, <hr> and <br> elements, we should select the previous
            // node, regardless if it was a block node or not
            if matches!(
                child.type_id(),
                NodeTypeId::Element(ElementTypeId::HTMLElement(
                    HTMLElementTypeId::HTMLInputElement,
                )) | NodeTypeId::Element(ElementTypeId::HTMLElement(
                    HTMLElementTypeId::HTMLTextAreaElement,
                )) | NodeTypeId::Element(ElementTypeId::HTMLElement(
                    HTMLElementTypeId::HTMLHRElement,
                )) | NodeTypeId::Element(ElementTypeId::HTMLElement(
                    HTMLElementTypeId::HTMLBRElement,
                ))
            ) {
                selected_node = previous_node;
                break;
            }
            // When we encounter a non-contenteditable element, we should select the previous
            // eligible node
            if child
                .downcast::<HTMLElement>()
                .is_some_and(|el| el.ContentEditable().str() == "false")
            {
                selected_node = previous_eligible_node;
                break;
            }
            // We can only select block nodes as eligible nodes for the case of non-conenteditable
            // nodes
            if child.is_block_node() {
                previous_eligible_node = child.clone();
            }
            previous_node = child;
        }
        let _ = range.SetStart(cx.no_gc(), &selected_node, selected_offset);
        let _ = range.SetEnd(cx.no_gc(), &selected_node, selected_offset);
        selection.AddRange(cx.no_gc(), &range);
    }
}
