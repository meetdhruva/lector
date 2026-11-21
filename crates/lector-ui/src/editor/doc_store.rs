//! The GPUI side of the headless document store: which
//! [`lector_app::documents::DocumentStore`] the app's tabs use, and the
//! executor it runs on.
//!
//! In the app the store is the `AppCore`'s (so commands, agents and every
//! window share documents) and runs on the shared scheduler. Under GPUI's
//! test dispatcher it runs on the background bridge's scheduler, whose jobs
//! run on the test executor, so `run_until_parked` and fake clocks drive
//! reparses, queries and debounces deterministically.

use std::sync::Arc;

use gpui::{App, Global};
use lector_app::documents::DocumentStore;

use crate::background;

/// The store used by this app's document models.
#[derive(Clone)]
struct GlobalStore(DocumentStore);

impl Global for GlobalStore {}

/// The document store of the app (created on first use).
pub fn document_store(cx: &mut App) -> DocumentStore {
    if let Some(store) = cx.try_global::<GlobalStore>() {
        return store.0.clone();
    }
    let store = new_store(cx);
    cx.set_global(GlobalStore(store.clone()));
    store
}

/// Installs `store` (e.g. one shared with a headless core) before any
/// document is opened.
pub fn set_document_store(store: DocumentStore, cx: &mut App) {
    cx.set_global(GlobalStore(store));
}

fn new_store(cx: &mut App) -> DocumentStore {
    if background::is_test_dispatcher(cx) {
        return DocumentStore::new(Arc::new(background::scheduler(cx)));
    }
    let core = lector_app::AppCore::global();
    (*DocumentStore::for_core(&core)).clone()
}
