use std::{
    marker::PhantomData,
    ops::{Add, Sub},
};

pub struct Buffer;
pub struct Delta;
pub struct Origin<D, S>(PhantomData<D>, PhantomData<S>);

pub struct Utf16<T>(T);

pub struct Absolute;
pub struct Relative;

// todo! Name C something like "Space"? Better name for HasOffsets?

pub trait HasOffsets {
    type Type;
    type Offset: Add<Self::OffsetDelta, Output = Self::Offset>
        + Sub<Self::OffsetDelta, Output = Self::Offset>
        + Sub<Output = Self::OffsetDelta>;
    type OffsetDelta: Add<Output = Self::OffsetDelta> + Sub<Output = Self::OffsetDelta>;
}

impl HasOffsets for Buffer {
    type Type = Absolute;
    type Offset = Chars<Delta>;
    type OffsetDelta = Chars<Delta>;
}

impl<C: HasOffsets> HasOffsets for Utf16<C> {
    type Type = C::Type;
    type Offset = OffsetUtf16<C>;
    type OffsetDelta = OffsetUtf16<Delta>;
}

impl HasOffsets for Delta {
    type Type = Relative;
    type Offset = Chars<Delta>;
    type OffsetDelta = Chars<Delta>;
}

impl<
        Type,
        Offset: Add<OffsetDelta, Output = Offset>
            + Sub<OffsetDelta, Output = Offset>
            + Sub<Output = OffsetDelta>,
        OffsetDelta: Add<Output = OffsetDelta> + Sub<Output = OffsetDelta>,
        Inner: HasOffsets<Type = Type, Offset = Offset, OffsetDelta = OffsetDelta>,
        Container: HasOffsets<Type = Type, Offset = Offset, OffsetDelta = OffsetDelta>,
    > HasOffsets for Origin<Inner, Container>
{
    type Type = Container::Type;
    type Offset = Container::Offset;
    type OffsetDelta = Container::OffsetDelta;
}

pub struct Point<C: HasOffsets> {
    row: Row<C>,
    column: C::Offset,
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

impl<C> Offset<C> {
    pub fn new(bytes: u32) -> Self {
        todo!()
    }
}

impl<C: HasOffsets<Type = Absolute>> Add<Point<Delta>> for Point<C> {
    type Output = Point<Delta>;

    fn add(self, other: Point<Delta>) -> Point<Delta> {
        todo!()
    }
}

impl Add for Offset<Delta> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        todo!()
    }
}

impl<C: HasOffsets<Type = Absolute>> Add<Offset<Delta>> for Offset<C> {
    type Output = Point<Delta>;

    fn add(self, other: Point<Delta>) -> Point<Delta> {
        todo!()
    }
}

impl Sub for Offset<Delta> {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        todo!()
    }
}

impl<C: HasOffsets<Type = Absolute>> Sub<Offset<Delta>> for Offset<C> {
    type Output = Point<Delta>;

    fn sub(self, other: Point<Delta>) -> Point<Delta> {
        todo!()
    }
}

impl<C: HasOffsets<Type = Absolute>> Sub for Offset<C> {
    type Output = Point<Delta>;

    fn sub(self, other: Point<Delta>) -> Point<Delta> {
        todo!()
    }
}

fn test() {
    let hmm = Offset::<Buffer>::new(0) + Offset::<Buffer>::new(1);
}

// Conversions

struct Same<Inner, Container>(PhantomData<Inner>, PhantomData<Container>);

fn coerce<
    Type,
    Offset,
    Inner: HasOffsets<Type = Type, Offset = Offset>,
    Container: HasOffsets<Type = Type, Offset = Offset>,
>(
    same: Same<Inner, Container>,
    from: Point<Inner>,
) -> Point<Container> {
    todo!()
}

// TODO use OffsetDelta?

fn to_inner<Inner, Container, Type, Offset>(
    origin: Point<Origin<Inner, Container>>,
    from: Point<Container>,
) -> Point<Inner>
where
    Inner: HasOffsets<Type = Type, Offset = Offset>,
    Container: HasOffsets<Type = Type, Offset = Offset>,
{
    // todo! can crash
    Point {
        row: Row {
            number: from.row.number - origin.row.number,
            _phantom: PhantomData,
        },
        column: from.column - origin.column,
    }
}
