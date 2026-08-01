pub mod arena;
pub mod ascii;
pub mod protocol;
pub mod query;
pub mod record;
pub mod store;

#[cfg(test)]
mod tests;

pub use arena::{ArchivedArena, Arena, ArenaBuilder};
pub use protocol::{QueryKind, Request, ResultEntry};
pub use query::Query;
pub use record::{
    filetime_to_secs, FileRecord, FILETIME_UNIX_EPOCH_SECS, FORMAT_VERSION, BUCKET_SIZE, DIR_BIT,
    PARENT_NONE,
};
pub use store::{ArenaStore, StoreError};
