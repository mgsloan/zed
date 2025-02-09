struct Offset(u32);
struct OffsetUtf8(u32);
struct OffsetUtf16(u32);

struct Utf16<T>(T);

trait HasPoints {
    type Column;
}

impl HasPoints for Utf16<T> {
    type Column = OffsetUtf16;
}

struct Point<C: HasPoints> {
    row: Row<C>,
    column: C::Column,
    _phantom: PhantomData,
}

struct Row<C> {
    number: u32,
}

struct Offset<C> {
    bytes: u32,
}
