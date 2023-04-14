/// Provides a data channels implementation backed by web browser implementations.
#[cfg(target_arch = "wasm32")]
mod wasm;
/// Provides a data channels implementation for native platforms.
#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(target_arch = "wasm32")]
pub(crate) use crate::backend::wasm::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crate::backend::native::*;