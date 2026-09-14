mod apply_patch;
mod manager;

pub use apply_patch::{PatchApplication, PatchChange, PatchFailure, apply_patch};
pub use manager::{FileEditManager, FileEditResult};
