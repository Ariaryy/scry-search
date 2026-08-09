pub mod arena;
pub mod ascii;
pub mod bitvec;
pub mod cancel;
pub mod delta;
pub mod dfs;
pub mod frnmap;
pub mod intervals;
mod literals;
pub mod metrics;
pub mod protocol;
pub mod query;
pub mod rank;
pub mod record;
pub mod spool;
pub mod store;
pub mod terms;
pub mod trigram;
pub mod view;

#[cfg(test)]
mod tests;

pub use arena::{ArchivedArena, Arena, ArenaBuilder};
pub use cancel::Cancellation;
pub use frnmap::FrnEntry;
pub use protocol::{QueryKind, Request, ResultEntry};
pub use query::Query;
pub use record::{
    bytes_to_size_kib, filetime_to_secs, pack_parent, unpack_parent, word_is_dir, BUCKET_SIZE,
    DIR_BIT, FILETIME_UNIX_EPOCH_SECS, FORMAT_VERSION, PARENT_NONE,
};
pub use store::{ArenaStore, StoreError};
pub use view::IndexView;
