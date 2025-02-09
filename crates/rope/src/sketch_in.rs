use std::{marker::PhantomData, ops::Add};

pub struct Delta;

// I believe this could be avoided via specialization.
pub struct In<T>(T);

pub struct Utf16<T>(T);

pub trait HasPoints {
    type Row;
    type Column;
}

impl HasPoints for Buffer {
    type Row = Row<Buffer>;
    type Column = Chars<Delta>;
}

impl HasPoints for Utf16<Buffer> {
    type Row = Row<Buffer>;
    type Column = OffsetUtf16<Buffer>;
}

impl HasPoints for Delta {
    type Row = Row<Delta>;
    type Column = Chars<Delta>;
}

impl HasPoints for Utf16<Delta> {
    type Row = Row<Delta>;
    type Column = OffsetUtf16<Delta>;
}

impl<T: HasPoints> HasPoints for In<T> {
    type Row = T::Row;
    type Column = T::Column;
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
    byte_len: u32,
    _phantom: PhantomData<C>,
}

pub struct OffsetUtf16<C> {
    codepoint_len: u32,
    _phantom: PhantomData<C>,
}

pub struct Chars<C> {
    char_len: u32,
    _phantom: PhantomData<C>,
}

impl Add for Point<Delta> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        todo!()
    }
}

impl<C: HasPoints> Add<Point<Delta>> for Point<In<C>> {
    type Output = Point<Delta>;

    fn add(self, other: Point<Delta>) -> Point<Delta> {
        todo!()
    }
}
