//! The bottom layer of Lector (ADR 0005): the pieces every other crate would otherwise define
//! for itself. No dependencies beyond `std`.
//!
//! * [`CancelToken`] / [`Cancelled`]: cooperative cancellation with child tokens and
//!   once-only cancel hooks.
//! * [`fuzzy`]: the one fuzzy scorer (completion, symbol / recent / command pickers; the file
//!   picker keeps its own index and nucleo scoring, see `lector-search`).
//! * [`thread`]: `spawn_named`, the only way owner threads are created (ADR 0003).
//! * [`private`]: the one way Lector creates its own directories (`0700`) and files (`0600`)
//!   under its data directory (ADR 0010).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cancel;
pub mod fuzzy;
pub mod private;
pub mod thread;

pub use cancel::{CancelToken, Cancelled};
