//! The page itself: its DOM handles, the shared `State`, and the status line.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use web_sys::{Document, HtmlCanvasElement, HtmlElement, HtmlVideoElement, Window};

use crate::identity;
use crate::js::element_by_id;
use crate::state::State;

/// Every callback holds one of these, so they all see the same page and state.
pub type PageRef = Rc<Page>;

pub const PAIRING_KEY_REJECTED: &str = "Pairing key rejected — scan the QR code on the PC again";

/// The colour of the status dot next to the status text.
#[derive(Clone, Copy)]
pub enum StatusKind {
    Neutral,
    Good,
    Bad,
}

impl StatusKind {
    fn css_class(self) -> &'static str {
        match self {
            StatusKind::Neutral => "",
            StatusKind::Good => "ok",
            StatusKind::Bad => "bad",
        }
    }
}

pub struct Page {
    pub window: Window,
    pub document: Document,
    /// The camera preview.
    pub video: HtmlVideoElement,
    /// Off-screen canvas the JPEG fallback draws frames into.
    pub canvas: HtmlCanvasElement,
    /// Identifies this tab to the PC across reconnects.
    pub client_id: String,
    pub pairing_key: String,
    pub state: RefCell<State>,
}

impl Page {
    pub fn new(window: Window) -> PageRef {
        let document = window.document().expect("document");
        let canvas = document.create_element("canvas").expect("canvas");
        Rc::new(Page {
            video: element_by_id(&document, "preview"),
            canvas: canvas.unchecked_into(),
            client_id: identity::client_id(&window),
            pairing_key: identity::pairing_key(&window),
            state: RefCell::new(State::initial()),
            window,
            document,
        })
    }

    pub fn element<T: JsCast>(&self, id: &str) -> T {
        element_by_id(&self.document, id)
    }

    pub fn set_status(&self, text: &str, kind: StatusKind) {
        self.element::<HtmlElement>("state").set_text_content(Some(text));
        let dot_class = format!("dot {}", kind.css_class());
        self.element::<HtmlElement>("dot").set_class_name(&dot_class);
    }

    /// Show `message` on the start screen, bringing it back if needed.
    pub fn show_error(&self, message: &str) {
        self.element::<HtmlElement>("error").set_text_content(Some(message));
        let _ = self.element::<HtmlElement>("start-screen").class_list().remove_1("hidden");
    }

    pub fn is_hidden(&self) -> bool {
        self.document.hidden()
    }
}
