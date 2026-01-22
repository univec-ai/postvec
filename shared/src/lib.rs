// File: shared/src/lib.rs
//! # Shared Library
//!
//! This crate serves as a shared library for the application, consolidating common
//! functionalities into a single, reusable package. It is designed to be a central
//! hub for modules that are used across different parts of the application.
//!
//! Currently, it contains and exports the `vectors` module, which provides a rich
//! set of tools for numerical computing, including vector and matrix operations,
//! generic tensor manipulation, and word embedding models.
//!
//! ## Modules
//!
//! - `vectors`: A comprehensive library for numerical computing.
//!
//! By re-exporting the contents of the `vectors` module at the crate root, users
//! can conveniently access types like `FloatVector` and traits like `VectorMathExt`
//! directly, for example: `use shared::FloatVector;`.

// Declare the vectors module, which is defined in the `vectors/mod.rs` file.
pub mod vectors;

// Re-export all public items from the `vectors` module to make them directly
// accessible from the `shared` crate root. This simplifies usage for consumer crates.
pub use vectors::*;

pub mod error;
pub use error::ErrorCode;
