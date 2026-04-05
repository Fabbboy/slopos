pub mod blockmap;
pub mod cache;
pub mod dir;
#[path = "alloc.rs"]
pub mod ext2_alloc;
pub mod file;
pub mod inode;
pub mod ondisk;
pub mod ops;
pub mod symlink;
pub mod time;
pub mod types;

// Re-export the legacy implementation unchanged so all existing consumers compile.
// Subsequent phases will gradually move code from legacy into the new submodules
// and remove this re-export.
mod legacy;
pub use legacy::*;
