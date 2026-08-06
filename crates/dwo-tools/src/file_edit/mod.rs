mod apply_patch;
mod manager;

pub use apply_patch::{PatchApplication, PatchChange, apply_patch};
pub use manager::{FileEditManager, FileEditResult};
