//! HTTP requests to the PC, shared by WebRTC signalling and the HTTP fallback.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, Headers, RequestInit, Response};

use crate::page::Page;

/// The PC's reply when the pairing key is wrong.
pub const HTTP_FORBIDDEN: u16 = 403;

/// POST `body` as JSON with the pairing key; `abort` can cancel the request.
pub async fn post_json(
    page: &Page,
    url: &str,
    body: &str,
    abort: Option<&AbortController>,
) -> Result<Response, JsValue> {
    let headers = Headers::new()?;
    headers.set("Content-Type", "application/json")?;
    headers.set(protocol::KEY_HEADER, &page.pairing_key)?;
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(&body.into());
    if let Some(abort) = abort {
        init.set_signal(Some(&abort.signal()));
    }
    let request = page.window.fetch_with_str_and_init(url, &init);
    Ok(JsFuture::from(request).await?.unchecked_into())
}

pub async fn response_text(response: &Response) -> Result<String, JsValue> {
    let text = JsFuture::from(response.text()?).await?;
    Ok(text.as_string().unwrap_or_default())
}
