/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use cssparser::match_ignore_ascii_case;
use js::context::JSContext;
use script_bindings::inheritance::Castable;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::RangeBinding::RangeMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::comment::Comment;
use crate::dom::document::Document;
use crate::dom::event::Event;
use crate::dom::event::inputevent::InputEvent;
use crate::dom::execcommand::basecommand::CommandName;
use crate::dom::execcommand::commands::fontsize::maybe_normalize_pixels;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::node::Node;
use crate::dom::processinginstruction::ProcessingInstruction;
use crate::dom::range::Range;
use crate::dom::selection::Selection;

/// <https://w3c.github.io/editing/docs/execCommand/#miscellaneous-commands>
fn is_command_listed_in_miscellaneous_section(command_name: CommandName) -> bool {
    matches!(
        command_name,
        CommandName::DefaultParagraphSeparator
            | CommandName::Redo
            | CommandName::SelectAll
            | CommandName::StyleWithCss
            | CommandName::Undo
            | CommandName::Usecss
    )
}

fn bump_selection_out_of_invalid_node(cx: &mut JSContext, selection: &Selection) -> Result<(), ()> {
    // Note: Here we make sure that if the selection range starts or ends inside of an HTML
    //       comment or PI, we get it out of there before trying to edit things. Trying to
    //       perform text editing inside of these nodes doesn't make any sense anyways and
    //       some commands aren't prepared to handle that. Picking the boundary point right
    //       before the problematic node is vaguely consistent with other browsers.
    let active_range = selection
        .active_range(cx)
        .expect("Must always have an active range");
    if let start_container = active_range.start_container() &&
        (start_container.is::<Comment>() || start_container.is::<ProcessingInstruction>())
    {
        let Some(parent) = start_container.GetParentNode() else {
            return Err(());
        };
        let _ = active_range.SetStart(cx.no_gc(), &parent, start_container.index());
    }
    if let end_container = active_range.end_container() &&
        (end_container.is::<Comment>() || end_container.is::<ProcessingInstruction>())
    {
        let Some(parent) = end_container.GetParentNode() else {
            return Err(());
        };
        let _ = active_range.SetEnd(cx.no_gc(), &parent, end_container.index());
    }
    Ok(())
}

/// <https://w3c.github.io/editing/docs/execCommand/#dfn-map-an-edit-command-to-input-type-value>
fn mapped_value_of_command(command: CommandName) -> DOMString {
    match command {
        CommandName::BackColor => "formatBackColor",
        CommandName::Bold => "formatBold",
        CommandName::CreateLink => "insertLink",
        CommandName::Cut => "deleteByCut",
        CommandName::Delete => "deleteContentBackward",
        CommandName::FontName => "formatFontName",
        CommandName::ForeColor => "formatFontColor",
        CommandName::ForwardDelete => "deleteContentForward",
        CommandName::Indent => "formatIndent",
        CommandName::InsertHorizontalRule => "insertHorizontalRule",
        CommandName::InsertLineBreak => "insertLineBreak",
        CommandName::InsertOrderedList => "insertOrderedList",
        CommandName::InsertParagraph => "insertParagraph",
        CommandName::InsertText => "insertText",
        CommandName::InsertUnorderedList => "insertUnorderedList",
        CommandName::JustifyCenter => "formatJustifyCenter",
        CommandName::JustifyFull => "formatJustifyFull",
        CommandName::JustifyLeft => "formatJustifyLeft",
        CommandName::JustifyRight => "formatJustifyRight",
        CommandName::Outdent => "formatOutdent",
        CommandName::Paste => "insertFromPaste",
        CommandName::Redo => "historyRedo",
        CommandName::Strikethrough => "formatStrikeThrough",
        CommandName::Superscript => "formatSuperscript",
        CommandName::Undo => "historyUndo",
        _ => "",
    }
    .into()
}

/// What asked for an editing command. It decides which input events the command fires.
pub(crate) enum EditingCommandSource {
    /// `document.execCommand()`: the events the execCommand specification defines.
    Script,
    /// The user editing the focused editing host, such as by typing or pressing Backspace: the
    /// `beforeinput` and `input` events the Input Events specification defines for user input,
    /// whose `data` is the text the user inserted.
    User { data: Option<DOMString> },
    /// Native IME preview replacement. Composition input is not cancelable and is composing.
    Composition {
        data: DOMString,
        /// Private preview range, updated before page input listeners can move the caret.
        range: DomRoot<Range>,
        epoch: u64,
    },
}

impl Document {
    /// Whether the user's input may edit `editing_host`. A selection left in an editing host
    /// after focus moved to another element must not take keystrokes meant for that element.
    /// Without a focused element, as when the document element is the editing host, the input
    /// goes to the selection; a focused editing host also takes input for those nested in it.
    pub(crate) fn focus_allows_user_editing_of(&self, editing_host: &Node) -> bool {
        let focused_area = self.focus_handler().focused_area();
        let Some(focused_element) = focused_area.element() else {
            return true;
        };
        focused_element
            .upcast::<Node>()
            .editing_host_of()
            .is_some_and(|focused_editing_host| {
                focused_editing_host.is_inclusive_ancestor_of(editing_host)
            })
    }
}

impl Node {
    fn is_in_plaintext_only_state(&self) -> bool {
        self.downcast::<HTMLElement>()
            .is_some_and(|el| el.ContentEditable().str() == "plaintext-only")
    }
}

impl Document {
    /// <https://w3c.github.io/editing/docs/execCommand/#enabled>
    fn selection_if_command_is_enabled(
        &self,
        cx: &mut JSContext,
        command_name: CommandName,
    ) -> Option<DomRoot<Selection>> {
        let selection = self.GetSelection(cx)?;
        // > Among commands defined in this specification, those listed in Miscellaneous commands are always enabled,
        // > except for the cut command and the paste command.
        //
        // Note: cut and paste are listed in the "clipboard commands" section, not the miscellaneous section
        if is_command_listed_in_miscellaneous_section(command_name) {
            return Some(selection);
        }
        // > The other commands defined here are enabled if the active range is not null,
        let range = selection.active_range(cx)?;
        // > its start node is either editable or an editing host,
        let start_container_editing_host = range.start_container().editing_host_of()?;
        // > the editing host of its start node is not an EditContext editing host,
        // TODO
        // > its end node is either editable or an editing host,
        let end_container_editing_host = range.end_container().editing_host_of()?;
        // > the editing host of its end node is not an EditContext editing host,
        // TODO
        // > and there is some editing host that is an inclusive ancestor of both its start node and its end node.
        // TODO

        if !command_name.is_enabled(cx, &range, &start_container_editing_host) {
            return None;
        }

        // Some commands are only enabled if the editing host is *not* in plaintext-only state.
        if !command_name.is_enabled_in_plaintext_only_state() &&
            (start_container_editing_host.is_in_plaintext_only_state() ||
                end_container_editing_host.is_in_plaintext_only_state())
        {
            None
        } else {
            Some(selection)
        }
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#supported>
    fn command_if_command_is_supported(&self, command_id: &DOMString) -> Option<CommandName> {
        // https://w3c.github.io/editing/docs/execCommand/#methods-to-query-and-execute-commands
        // > All of these methods must treat their command argument ASCII case-insensitively.
        Some(match_ignore_ascii_case! { &command_id.str(),
            "backcolor" => CommandName::BackColor,
            "bold" => CommandName::Bold,
            "createlink" => CommandName::CreateLink,
            "delete" => CommandName::Delete,
            "defaultparagraphseparator" => CommandName::DefaultParagraphSeparator,
            "fontname" => CommandName::FontName,
            "fontsize" => CommandName::FontSize,
            "forecolor" => CommandName::ForeColor,
            "forwarddelete" => CommandName::ForwardDelete,
            "hilitecolor" => CommandName::HiliteColor,
            "indent" => CommandName::Indent,
            "inserthorizontalrule" => CommandName::InsertHorizontalRule,
            "insertimage" => CommandName::InsertImage,
            "insertlinebreak" => CommandName::InsertLineBreak,
            "insertparagraph" => CommandName::InsertParagraph,
            "inserttext" => CommandName::InsertText,
            "italic" => CommandName::Italic,
            "removeformat" => CommandName::RemoveFormat,
            "strikethrough" => CommandName::Strikethrough,
            "stylewithcss" => CommandName::StyleWithCss,
            "subscript" => CommandName::Subscript,
            "superscript" => CommandName::Superscript,
            "underline" => CommandName::Underline,
            "unlink" => CommandName::Unlink,
            _ => return None,
        })
    }
}

pub(crate) trait DocumentExecCommandSupport {
    fn is_command_supported(&self, command_id: DOMString) -> bool;
    fn is_command_indeterminate(&self, cx: &mut JSContext, command_id: DOMString) -> bool;
    fn command_state_for_command(&self, cx: &mut JSContext, command_id: DOMString) -> bool;
    fn command_value_for_command(&self, cx: &mut JSContext, command_id: DOMString) -> DOMString;
    fn check_support_and_enabled(
        &self,
        cx: &mut JSContext,
        command_id: &DOMString,
    ) -> Option<(CommandName, DomRoot<Selection>)>;
    fn exec_command_for_command_id(
        &self,
        cx: &mut JSContext,
        command_id: DOMString,
        value: DOMString,
        source: EditingCommandSource,
    ) -> bool;
}

impl DocumentExecCommandSupport for Document {
    /// <https://w3c.github.io/editing/docs/execCommand/#querycommandsupported()>
    fn is_command_supported(&self, command_id: DOMString) -> bool {
        self.command_if_command_is_supported(&command_id).is_some()
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#querycommandindeterm()>
    fn is_command_indeterminate(&self, cx: &mut JSContext, command_id: DOMString) -> bool {
        // Step 1. If command is not supported or has no indeterminacy, return false.
        // Step 2. Return true if command is indeterminate, otherwise false.
        self.command_if_command_is_supported(&command_id)
            .is_some_and(|command| command.is_indeterminate(cx, self))
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#querycommandstate()>
    fn command_state_for_command(&self, cx: &mut JSContext, command_id: DOMString) -> bool {
        // Step 1. If command is not supported or has no state, return false.
        let Some(command) = self.command_if_command_is_supported(&command_id) else {
            return false;
        };
        let Some(state) = command.current_state(cx, self) else {
            return false;
        };
        // Step 2. If the state override for command is set, return it.
        // Step 3. Return true if command's state is true, otherwise false.
        self.state_override(&command).unwrap_or(state)
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#querycommandvalue()>
    fn command_value_for_command(&self, cx: &mut JSContext, command_id: DOMString) -> DOMString {
        // Step 1. If command is not supported or has no value, return the empty string.
        let Some(command) = self.command_if_command_is_supported(&command_id) else {
            return DOMString::new();
        };
        let Some(value) = command.current_value(cx, self) else {
            return DOMString::new();
        };
        // Step 3. If the value override for command is set, return it.
        self.value_override(&command)
            .map(|value_override| {
                // Step 2. If command is "fontSize" and its value override is set,
                // convert the value override to an integer number of pixels and return the legacy font size for the result.
                if command == CommandName::FontSize {
                    maybe_normalize_pixels(&value_override, self).unwrap_or(value_override)
                } else {
                    value_override
                }
            })
            // Step 4. Return command's value.
            .unwrap_or(value)
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#querycommandenabled()>
    fn check_support_and_enabled(
        &self,
        cx: &mut JSContext,
        command_id: &DOMString,
    ) -> Option<(CommandName, DomRoot<Selection>)> {
        // Step 2. Return true if command is both supported and enabled, false otherwise.
        let command = self.command_if_command_is_supported(command_id)?;
        let selection = self.selection_if_command_is_enabled(cx, command)?;
        Some((command, selection))
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#execcommand()>
    ///
    /// Also runs the commands for user editing in an editing host, which differ only in the
    /// input events they fire and in requiring the editing host to have focus (see
    /// [`EditingCommandSource`]).
    fn exec_command_for_command_id(
        &self,
        cx: &mut JSContext,
        command_id: DOMString,
        value: DOMString,
        source: EditingCommandSource,
    ) -> bool {
        let is_user_input = !matches!(source, EditingCommandSource::Script);
        let is_composing = matches!(source, EditingCommandSource::Composition { .. });
        let (user_data, composition) = match source {
            EditingCommandSource::Script => (None, None),
            EditingCommandSource::User { data } => (data, None),
            EditingCommandSource::Composition { data, range, epoch } => {
                (Some(data), Some((range, epoch)))
            },
        };
        let expected_range = composition.as_ref().map(|(range, _)| {
            (
                range.start_container(),
                range.start_offset(),
                range.end_container(),
                range.end_offset(),
            )
        });
        let window = self.window();
        // Step 3. If command is not supported or not enabled, return false.
        let Some((command, mut selection)) = self.check_support_and_enabled(cx, &command_id) else {
            return false;
        };
        // Step 4. If command is not in the Miscellaneous commands section:
        let affected_editing_host = if !is_command_listed_in_miscellaneous_section(command) {
            // Step 4.1. Let affected editing host be the editing host that is an inclusive ancestor
            // of the active range's start node and end node, and is not the ancestor of any editing host
            // that is an inclusive ancestor of the active range's start node and end node.
            let Some(affected_editing_host) = selection
                .active_range(cx)
                .expect("Must always have an active range")
                .CommonAncestorContainer()
                .editing_host_of()
            else {
                return false;
            };

            if is_user_input && !self.focus_allows_user_editing_of(&affected_editing_host) {
                return false;
            }

            // Step 4.2. Fire an event named "beforeinput" at affected editing host using InputEvent,
            // with its bubbles and cancelable attributes initialized to true, and its data attribute initialized to null
            //
            // User input instead fires the beforeinput event of the Input Events specification:
            // composed, with the inputType of the command and the text the user inserted.
            // <https://w3c.github.io/input-events/#event-type-beforeinput>
            let event = InputEvent::new(
                cx,
                window,
                None,
                atom!("beforeinput"),
                true,
                !is_composing,
                Some(window),
                0,
                user_data.clone(),
                is_composing,
                if is_composing {
                    DOMString::from_static("insertCompositionText")
                } else if is_user_input {
                    mapped_value_of_command(command)
                } else {
                    DOMString::new()
                },
            );
            let event = event.upcast::<Event>();
            event.set_composed(is_user_input);
            // Step 4.3. If the value returned by the previous step is false, return false.
            //
            // A page that cancels the beforeinput event of user input has handled that input.
            if !event.fire(cx, affected_editing_host.upcast()) {
                return is_user_input;
            }

            // Step 4.4. If command is not enabled, return false.
            let Some(new_selection) = self.selection_if_command_is_enabled(cx, command) else {
                return false;
            };
            selection = new_selection;

            // Step 4.5. Let affected editing host be the editing host that is an inclusive ancestor
            // of the active range's start node and end node, and is not the ancestor of any editing host
            // that is an inclusive ancestor of the active range's start node and end node.
            let host = selection
                .active_range(cx)
                .expect("Must always have an active range")
                .CommonAncestorContainer()
                .editing_host_of();
            if is_user_input
                && host
                    .as_ref()
                    .is_none_or(|host| !self.focus_allows_user_editing_of(host))
            {
                return false;
            }
            host
        } else {
            None
        };

        if let Some((_, epoch)) = composition.as_ref() {
            let Some(range) = selection.active_range(cx) else {
                return false;
            };
            let current = (
                range.start_container(),
                range.start_offset(),
                range.end_container(),
                range.end_offset(),
            );
            if !self.event_handler().composition_is_current(*epoch)
                || Some(current) != expected_range
            {
                // Non-cancelable beforeinput is still reentrant. Do not follow a listener's
                // new caret, DOM replacement, or blur/refocus cycle with native preview text.
                return false;
            }
        }
        if affected_editing_host.is_some()
            && bump_selection_out_of_invalid_node(cx, &selection).is_err()
        {
            return false;
        }

        let composition_start = composition.as_ref().map(|(range, _)| {
            let start = range.CloneRange(cx);
            start.Collapse(cx.no_gc(), true);
            start
        });
        // Step 5. Take the action for command, passing value to the instructions as an argument.
        let result = command.execute(cx, self, &selection, value);
        // Step 6. If the previous step returned false, return false.
        if !result {
            return false;
        }
        if let Some(((range, epoch), start)) = composition.as_ref().zip(composition_start.as_ref())
        {
            let Some(caret) = selection.active_range(cx) else {
                return false;
            };
            if !caret.collapsed()
                || caret.end_container().editing_host_of()
                    != start.start_container().editing_host_of()
            {
                return false;
            }
            // Capture the actual inserted interval before input fires. Reading the selection
            // afterward could include unrelated text selected by a page listener.
            start.set_end(cx.no_gc(), &caret.end_container(), caret.end_offset());
            range.set_start(cx.no_gc(), &start.start_container(), start.start_offset());
            range.set_end(cx.no_gc(), &start.end_container(), start.end_offset());
            self.event_handler().composition_preview_updated(
                *epoch,
                user_data
                    .as_ref()
                    .expect("composition carries text")
                    .clone(),
            );
        }
        // Step 7. If the action modified DOM tree, then fire an event named "input" at affected editing
        // host using InputEvent, with its isTrusted and bubbles attributes initialized to true,
        // inputType attribute initialized to the mapped value of command, and its data attribute initialized to null.
        if let Some(affected_editing_host) = affected_editing_host {
            let event = InputEvent::new(
                cx,
                window,
                None,
                atom!("input"),
                true,
                false,
                Some(window),
                0,
                user_data,
                is_composing,
                if is_composing {
                    DOMString::from_static("insertCompositionText")
                } else {
                    mapped_value_of_command(command)
                },
            );
            let event = event.upcast::<Event>();
            event.set_trusted(true);
            event.set_composed(is_user_input);
            event.fire(cx, affected_editing_host.upcast());
        }

        // Step 8. Return true.
        true
    }
}
