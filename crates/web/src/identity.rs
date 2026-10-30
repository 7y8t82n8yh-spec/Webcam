//! Who this phone is to the PC: a per-tab client id, and the pairing key from the
//! QR code link that proves the phone may stream.

use web_sys::{Storage, UrlSearchParams, Window};

const CLIENT_ID_STORAGE_KEY: &str = "clientId";
const PAIRING_KEY_STORAGE_KEY: &str = "pairingKey";
const PAIRING_KEY_URL_PARAM: &str = "k";
/// Turns `Math.random()` in [0, 1) into a large integer for the id.
const RANDOM_ID_SCALE: f64 = 1e16;

/// The id the PC knows this tab by, kept in session storage so a reload is the same client.
pub fn client_id(window: &Window) -> String {
    let storage = window.session_storage().ok().flatten();
    let stored_id = storage.as_ref().and_then(|storage| read_item(storage, CLIENT_ID_STORAGE_KEY));
    if let Some(id) = stored_id {
        return id;
    }
    let id = random_id();
    if let Some(storage) = storage {
        let _ = storage.set_item(CLIENT_ID_STORAGE_KEY, &id);
    }
    id
}

/// Pairing key from the QR code link. Remembered so a plain reload keeps working.
pub fn pairing_key(window: &Window) -> String {
    let key_from_url = pairing_key_from_url(window);
    let storage = window.local_storage().ok().flatten();
    let Some(key) = key_from_url else {
        let stored_key = storage.and_then(|storage| read_item(&storage, PAIRING_KEY_STORAGE_KEY));
        return stored_key.unwrap_or_default();
    };
    if let Some(storage) = storage {
        let _ = storage.set_item(PAIRING_KEY_STORAGE_KEY, &key);
    }
    key
}

fn pairing_key_from_url(window: &Window) -> Option<String> {
    let search = window.location().search().ok()?;
    let params = UrlSearchParams::new_with_str(&search).ok()?;
    params.get(PAIRING_KEY_URL_PARAM)
}

fn read_item(storage: &Storage, key: &str) -> Option<String> {
    storage.get_item(key).ok().flatten()
}

fn random_id() -> String {
    let id = (js_sys::Math::random() * RANDOM_ID_SCALE) as u64;
    format!("{id:x}")
}
