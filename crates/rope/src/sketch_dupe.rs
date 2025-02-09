use std::{marker::PhantomData, ops::Add};

pub struct Utf16<T>(T);

pub trait HasPoints {
    type Row;
    type RowDelta;
    type Column;
}

impl HasPoints for Buffer {
    type Row = Row<Buffer>;
    type RowDelta = RowDelta<Buffer>;
    type Column = CharDelta<Buffer>;
}

impl HasPoints for Utf16<C> {
    type Row = Row<Buffer>;
    type RowDelta = RowDelta<Buffer>;
    type Column = CharDelta<Buffer>;
}

pub struct Point<C: HasPoints> {
    row: C::Row,
    column: C::Column,
}

pub struct Row<C> {
    number: u32,
    _phantom: PhantomData<C>,
}

pub struct Offset<C> {
    byte_pos: u32,
    _phantom: PhantomData<C>,
}

pub struct OffsetUtf16<C> {
    codepoint_pos: u32,
    _phantom: PhantomData<C>,
}

pub struct PointDelta<C> {
    row: C::RowDelta,
    column: C::Column,
}

pub struct RowDelta {
    row_len: u32,
}

pub struct OffsetDelta {
    byte_len: u32,
}

pub struct OffsetUtf16Delta {
    codepoint_len: u32,
}

pub struct CharDelta {
    char_len: u32,
}

impl<C: HasPoints> Add for PointDelta<C> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        todo!()
    }
}

impl<C: HasPoints> Add<PointDelta<C>> for Point<C> {
    type Output = Self;

    fn add(self, other: PointDelta<C>) -> Self {
        todo!()
    }
}
