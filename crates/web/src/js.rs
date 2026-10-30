//! Thin helpers over `wasm-bindgen` for what the page does everywhere: looking up
//! elements, reading and writing plain JS objects, timers, event listeners and logging.

use js_sys::{Array, Function, Object, Reflect};
use wasm_bindgen::closure::WasmClosure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Document, EventTarget, Window};

/// The handle `setTimeout` returns, for `clearTimeout`.
pub type TimerHandle = i32;

/// The element with `id`, cast to `T`. The ids are fixed in `index.html`, so a missing one is a bug.
pub fn element_by_id<T: JsCast>(document: &Document, id: &str) -> T {
    let element = document.get_element_by_id(id).unwrap_or_else(|| panic!("#{id} missing"));
    element.unchecked_into()
}

/// A plain JS object literal with the given properties.
pub fn js_object(properties: &[(&str, JsValue)]) -> Object {
    let object = Object::new();
    for (name, value) in properties {
        set_property(&object, name, value);
    }
    object
}

pub fn get_property(target: &JsValue, name: &str) -> JsValue {
    Reflect::get(target, &name.into()).unwrap_or(JsValue::UNDEFINED)
}

pub fn set_property(target: &JsValue, name: &str, value: &JsValue) {
    let _ = Reflect::set(target, &name.into(), value);
}

/// Call `target.method(args...)` dynamically, for APIs web-sys lacks or Safari may not have.
pub fn call_method(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let function: Function = get_property(target, method).dyn_into()?;
    let args: Array = args.iter().collect();
    Reflect::apply(&function, target, &args)
}

/// A readable message for a JS error: its `message`, else its `name`.
pub fn error_message(error: &JsValue) -> String {
    let message = get_property(error, "message").as_string().unwrap_or_default();
    if !message.is_empty() {
        return message;
    }
    get_property(error, "name").as_string().unwrap_or_else(|| format!("{error:?}"))
}

/// Milliseconds since the page loaded.
pub fn now_ms() -> f64 {
    let performance = web_sys::window().and_then(|window| window.performance());
    performance.map_or(0.0, |performance| performance.now())
}

pub fn log(message: &str) {
    web_sys::console::warn_1(&message.into());
}

/// Run `callback` once after `delay_ms`.
pub fn set_timeout(window: &Window, delay_ms: i32, callback: impl FnOnce() + 'static) -> TimerHandle {
    let callback = Closure::once_into_js(callback);
    window.set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), delay_ms).unwrap_or(0)
}

/// Hand `closure` to JS as a function. It is never freed: every handler lives as long as the page.
pub fn into_js_function<F: ?Sized + WasmClosure>(closure: Closure<F>) -> Function {
    closure.into_js_value().unchecked_into()
}

/// Call `handler` every time `target` fires `event`.
pub fn add_listener(target: &EventTarget, event: &str, handler: impl FnMut() + 'static) -> Result<(), JsValue> {
    let handler = into_js_function(Closure::<dyn FnMut()>::new(handler));
    target.add_event_listener_with_callback(event, &handler)
}
