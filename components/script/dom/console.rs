/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::convert::TryFrom;
use std::ffi::CStr;
use std::ptr::{self, NonNull};

use devtools_traits::{
    ConsoleLogLevel, ConsoleMessage, ConsoleMessageFields, DebuggerValue, FunctionPreview,
    ObjectPreview, PropertyDescriptor as DevtoolsPropertyDescriptor, ScriptToDevtoolsControlMsg,
    StackFrame, get_time_stamp,
};
use embedder_traits::EmbedderMsg;
use js::context::JSContext;
use js::conversions::jsstr_to_string;
use js::jsapi::{
    self, ESClass, IsWindowProxy, JS_GetFunctionArity, PropertyDescriptor, SavedFrameSelfHosted,
};
use js::jsval::UndefinedValue;
use js::realm::CurrentRealm;
use js::rust::wrappers2::{
    GetArrayLength, GetBuiltinClass, GetPropertyKeys, GetRegExpSource, GetSavedFrameColumn,
    GetSavedFrameFunctionDisplayName, GetSavedFrameLine, GetSavedFrameSource,
    JS_ClearPendingException, JS_GetElement, JS_GetFunctionDisplayId, JS_GetFunctionId,
    JS_GetOwnPropertyDescriptor, JS_GetOwnPropertyDescriptorById, JS_GetPrototype, JS_IdToValue,
    JS_ValueToFunction, JS_ValueToSource, MapEntries, MapSize,
};
use js::rust::{
    CapturedJSStack, HandleObject, HandleValue, IdVector, ToNumber, describe_scripted_caller,
    for_of, get_object_class,
};
use script_bindings::conversions::{get_dom_class, is_dom_proxy};

use crate::dom::bindings::codegen::Bindings::ConsoleBinding::consoleMethods;
use crate::dom::bindings::codegen::PrototypeList;
use crate::dom::bindings::error::report_pending_exception;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::workerglobalscope::WorkerGlobalScope;

/// The maximum object depth logged by console methods.
const MAX_LOG_DEPTH: usize = 10;
/// The maximum elements in an object logged by console methods.
const MAX_LOG_CHILDREN: usize = 15;

/// <https://developer.mozilla.org/en-US/docs/Web/API/Console>
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct Console;

impl Console {
    fn build_message(
        cx: &mut JSContext,
        level: ConsoleLogLevel,
        arguments: Vec<DebuggerValue>,
        stacktrace: Option<Vec<StackFrame>>,
    ) -> ConsoleMessage {
        let caller = describe_scripted_caller(cx).unwrap_or_default();

        ConsoleMessage {
            fields: ConsoleMessageFields {
                level,
                filename: caller.filename,
                line_number: caller.line,
                column_number: caller.col,
                time_stamp: get_time_stamp(),
            },
            arguments,
            stacktrace,
        }
    }

    /// Helper to send a message that only consists of a single string
    fn send_string_message(
        cx: &mut JSContext,
        global: &GlobalScope,
        level: ConsoleLogLevel,
        message: String,
    ) {
        let prefix = global.current_group_label().unwrap_or_default();
        let formatted_message = format!("{prefix}{message}");

        Self::send_to_embedder(global, level.clone(), formatted_message);

        let console_message =
            Self::build_message(cx, level, vec![DebuggerValue::StringValue(message)], None);

        Self::send_to_devtools(global, console_message);
    }

    fn method(
        cx: &mut JSContext,
        global: &GlobalScope,
        level: ConsoleLogLevel,
        messages: Vec<HandleValue>,
        include_stacktrace: IncludeStackTrace,
    ) {
        // Devtools get arguments in structured form. Build it only for an attached client:
        // unlike the embedder text, it walks proxies and Map iterators, which run page script.
        let devtools_attached = global.devtools_chan().is_some();

        // If the first argument is a string, apply sprintf-style substitutions per the
        // WHATWG Console spec formatter. The result is a single formatted string followed
        // by any arguments that were not consumed by a substitution specifier.
        let (arguments, embedder_msg) = if !messages.is_empty() && messages[0].is_string() {
            let (formatted, consumed) = apply_sprintf_substitutions(cx, &messages);
            let remaining = &messages[consumed..];

            let mut arguments: Vec<DebuggerValue> =
                vec![DebuggerValue::StringValue(formatted.clone())];
            if devtools_attached {
                for msg in remaining {
                    arguments.push(console_argument_from_handle_value(
                        cx,
                        *msg,
                        &mut Vec::new(),
                    ));
                }
            }

            let embedder_msg = if remaining.is_empty() {
                formatted
            } else {
                format!("{formatted} {}", stringify_handle_values(cx, remaining))
            };

            (arguments, embedder_msg.into())
        } else {
            let arguments = if devtools_attached {
                messages
                    .iter()
                    .map(|msg| console_argument_from_handle_value(cx, *msg, &mut Vec::new()))
                    .collect()
            } else {
                Vec::new()
            };
            (arguments, stringify_handle_values(cx, &messages))
        };

        if devtools_attached {
            let stacktrace =
                (include_stacktrace == IncludeStackTrace::Yes).then_some(get_js_stack(cx));
            let console_message = Self::build_message(cx, level.clone(), arguments, stacktrace);
            Console::send_to_devtools(global, console_message);
        }

        let prefix = global.current_group_label().unwrap_or_default();
        let formatted_message = format!("{prefix}{embedder_msg}");

        Self::send_to_embedder(global, level, formatted_message);
    }

    fn send_to_devtools(global: &GlobalScope, message: ConsoleMessage) {
        if let Some(chan) = global.devtools_chan() {
            let worker_id = global
                .downcast::<WorkerGlobalScope>()
                .map(|worker| worker.worker_id());
            let devtools_message =
                ScriptToDevtoolsControlMsg::ConsoleAPI(global.pipeline_id(), message, worker_id);
            chan.send(devtools_message).unwrap();
        }
    }

    fn send_to_embedder(global: &GlobalScope, level: ConsoleLogLevel, message: String) {
        global.send_to_embedder(EmbedderMsg::ShowConsoleApiMessage(
            global.webview_id(),
            level,
            message,
        ));
    }

    // Directly logs a string message, without processing the message
    pub(crate) fn internal_warn(cx: &mut JSContext, global: &GlobalScope, message: String) {
        Console::send_string_message(cx, global, ConsoleLogLevel::Warn, message);
    }

    pub(crate) fn internal_error(cx: &mut JSContext, global: &GlobalScope, message: String) {
        Console::send_string_message(cx, global, ConsoleLogLevel::Error, message);
    }
}

#[expect(unsafe_code)]
fn handle_value_to_string(cx: &mut JSContext, value: HandleValue) -> DOMString {
    match std::ptr::NonNull::new(unsafe { JS_ValueToSource(cx, value) }) {
        Some(js_str) => unsafe { jsstr_to_string(cx, js_str) }.into(),
        None => DOMString::from_static("<error converting value to string>"),
    }
}

fn console_argument_from_handle_value(
    cx: &mut JSContext,
    handle_value: HandleValue,
    seen: &mut Vec<u64>,
) -> DebuggerValue {
    #[expect(unsafe_code)]
    fn inner(
        cx: &mut JSContext,
        handle_value: HandleValue,
        seen: &mut Vec<u64>,
    ) -> Result<DebuggerValue, ()> {
        if handle_value.is_string() {
            let js_string = ptr::NonNull::new(handle_value.to_string()).unwrap();
            let dom_string = unsafe { jsstr_to_string(cx, js_string) };
            return Ok(DebuggerValue::StringValue(dom_string));
        }

        if handle_value.is_number() {
            let number = handle_value.to_number();
            return Ok(DebuggerValue::NumberValue(number));
        }

        if handle_value.is_boolean() {
            let boolean = handle_value.to_boolean();
            return Ok(DebuggerValue::BooleanValue(boolean));
        }

        if handle_value.is_object() {
            // JS objects can create circular reference, and we want to avoid recursing infinitely
            if seen.contains(&handle_value.asBits_) {
                // FIXME: Handle this properly
                return Ok(DebuggerValue::StringValue("[circular]".into()));
            }

            seen.push(handle_value.asBits_);
            let console_object = console_object_from_handle_value(cx, handle_value, seen);
            let js_value = seen.pop();
            debug_assert_eq!(js_value, Some(handle_value.asBits_));

            if let Some((class, preview)) = console_object {
                return Ok(DebuggerValue::ObjectValue {
                    actor: None,
                    class,
                    own_property_length: preview.own_properties_length,
                    preview: Some(Box::new(preview)),
                });
            }

            return Err(());
        }

        // FIXME: Handle more complex argument types here
        let stringified_value = stringify_handle_value(cx, handle_value);

        Ok(DebuggerValue::StringValue(stringified_value.into()))
    }

    match inner(cx, handle_value, seen) {
        Ok(arg) => arg,
        Err(()) => {
            report_pending_exception(&mut CurrentRealm::assert(cx));
            DebuggerValue::StringValue("<error>".into())
        },
    }
}

fn accessor_value_from_property_descriptor(descriptor: &PropertyDescriptor) -> DebuggerValue {
    // https://console.spec.whatwg.org/#printer
    // Objects with either generic JavaScript object formatting or optimally useful formatting applied.
    DebuggerValue::StringValue(accessor_kind(descriptor).into())
}

/// Names an accessor property's kind, e.g. "Getter", without calling it.
fn accessor_kind(descriptor: &PropertyDescriptor) -> &'static str {
    match (
        descriptor.hasGetter_() && !descriptor.getter_.is_null(),
        descriptor.hasSetter_() && !descriptor.setter_.is_null(),
    ) {
        (true, true) => "Getter/Setter",
        (true, false) => "Getter",
        (false, true) => "Setter",
        (false, false) => "undefined",
    }
}

#[expect(unsafe_code)]
fn console_map_object_from_handle_value(
    cx: &mut JSContext,
    handle_object: HandleObject,
    seen: &mut Vec<u64>,
) -> Option<(String, ObjectPreview)> {
    rooted!(&in(cx) let mut iterator = UndefinedValue());
    if !unsafe { MapEntries(cx, handle_object, iterator.handle_mut()) } {
        return None;
    }

    let mut entries = Vec::new();
    for_of(cx, iterator.handle(), |cx, entry| {
        if !entry.is_object() {
            return Err(().into());
        }

        rooted!(&in(cx) let entry_object = entry.to_object());
        rooted!(&in(cx) let mut key = UndefinedValue());
        rooted!(&in(cx) let mut value = UndefinedValue());

        // Each map entry is a [key, value] pair.
        if !unsafe { JS_GetElement(cx, entry_object.handle(), 0, key.handle_mut()) } ||
            !unsafe { JS_GetElement(cx, entry_object.handle(), 1, value.handle_mut()) }
        {
            return Err(().into());
        }

        entries.push((
            console_argument_from_handle_value(cx, key.handle(), seen),
            console_argument_from_handle_value(cx, value.handle(), seen),
        ));

        Ok(std::ops::ControlFlow::Continue(()))
    })
    .ok()?;

    Some((
        "Map".into(),
        ObjectPreview {
            kind: "MapLike".into(),
            size: Some(unsafe { MapSize(cx, handle_object) }),
            entries: Some(entries),
            own_properties_length: Some(0),
            own_properties: None,
            function: None,
            array_length: None,
            items: None,
        },
    ))
}

#[expect(unsafe_code)]
fn console_object_from_handle_value(
    cx: &mut JSContext,
    handle_value: HandleValue,
    seen: &mut Vec<u64>,
) -> Option<(String, ObjectPreview)> {
    rooted!(&in(cx) let object = handle_value.to_object());
    let mut object_class = ESClass::Other;
    if !unsafe { GetBuiltinClass(cx, object.handle(), &mut object_class as *mut _) } {
        return None;
    }
    if object_class != ESClass::Object &&
        object_class != ESClass::Array &&
        object_class != ESClass::Map &&
        object_class != ESClass::Function
    {
        return None;
    }

    if object_class == ESClass::Map {
        return console_map_object_from_handle_value(cx, object.handle(), seen);
    }

    let mut own_properties = Vec::new();
    let mut items: Vec<(i32, DebuggerValue)> = Vec::new();
    let mut ids = IdVector::new(cx);
    // https://console.spec.whatwg.org/#printer
    // Objects with either generic JavaScript object formatting or optimally useful formatting applied.
    if !unsafe {
        GetPropertyKeys(
            cx,
            object.handle(),
            jsapi::JSITER_OWNONLY | jsapi::JSITER_SYMBOLS | jsapi::JSITER_HIDDEN,
            ids.handle_mut(),
        )
    } {
        return None;
    }

    for id in ids.iter() {
        rooted!(&in(cx) let id = *id);
        rooted!(&in(cx) let mut descriptor = PropertyDescriptor::default());

        let mut is_none = false;
        if !unsafe {
            JS_GetOwnPropertyDescriptorById(
                cx,
                object.handle(),
                id.handle(),
                descriptor.handle_mut(),
                &mut is_none,
            )
        } {
            return None;
        }
        if is_none {
            continue;
        }

        // https://console.spec.whatwg.org/#printer
        // Objects with either generic JavaScript object formatting or optimally useful formatting applied.
        let is_accessor = (descriptor.hasGetter_() && !descriptor.getter_.is_null()) ||
            (descriptor.hasSetter_() && !descriptor.setter_.is_null());
        let value = if is_accessor {
            accessor_value_from_property_descriptor(&descriptor)
        } else {
            rooted!(&in(cx) let property = descriptor.value_);
            console_argument_from_handle_value(cx, property.handle(), seen)
        };

        if object_class == ESClass::Array && id.is_int() {
            let index = id.to_int();
            items.push((index, value));
            continue;
        }

        let key = if id.is_string() {
            rooted!(&in(cx) let mut key_value = UndefinedValue());
            if !unsafe { JS_IdToValue(cx, id.handle().get(), key_value.handle_mut()) } {
                continue;
            }
            rooted!(&in(cx) let js_string = key_value.to_string());
            let Some(js_string) = NonNull::new(js_string.get()) else {
                continue;
            };
            unsafe { jsstr_to_string(cx, js_string) }
        } else if id.is_symbol() || id.is_int() {
            rooted!(&in(cx) let mut key_value = UndefinedValue());
            if !unsafe { JS_IdToValue(cx, id.handle().get(), key_value.handle_mut()) } {
                continue;
            }
            handle_value_to_string(cx, key_value.handle()).to_string()
        } else {
            continue;
        };

        own_properties.push(DevtoolsPropertyDescriptor {
            name: key,
            value,
            configurable: descriptor.hasConfigurable_() && descriptor.configurable_(),
            enumerable: descriptor.hasEnumerable_() && descriptor.enumerable_(),
            writable: !is_accessor && descriptor.hasWritable_() && descriptor.writable_(),
            is_accessor,
        });
    }

    let (class, kind, function, array_length, items) = match object_class {
        ESClass::Array => {
            let mut len = 0u32;
            if !unsafe { GetArrayLength(cx, object.handle(), &mut len) } {
                return None;
            }
            items.sort_by_key(|(index, _)| *index);
            let ordered: Vec<DebuggerValue> = items.into_iter().map(|(_, value)| value).collect();
            (
                "Array".into(),
                "ArrayLike".into(),
                None,
                Some(len),
                Some(ordered),
            )
        },
        ESClass::Function => {
            rooted!(&in(cx) let fun = unsafe { JS_ValueToFunction(cx, handle_value) });
            rooted!(&in(cx) let mut name = std::ptr::null_mut::<jsapi::JSString>());
            rooted!(&in(cx) let mut display_name = std::ptr::null_mut::<jsapi::JSString>());
            let arity;
            unsafe {
                JS_GetFunctionId(cx, fun.handle(), name.handle_mut());
                JS_GetFunctionDisplayId(cx, fun.handle(), display_name.handle_mut());
                arity = JS_GetFunctionArity(fun.get());
            }
            let name = ptr::NonNull::new(*name).map(|name| unsafe { jsstr_to_string(cx, name) });
            let display_name = ptr::NonNull::new(*display_name)
                .map(|display_name| unsafe { jsstr_to_string(cx, display_name) });

            // TODO: We should get the actual argument names from the function
            // It's not trivial since we can't access the debugger API here
            let parameter_names = (0..arity).map(|i| format!("<arg{i}>")).collect();

            let function = FunctionPreview {
                name,
                display_name,
                parameter_names,
                is_async: None,
                is_generator: None,
            };
            (
                "Function".into(),
                "Object".into(),
                Some(function),
                None,
                None,
            )
        },
        // TODO: Investigate if this class should be the object class
        _ => ("Object".into(), "Object".into(), None, None, None),
    };

    Some((
        class,
        ObjectPreview {
            kind,
            size: None,
            entries: None,
            own_properties_length: Some(own_properties.len() as u32),
            own_properties: Some(own_properties),
            function,
            array_length,
            items,
        },
    ))
}

/// Formats a console argument as text for the embedder without running page script.
///
/// Browsers format console arguments only for an attached devtools client, so a page cannot
/// observe its own logging. Calling a getter, proxy trap, `toString` or `toJSON` here would let
/// it: logging `{ get x() { … } }` would run the getter. Accessors therefore print as their
/// kind (`{"x": Getter}`), proxies as their class name, and other objects from internal state.
#[expect(unsafe_code)]
pub(crate) fn stringify_handle_value(cx: &mut JSContext, message: HandleValue) -> DOMString {
    if message.is_string() {
        let jsstr = std::ptr::NonNull::new(message.to_string()).unwrap();
        return unsafe { jsstr_to_string(cx, jsstr) }.into();
    }
    fn stringify_object_from_handle_value(
        cx: &mut JSContext,
        value: HandleValue,
        parents: Vec<u64>,
    ) -> DOMString {
        rooted!(&in(cx) let obj = value.to_object());
        if unsafe { IsWindowProxy(obj.get()) } {
            return DOMString::from_static("Window");
        }
        if is_foreign_proxy(obj.get()) {
            return DOMString::from_static("Proxy");
        }

        let mut object_class = ESClass::Other;
        if !unsafe { GetBuiltinClass(cx, obj.handle(), &mut object_class as *mut _) } {
            return invalid_value(cx);
        }
        match object_class {
            ESClass::Object | ESClass::Array => {},
            ESClass::Error => return stringify_error(cx, obj.handle()),
            ESClass::RegExp => return stringify_regexp(cx, obj.handle()),
            // SpiderMonkey converts these to source from internal slots alone, as long as realms
            // keep `toSource` lookups disabled (the default). Boxed BigInts are not among them:
            // their conversion enumerates and converts the properties.
            ESClass::Function |
            ESClass::Date |
            ESClass::Boolean |
            ESClass::Number |
            ESClass::String => return handle_value_to_string(cx, value),
            _ => {},
        }

        let mut ids = IdVector::new(cx);
        if !unsafe {
            GetPropertyKeys(
                cx,
                obj.handle(),
                jsapi::JSITER_OWNONLY | jsapi::JSITER_SYMBOLS,
                ids.handle_mut(),
            )
        } {
            return invalid_value(cx);
        }
        let truncate = ids.len() > MAX_LOG_CHILDREN;

        let mut explicit_keys = object_class != ESClass::Array;
        let mut props = Vec::with_capacity(ids.len());
        for id in ids.iter().take(MAX_LOG_CHILDREN) {
            rooted!(&in(cx) let id = *id);
            rooted!(&in(cx) let mut desc = PropertyDescriptor::default());

            let mut is_none = false;
            if !unsafe {
                JS_GetOwnPropertyDescriptorById(
                    cx,
                    obj.handle(),
                    id.handle(),
                    desc.handle_mut(),
                    &mut is_none,
                )
            } {
                return invalid_value(cx);
            }

            if !explicit_keys {
                if id.is_int() {
                    if let Ok(id_int) = usize::try_from(id.to_int()) {
                        explicit_keys = props.len() != id_int;
                    } else {
                        explicit_keys = false;
                    }
                } else {
                    explicit_keys = false;
                }
            }

            // Reading through an accessor would call page script, so name its kind instead.
            let value_string = if desc.hasGetter_() || desc.hasSetter_() {
                DOMString::from_static(accessor_kind(&desc))
            } else {
                rooted!(&in(cx) let property = desc.value_);
                stringify_inner(cx, property.handle(), parents.clone())
            };
            if explicit_keys {
                let key = if id.is_string() || id.is_symbol() || id.is_int() {
                    rooted!(&in(cx) let mut key_value = UndefinedValue());
                    if !unsafe { JS_IdToValue(cx, id.handle().get(), key_value.handle_mut()) } {
                        return invalid_value(cx);
                    }
                    handle_value_to_string(cx, key_value.handle())
                } else {
                    return invalid_value(cx);
                };
                props.push(format!("{}: {}", key, value_string,));
            } else {
                props.push(String::from(value_string));
            }
        }
        if truncate {
            props.push("…".to_string());
        }
        match object_class {
            ESClass::Array => DOMString::from(format!("[{}]", itertools::join(props, ", "))),
            ESClass::Object => DOMString::from(format!("{{{}}}", itertools::join(props, ", "))),
            _ => DOMString::from(format!(
                "{} {{{}}}",
                object_class_name(obj.get()),
                itertools::join(props, ", ")
            )),
        }
    }
    fn stringify_inner(cx: &mut JSContext, value: HandleValue, mut parents: Vec<u64>) -> DOMString {
        if parents.len() >= MAX_LOG_DEPTH {
            return DOMString::from_static("...");
        }
        let value_bits = value.asBits_;
        if parents.contains(&value_bits) {
            return DOMString::from_static("[circular]");
        }
        if value.is_undefined() {
            // This produces a better value than "(void 0)" from JS_ValueToSource.
            return DOMString::from_static("undefined");
        } else if !value.is_object() {
            return handle_value_to_string(cx, value);
        }
        parents.push(value_bits);
        stringify_object_from_handle_value(cx, value, parents)
    }
    stringify_inner(cx, message, Vec::new())
}

/// Whether `obj` is a proxy other than a DOM object, such as `new Proxy(…)` or a
/// cross-compartment wrapper. Inspecting one can run page-defined traps.
#[expect(unsafe_code)]
fn is_foreign_proxy(obj: *mut jsapi::JSObject) -> bool {
    let class = unsafe { get_object_class(obj) };
    let is_proxy = unsafe { (*class).flags } & js::JSCLASS_IS_PROXY != 0;
    is_proxy && !unsafe { is_dom_proxy(obj) }
}

/// The name of an object's class from engine data, e.g. "Map" or "HTMLDivElement".
#[expect(unsafe_code)]
fn object_class_name(obj: *mut jsapi::JSObject) -> String {
    if let Ok(dom_class) = unsafe { get_dom_class(obj) } {
        let interface = dom_class.interface_chain[dom_class.depth as usize];
        return PrototypeList::proto_id_to_name(interface as u16).to_owned();
    }
    let name = unsafe { (*get_object_class(obj)).name };
    if name.is_null() {
        return "Object".to_owned();
    }
    unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
}

/// Formats an error like `Error.prototype.toString`, e.g. "TypeError: x is null", reading
/// `name` and `message` only where they are data properties.
fn stringify_error(cx: &mut JSContext, error: HandleObject) -> DOMString {
    let name = data_property_string(cx, error, c"name").unwrap_or_else(|| "Error".to_owned());
    let message = data_property_string(cx, error, c"message").unwrap_or_default();
    if message.is_empty() {
        return name.into();
    }
    format!("{name}: {message}").into()
}

/// Formats a regular expression from its internal source, e.g. "/a+b/". Its `source` and
/// `flags` properties are getters a page can replace, and mozjs binds no internal accessor
/// for the flags, so they are left out.
#[expect(unsafe_code)]
fn stringify_regexp(cx: &mut JSContext, regexp: HandleObject) -> DOMString {
    rooted!(&in(cx) let source = unsafe { GetRegExpSource(cx, regexp) });
    let Some(source) = NonNull::new(source.get()) else {
        return invalid_value(cx);
    };
    format!("/{}/", unsafe { jsstr_to_string(cx, source) }).into()
}

/// The text for a value the formatter failed to read. Logging must not throw, so the
/// failure's pending exception is cleared.
#[expect(unsafe_code)]
fn invalid_value(cx: &mut JSContext) -> DOMString {
    unsafe { JS_ClearPendingException(cx) };
    DOMString::from_static("/* invalid */")
}

/// Looks up a string-valued data property on `object` or its prototypes without running page
/// script. Returns `None` once the lookup meets an accessor, a foreign proxy or another value.
#[expect(unsafe_code)]
fn data_property_string(cx: &mut JSContext, object: HandleObject, name: &CStr) -> Option<String> {
    rooted!(&in(cx) let mut current = object.get());
    while !current.is_null() && !is_foreign_proxy(current.get()) {
        rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
        let mut is_none = true;
        if !unsafe {
            JS_GetOwnPropertyDescriptor(
                cx,
                current.handle(),
                name.as_ptr(),
                desc.handle_mut(),
                &mut is_none,
            )
        } {
            unsafe { JS_ClearPendingException(cx) };
            return None;
        }
        if !is_none {
            if desc.hasGetter_() || desc.hasSetter_() || !desc.value_.is_string() {
                return None;
            }
            let string = NonNull::new(desc.value_.to_string())?;
            return Some(unsafe { jsstr_to_string(cx, string) });
        }

        rooted!(&in(cx) let mut prototype = ptr::null_mut::<jsapi::JSObject>());
        if !unsafe { JS_GetPrototype(cx, current.handle(), prototype.handle_mut()) } {
            unsafe { JS_ClearPendingException(cx) };
            return None;
        }
        current.set(prototype.get());
    }
    None
}

/// Apply sprintf-style substitutions to console format arguments per the WHATWG Console spec.
///
/// If the first argument is a string, it is treated as a format string where `%s`, `%d`, `%i`,
/// `%f`, `%o`, `%O`, and `%c` are replaced by subsequent arguments. Returns the formatted string
/// and the index of the first argument that was not consumed by a substitution.
///
/// <https://console.spec.whatwg.org/#formatter>
#[expect(unsafe_code)]
fn apply_sprintf_substitutions(cx: &mut JSContext, messages: &[HandleValue]) -> (String, usize) {
    debug_assert!(!messages.is_empty() && messages[0].is_string());

    let js_string = ptr::NonNull::new(messages[0].to_string()).unwrap();
    let format_string = unsafe { jsstr_to_string(cx, js_string) };

    let mut result = String::new();
    let mut arg_index = 1usize;
    let mut chars = format_string.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '%' {
            result.push(c);
            continue;
        }

        match chars.peek().copied() {
            Some('s') => {
                chars.next();
                if arg_index < messages.len() {
                    result.push_str(&stringify_handle_value(cx, messages[arg_index]).str());
                    arg_index += 1;
                } else {
                    result.push_str("%s");
                }
            },
            Some('d') | Some('i') => {
                let spec = chars.next().unwrap();
                if arg_index < messages.len() {
                    let num = substitution_number(cx, messages[arg_index]);
                    arg_index += 1;
                    format_integer_substitution(&mut result, num);
                } else {
                    result.push('%');
                    result.push(spec);
                }
            },
            Some('f') => {
                chars.next();
                if arg_index < messages.len() {
                    let num = substitution_number(cx, messages[arg_index]);
                    arg_index += 1;
                    format_float_substitution(&mut result, num);
                } else {
                    result.push_str("%f");
                }
            },
            Some('o') | Some('O') => {
                let spec = chars.next().unwrap();
                if arg_index < messages.len() {
                    result.push_str(&stringify_handle_value(cx, messages[arg_index]).str());
                    arg_index += 1;
                } else {
                    result.push('%');
                    result.push(spec);
                }
            },
            Some('c') => {
                chars.next();
                if arg_index < messages.len() {
                    arg_index += 1; // consume but ignore CSS styling
                }
            },
            Some('%') => {
                chars.next();
                result.push('%');
            },
            _ => {
                result.push('%');
            },
        }
    }

    (result, arg_index)
}

/// Converts a `%d`, `%i` or `%f` argument to a number. Objects become NaN unconverted, since
/// converting them would call page-defined `valueOf`, `toString` or `Symbol.toPrimitive`.
#[expect(unsafe_code)]
fn substitution_number(cx: &mut JSContext, value: HandleValue) -> Result<f64, ()> {
    if value.is_object() {
        return Err(());
    }
    let num = unsafe { ToNumber(cx, value) };
    if num.is_err() {
        unsafe { JS_ClearPendingException(cx) };
    }
    num
}

fn format_integer_substitution(result: &mut String, num: Result<f64, ()>) {
    match num {
        Ok(n) if n.is_nan() => result.push_str("NaN"),
        Ok(n) if n == f64::INFINITY => result.push_str("Infinity"),
        Ok(n) if n == f64::NEG_INFINITY => result.push_str("-Infinity"),
        Ok(n) => result.push_str(&(n.trunc() as i64).to_string()),
        Err(_) => result.push_str("NaN"),
    }
}

fn format_float_substitution(result: &mut String, num: Result<f64, ()>) {
    match num {
        Ok(n) if n.is_nan() => result.push_str("NaN"),
        Ok(n) if n == f64::INFINITY => result.push_str("Infinity"),
        Ok(n) if n == f64::NEG_INFINITY => result.push_str("-Infinity"),
        Ok(n) => result.push_str(&n.to_string()),
        Err(_) => result.push_str("NaN"),
    }
}

fn stringify_handle_values(cx: &mut JSContext, messages: &[HandleValue]) -> DOMString {
    DOMString::from(itertools::join(
        messages
            .iter()
            .copied()
            .map(|msg| stringify_handle_value(cx, msg)),
        " ",
    ))
}

#[derive(Debug, Eq, PartialEq)]
enum IncludeStackTrace {
    Yes,
    No,
}

impl consoleMethods<crate::DomTypeHolder> for Console {
    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/log>
    fn Log(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Log,
            messages,
            IncludeStackTrace::No,
        );
    }

    /// <https://console.spec.whatwg.org/#table>
    fn Table(
        cx: &mut JSContext,
        global: &GlobalScope,
        tabular_data: HandleValue,
        _properties: Option<Vec<DOMString>>,
    ) {
        // Servo builds no table; the specification allows logging the argument instead.
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Log,
            vec![tabular_data],
            IncludeStackTrace::No,
        );
    }

    /// <https://console.spec.whatwg.org/#dirxml>
    fn Dirxml(cx: &mut JSContext, global: &GlobalScope, data: Vec<HandleValue>) {
        // Servo has no DOM tree representation, so the data is logged as it is.
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Log,
            data,
            IncludeStackTrace::No,
        );
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/clear>
    fn Clear(global: &GlobalScope) {
        if let Some(chan) = global.devtools_chan() {
            let worker_id = global
                .downcast::<WorkerGlobalScope>()
                .map(|worker| worker.worker_id());
            let devtools_message =
                ScriptToDevtoolsControlMsg::ClearConsole(global.pipeline_id(), worker_id);
            if let Err(error) = chan.send(devtools_message) {
                log::warn!("Error sending clear message to devtools: {error:?}");
            }
        }
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console>
    fn Debug(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Debug,
            messages,
            IncludeStackTrace::No,
        );
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/info>
    fn Info(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Info,
            messages,
            IncludeStackTrace::No,
        );
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/warn>
    fn Warn(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Warn,
            messages,
            IncludeStackTrace::No,
        );
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/error>
    fn Error(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Error,
            messages,
            IncludeStackTrace::No,
        );
    }

    /// <https://console.spec.whatwg.org/#trace>
    fn Trace(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        Console::method(
            cx,
            global,
            ConsoleLogLevel::Trace,
            messages,
            IncludeStackTrace::Yes,
        );
    }

    /// <https://console.spec.whatwg.org/#dir>
    fn Dir(
        cx: &mut js::context::JSContext,
        global: &GlobalScope,
        item: HandleValue,
        _options: Option<*mut jsapi::JSObject>,
    ) {
        // Step 1. Let object be item with generic JavaScript object formatting applied.
        // Step 2. Perform Printer("dir", « object », options).
        // As in `method`, only an attached devtools client gets the structured form.
        if global.devtools_chan().is_some() {
            let argument = console_argument_from_handle_value(cx, item, &mut Vec::new());
            Console::send_to_devtools(
                global,
                Self::build_message(cx, ConsoleLogLevel::Dir, vec![argument], None),
            );
        }
        let prefix = global.current_group_label().unwrap_or_default();
        Self::send_to_embedder(
            global,
            ConsoleLogLevel::Dir,
            format!("{prefix}{}", stringify_handle_value(cx, item)),
        );
    }

    /// <https://developer.mozilla.org/en-US/docs/Web/API/Console/assert>
    fn Assert(
        cx: &mut JSContext,
        global: &GlobalScope,
        condition: bool,
        messages: Vec<HandleValue>,
    ) {
        if !condition {
            let message = format!(
                "Assertion failed: {}",
                stringify_handle_values(cx, &messages)
            );

            Console::send_string_message(cx, global, ConsoleLogLevel::Log, message);
        }
    }

    /// <https://console.spec.whatwg.org/#time>
    fn Time(cx: &mut JSContext, global: &GlobalScope, label: DOMString) {
        if let Ok(()) = global.time(label.clone()) {
            let message = format!("{label}: timer started");
            Console::send_string_message(cx, global, ConsoleLogLevel::Log, message);
        }
    }

    /// <https://console.spec.whatwg.org/#timelog>
    fn TimeLog(cx: &mut JSContext, global: &GlobalScope, label: DOMString, data: Vec<HandleValue>) {
        if let Ok(delta) = global.time_log(&label) {
            let message = format!("{label}: {delta}ms {}", stringify_handle_values(cx, &data));

            Console::send_string_message(cx, global, ConsoleLogLevel::Log, message);
        }
    }

    /// <https://console.spec.whatwg.org/#timeend>
    fn TimeEnd(cx: &mut JSContext, global: &GlobalScope, label: DOMString) {
        if let Ok(delta) = global.time_end(&label) {
            let message = format!("{label}: {delta}ms");

            Console::send_string_message(cx, global, ConsoleLogLevel::Log, message);
        }
    }

    /// <https://console.spec.whatwg.org/#group>
    fn Group(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        global.push_console_group(stringify_handle_values(cx, &messages));
    }

    /// <https://console.spec.whatwg.org/#groupcollapsed>
    fn GroupCollapsed(cx: &mut JSContext, global: &GlobalScope, messages: Vec<HandleValue>) {
        global.push_console_group(stringify_handle_values(cx, &messages));
    }

    /// <https://console.spec.whatwg.org/#groupend>
    fn GroupEnd(global: &GlobalScope) {
        global.pop_console_group();
    }

    /// <https://console.spec.whatwg.org/#count>
    fn Count(cx: &mut JSContext, global: &GlobalScope, label: DOMString) {
        let count = global.increment_console_count(&label);
        let message = format!("{label}: {count}");

        Console::send_string_message(cx, global, ConsoleLogLevel::Log, message);
    }

    /// <https://console.spec.whatwg.org/#countreset>
    fn CountReset(cx: &mut JSContext, global: &GlobalScope, label: DOMString) {
        if global.reset_console_count(&label).is_err() {
            Self::internal_warn(cx, global, format!("Counter “{label}” doesn’t exist."))
        }
    }
}

#[expect(unsafe_code)]
fn get_js_stack(cx: &mut JSContext) -> Vec<StackFrame> {
    const MAX_FRAME_COUNT: u32 = 128;

    let mut frames = vec![];
    rooted!(&in(cx) let mut handle =  ptr::null_mut());
    let captured_js_stack = unsafe { CapturedJSStack::new(cx, handle, Some(MAX_FRAME_COUNT)) };
    let Some(mut captured_js_stack) = captured_js_stack else {
        return frames;
    };

    captured_js_stack.for_each_stack_frame(|cx, frame| {
        rooted!(&in(cx) let mut result: *mut jsapi::JSString = ptr::null_mut());

        // Get function name
        unsafe {
            GetSavedFrameFunctionDisplayName(
                cx,
                ptr::null_mut(),
                frame,
                result.handle_mut(),
                SavedFrameSelfHosted::Include,
            );
        }
        let function_name = if let Some(nonnull_result) = ptr::NonNull::new(*result) {
            unsafe { jsstr_to_string(cx, nonnull_result) }
        } else {
            "<anonymous>".into()
        };

        // Get source file name
        result.set(ptr::null_mut());
        unsafe {
            GetSavedFrameSource(
                cx,
                ptr::null_mut(),
                frame,
                result.handle_mut(),
                SavedFrameSelfHosted::Include,
            );
        }
        let filename = if let Some(nonnull_result) = ptr::NonNull::new(*result) {
            unsafe { jsstr_to_string(cx, nonnull_result) }
        } else {
            "<anonymous>".into()
        };

        // get line/column number
        let mut line_number = 0;
        unsafe {
            GetSavedFrameLine(
                cx,
                ptr::null_mut(),
                frame,
                &mut line_number,
                SavedFrameSelfHosted::Include,
            );
        }

        let mut column_number = jsapi::JS::TaggedColumnNumberOneOrigin { value_: 0 };
        unsafe {
            GetSavedFrameColumn(
                cx,
                ptr::null_mut(),
                frame,
                &mut column_number,
                SavedFrameSelfHosted::Include,
            );
        }
        let frame = StackFrame {
            filename,
            function_name,
            line_number,
            column_number: column_number.value_,
        };

        frames.push(frame);
    });

    frames
}
