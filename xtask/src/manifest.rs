//! The only parser of the gate contract. Public fields are shared with later producers.
#![allow(dead_code)]
mod load;
pub mod schema;
pub mod validate;
pub use load::load;
#[cfg(test)]
pub use load::parse;
pub use schema::*;
