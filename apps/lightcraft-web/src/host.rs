//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! LightCraft in an `<iframe>` (such as a Nextcloud app). Messages are plain objects with a `type`:
//!
//! - `lightcraft:ready` (to the parent): `{ version }`, the app is listening; send a photo now.
//! - `lightcraft:open` (from the parent): `{ name, path, bytes | blob }`, a photo to edit. It is
//!   added to the library in this browser like a picked or dropped file (once: the same photo
//!   again is the one already there), then selected and shown with the Edit panel.
//! - `lightcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file LightCraft would otherwise download (exports). `name` is its path on the parent's side:
//!   the Export dialog's folder starts as the opened photo's folder, and the parent stores the file
//!   there (creating missing folders) and asks before replacing a file.
//!   An export never takes the place of a photo the parent sent: it gets ` (edited)` in its name.
//! - `lightcraft:dirty` (to the parent): `{ dirty }` while library changes wait to be written to
//!   browser storage.
//! - `lightcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! Only the parent window on the page's own origin is heard or answered.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// A photo the parent sent.
pub struct Opened {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    opened: Rc<RefCell<Vec<Opened>>>,
    /// The folder (parent's side) of the photo opened last: exports go there.
    folder: Rc<RefCell<String>>,
    /// The photos the parent sent (paths, lower case): exports never replace them.
    originals: Rc<RefCell<HashSet<String>>>,
    /// The state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, opened: Rc::default(), folder: Rc::default(), originals: Rc::default(), reported: Rc::default() })
    }

    /// Hear photos from the parent.
    pub fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let opened = self.opened.clone();
        let folder = self.folder.clone();
        let originals = self.originals.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            if string(&data, "type").as_deref() != Some("lightcraft:open") {
                return;
            }
            let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "photo.jpg".to_string());
            if let Some(path) = string(&data, "path").filter(|p| p.starts_with('/')) {
                let dir = path.rsplit_once('/').map(|(dir, _)| dir).filter(|d| !d.is_empty()).unwrap_or("/");
                *folder.borrow_mut() = dir.to_string();
                // the Export dialog's folder (a path on the parent's side) starts there
                lightcraft_ui_egui::control::set_web_export_dir(dir);
                originals.borrow_mut().insert(path.to_lowercase());
            }
            let (opened, ctx) = (opened.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                match bytes(&data).await {
                    Some(bytes) => opened.borrow_mut().push(Opened { name, bytes }),
                    None => log::error!("couldn't read {name} from the page"),
                }
                ctx.request_repaint();
            });
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Photos the parent sent since the last call.
    pub fn take_opened(&self) -> Vec<Opened> {
        std::mem::take(&mut *self.opened.borrow_mut())
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "lightcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "lightcraft:failed".into()), ("error", error.into())]);
    }

    /// Tell the parent when library changes start or finish waiting for browser storage.
    pub fn report_pending(&self, pending: bool) {
        if self.reported.get() != Some(pending) && self.send(&[("type", "lightcraft:dirty".into()), ("dirty", pending.into())]) {
            self.reported.set(Some(pending));
        }
    }

    /// An export: to its path on the parent's side (the Export dialog's folder, which starts as the
    /// opened photo's), or into the opened photo's folder when the path isn't absolute.
    pub fn post_export(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mut target = if path.starts_with('/') {
            path.to_string()
        } else {
            let folder = self.folder.borrow();
            format!("{}/{}", folder.trim_end_matches('/'), crate::store::download_name(path))
        };
        if self.originals.borrow().contains(&target.to_lowercase()) {
            target = edited_name(&target);
        }
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", "lightcraft:save".into()), ("name", target.as_str().into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        log::info!("exported {target} ({} bytes)", bytes.len());
        Ok(())
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// `/a/IMG_1.jpg` → `/a/IMG_1 (edited).jpg`.
fn edited_name(path: &str) -> String {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{dir}/{stem} (edited).{ext}"),
        _ => format!("{dir}/{file} (edited)"),
    }
}

/// `?host=parent`.
pub fn requested() -> bool {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .and_then(|q| web_sys::UrlSearchParams::new_with_str(&q).ok())
        .and_then(|p| p.get("host"))
        .is_some_and(|h| h == "parent")
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` (an `ArrayBuffer` or `Uint8Array`) or `blob`.
async fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        return Some(array.to_vec());
    }
    if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        return Some(js_sys::Uint8Array::new(&bytes).to_vec());
    }
    let blob = js_sys::Reflect::get(data, &JsValue::from_str("blob")).ok()?.dyn_into::<web_sys::Blob>().ok()?;
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await.ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}
