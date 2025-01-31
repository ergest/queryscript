pub mod context;
pub mod error;
pub mod functions;
mod normalize;
pub mod runtime;
pub mod sql;

pub mod duckdb;

// NOTE: Datafusion is no longer in the repo, so this is technically dead code.
// pub mod datafusion;

pub use crate::runtime::runtime::*;
pub use context::Context;
pub use error::{Result, RuntimeError};
pub use sql::*;
