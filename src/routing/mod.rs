pub mod inode;
pub mod table;

pub use inode::{NewFileParams, StorageLocation, VirtualInode};
pub use table::InMemoryRoutingTable;
