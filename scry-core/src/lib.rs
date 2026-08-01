pub mod arena;
pub mod ascii;
pub mod delta;
pub mod frnmap;
pub mod protocol;
pub mod query;
pub mod record;
pub mod store;
pub mod trigram;
pub mod view;

#[cfg(test)]
mod tests;

pub use arena::{ArchivedArena, Arena, ArenaBuilder};
pub use protocol::{QueryKind, Request, ResultEntry};
pub use query::Query;
pub use record::{
    filetime_to_secs, FileRecord, BUCKET_SIZE, DIR_BIT, FILETIME_UNIX_EPOCH_SECS, FORMAT_VERSION,
    PARENT_NONE,
};
pub use store::{ArenaStore, StoreError};
pub use view::IndexView;
