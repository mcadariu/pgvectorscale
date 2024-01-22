use std::cmp::Ordering;
use std::mem::size_of;
use std::pin::Pin;

use ndarray::Array3;
use pgrx::pg_sys::{InvalidBlockNumber, InvalidOffsetNumber, BLCKSZ};
use pgrx::*;
use reductive::pq::Pq;
use rkyv::vec::ArchivedVec;
use rkyv::{Archive, Archived, Deserialize, Serialize};

use crate::util::page::PageType;
use crate::util::tape::Tape;
use crate::util::{
    ArchivedItemPointer, HeapPointer, IndexPointer, ItemPointer, ReadableBuffer, WritableBuffer,
};

use super::meta_page::MetaPage;
use super::stats::StatsNodeRead;
use super::storage::StorageType;

#[derive(Archive, Deserialize, Serialize)]
#[archive(check_bytes)]
#[repr(C)]
pub struct PqQuantizerDef {
    dim_0: usize,
    dim_1: usize,
    dim_2: usize,
    vec_len: usize,
    next_vector_pointer: ItemPointer,
}

impl PqQuantizerDef {
    pub fn new(dim_0: usize, dim_1: usize, dim_2: usize, vec_len: usize) -> PqQuantizerDef {
        {
            Self {
                dim_0,
                dim_1,
                dim_2,
                vec_len,
                next_vector_pointer: ItemPointer {
                    block_number: 0,
                    offset: 0,
                },
            }
        }
    }

    pub unsafe fn write(&self, tape: &mut Tape) -> ItemPointer {
        let bytes = rkyv::to_bytes::<_, 256>(self).unwrap();
        tape.write(&bytes)
    }
    pub unsafe fn read<'a>(
        index: &'a PgRelation,
        index_pointer: &ItemPointer,
    ) -> ReadablePqQuantizerDef<'a> {
        let rb = index_pointer.read_bytes(index);
        ReadablePqQuantizerDef { _rb: rb }
    }
}

pub struct ReadablePqQuantizerDef<'a> {
    _rb: ReadableBuffer<'a>,
}

impl<'a> ReadablePqQuantizerDef<'a> {
    pub fn get_archived_node(&self) -> &ArchivedPqQuantizerDef {
        // checking the code here is expensive during build, so skip it.
        // TODO: should we check the data during queries?
        //rkyv::check_archived_root::<Node>(self._rb.get_data_slice()).unwrap()
        unsafe { rkyv::archived_root::<PqQuantizerDef>(self._rb.get_data_slice()) }
    }
}

#[derive(Archive, Deserialize, Serialize)]
#[archive(check_bytes)]
#[repr(C)]
pub struct PqQuantizerVector {
    vec: Vec<f32>,
    next_vector_pointer: ItemPointer,
}

impl PqQuantizerVector {
    pub unsafe fn write(&self, tape: &mut Tape) -> ItemPointer {
        let bytes = rkyv::to_bytes::<_, 8192>(self).unwrap();
        tape.write(&bytes)
    }
    pub unsafe fn read<'a>(
        index: &'a PgRelation,
        index_pointer: &ItemPointer,
    ) -> ReadablePqVectorNode<'a> {
        let rb = index_pointer.read_bytes(index);
        ReadablePqVectorNode { _rb: rb }
    }
}

//ReadablePqNode ties an archive node to it's underlying buffer
pub struct ReadablePqVectorNode<'a> {
    _rb: ReadableBuffer<'a>,
}

impl<'a> ReadablePqVectorNode<'a> {
    pub fn get_archived_node(&self) -> &ArchivedPqQuantizerVector {
        // checking the code here is expensive during build, so skip it.
        // TODO: should we check the data during queries?
        //rkyv::check_archived_root::<Node>(self._rb.get_data_slice()).unwrap()
        unsafe { rkyv::archived_root::<PqQuantizerVector>(self._rb.get_data_slice()) }
    }
}

pub unsafe fn read_pq<S: StatsNodeRead>(
    index: &PgRelation,
    index_pointer: &IndexPointer,
    stats: &mut S,
) -> Pq<f32> {
    //TODO: handle stats better
    let rpq = PqQuantizerDef::read(index, &index_pointer);
    stats.record_read();
    let rpn = rpq.get_archived_node();
    let size = rpn.dim_0 * rpn.dim_1 * rpn.dim_2;
    let mut result: Vec<f32> = Vec::with_capacity(size as usize);
    let mut next = rpn.next_vector_pointer.deserialize_item_pointer();
    loop {
        if next.offset == 0 && next.block_number == 0 {
            break;
        }
        let qvn = PqQuantizerVector::read(index, &next);
        stats.record_read();
        let vn = qvn.get_archived_node();
        result.extend(vn.vec.iter());
        next = vn.next_vector_pointer.deserialize_item_pointer();
    }
    let sq = Array3::from_shape_vec(
        (rpn.dim_0 as usize, rpn.dim_1 as usize, rpn.dim_2 as usize),
        result,
    )
    .unwrap();
    Pq::new(None, sq)
}

pub unsafe fn write_pq(pq: &Pq<f32>, index: &PgRelation) -> ItemPointer {
    let vec = pq.subquantizers().to_slice_memory_order().unwrap().to_vec();
    let shape = pq.subquantizers().dim();
    let mut pq_node = PqQuantizerDef::new(shape.0, shape.1, shape.2, vec.len());

    let mut pqt = Tape::new(index, PageType::PqQuantizerDef);

    // write out the large vector bits.
    // we write "from the back"
    let mut prev: IndexPointer = ItemPointer {
        block_number: 0,
        offset: 0,
    };
    let mut prev_vec = vec;

    // get numbers that can fit in a page by subtracting the item pointer.
    let block_fit = (BLCKSZ as usize / size_of::<f32>()) - size_of::<ItemPointer>() - 64;
    let mut tape = Tape::new(index, PageType::PqQuantizerVector);
    loop {
        let l = prev_vec.len();
        if l == 0 {
            pq_node.next_vector_pointer = prev;
            return pq_node.write(&mut pqt);
        }
        let lv = prev_vec;
        let ni = if l > block_fit { l - block_fit } else { 0 };
        let (b, a) = lv.split_at(ni);

        let pqv_node = PqQuantizerVector {
            vec: a.to_vec(),
            next_vector_pointer: prev,
        };
        let index_pointer: IndexPointer = pqv_node.write(&mut tape);
        prev = index_pointer;
        prev_vec = b.clone().to_vec();
    }
}