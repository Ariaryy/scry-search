use crate::ascii;
use crate::record::{pack_parent, BUCKET_SIZE, PARENT_NONE};
use crate::trigram::{for_each_trigram, num_blocks, row_bytes, TRIGRAM_BLOCK};

use super::names::*;
use super::*;
use crate::store::save;

fn simple_arena(names: &[&str]) -> Arena {
    let mut b = ArenaBuilder::default();
    for &name in names {
        b.push(name, 0, false);
    }
    b.build().0
}

mod benchmarks;
mod builders;
mod names;
mod tree;
