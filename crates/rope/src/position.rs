use std::{
    cmp::Ordering,
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::{Add, AddAssign, Range, Sub, SubAssign},
};

use crate::{DeltaColumn, DeltaOffset, DeltaPoint, DeltaRow, SaturatingSub};

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct Bytes(pub u32);

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct Utf8<T>(pub T);

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct Utf16<T>(pub T);

trait HasPoints {
    type Column;
}

trait HasOffsets {
    type Offset;
}

impl<T: HasPoints> HasPoints for Utf8<T> {
    type Column = DeltaOffset<Utf8<u32>>;
}

impl<T: HasPoints> HasPoints for Utf16<T> {
    type Column = DeltaOffset<Utf16<u32>>;
}

impl<T: HasOffsets> HasOffsets for Utf8<T> {
    type Offset = DeltaOffset<Utf8<u32>>;
}

impl<T: HasOffsets> HasOffsets for Utf16<T> {
    type Offset = DeltaOffset<Utf16<u32>>;
}

/// A zero-indexed point in a text buffer consisting of a row and column.
pub struct Point<T: HasPoints> {
    pub row: Row<T>,
    pub column: T::Column,
}

#[repr(transparent)]
pub struct Row<T> {
    pub number: u32,
    _phantom: std::marker::PhantomData<T>,
}

/*
#[repr(transparent)]
pub struct Column<T> {
    pub number: u32,
    _phantom: std::marker::PhantomData<T>,
}
*/

#[repr(transparent)]
pub struct Offset<T> {
    pub position: usize,
    _phantom: std::marker::PhantomData<T>,
}

// todo! move + rename
pub trait OffsetRangeExt {
    fn to_usize(self) -> Range<usize>;
}

impl<T> OffsetRangeExt for Range<Offset<T>> {
    fn to_usize(self) -> Range<usize> {
        unsafe { std::mem::transmute(self) }
    }
}

impl<T> Point<T> {
    pub const ZERO: Self = Self {
        row: Row {
            number: 0,
            _phantom: PhantomData,
        },
        column: Column {
            number: 0,
            _phantom: PhantomData,
        },
    };

    pub const MAX: Self = Self {
        row: Row {
            number: u32::MAX,
            _phantom: PhantomData,
        },
        column: Column {
            number: u32::MAX,
            _phantom: PhantomData,
        },
    };

    pub fn new<R, C>(row: R, column: C) -> Self
    where
        R: Into<Row<T>>,
        C: Into<Column<T>>,
    {
        Point {
            row: row.into(),
            column: column.into(),
        }
    }

    pub fn row_range(range: Range<Row<T>>) -> Range<Self> {
        Point::new(range.start, 0)..Point::new(range.end, 0)
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    #[inline(always)]
    pub fn to_delta(self) -> DeltaPoint {
        unsafe { std::mem::transmute(self) }
    }

    // todo! document why private
    #[inline(always)]
    fn from_delta(delta: DeltaPoint) -> Self {
        unsafe { std::mem::transmute(delta) }
    }
}

impl<T> Row<T> {
    const ZERO: Self = Self::new(0);

    pub const fn new(row_number: u32) -> Self {
        Self {
            number: row_number,
            _phantom: PhantomData,
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn to_delta(self) -> DeltaRow {
        DeltaRow { count: self.number }
    }

    // todo! document why private
    fn from_delta(delta: DeltaRow) -> Self {
        Self::new(delta.count)
    }
}

impl<T> Column<T> {
    const ZERO: Self = Self::new(0);

    pub const fn new(column_number: u32) -> Self {
        Self {
            number: column_number,
            _phantom: PhantomData,
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn to_delta(self) -> DeltaColumn {
        DeltaColumn { count: self.number }
    }

    // todo! document why private
    fn from_delta(delta: DeltaColumn) -> Self {
        Self::new(delta.count)
    }
}

impl<T> Offset<T> {
    const ZERO: Self = Self::new(0);

    pub const fn new(byte_position: usize) -> Self {
        Self {
            position: byte_position,
            _phantom: PhantomData,
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn to_delta(self) -> DeltaOffset {
        DeltaOffset {
            count: self.position,
        }
    }

    // todo! document why private
    fn from_delta(delta: DeltaOffset) -> Self {
        Self::new(delta.count)
    }
}

impl<T> From<(u32, u32)> for Point<T> {
    fn from((row, column): (u32, u32)) -> Self {
        Self::new(row, column)
    }
}
impl<T> From<u32> for Row<T> {
    fn from(row_number: u32) -> Self {
        Self::new(row_number)
    }
}
impl<T> From<u32> for Column<T> {
    fn from(column_number: u32) -> Self {
        Self::new(column_number)
    }
}
impl<T> From<usize> for Offset<T> {
    fn from(byte_position: usize) -> Self {
        Self::new(byte_position)
    }
}

impl<T> From<(u32, u32)> for Utf16<Point<T>> {
    fn from((row, column): (u32, u32)) -> Self {
        Utf16(Point::new(row, column))
    }
}
impl<T> From<u32> for Utf16<Row<T>> {
    fn from(row_number: u32) -> Self {
        Utf16(Row::new(row_number))
    }
}
impl<T> From<u32> for Utf16<Column<T>> {
    fn from(column_number: u32) -> Self {
        Utf16(Column::new(column_number))
    }
}
impl<T> From<usize> for Utf16<Offset<T>> {
    fn from(byte_position: usize) -> Self {
        Utf16(Offset::new(byte_position))
    }
}

impl<T> Debug for Point<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}Point({}, {})",
            type_name::<T>(),
            self.row,
            self.column
        )
    }
}
impl<T> Debug for Row<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Row({})", type_name::<T>(), self.number)
    }
}
impl<T> Debug for Column<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Column({})", type_name::<T>(), self.number)
    }
}
impl<T> Debug for Offset<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Offset({})", type_name::<T>(), self.position)
    }
}

fn type_name<T>() -> &'static str {
    std::any::type_name::<T>().split("::").last().unwrap()
}

impl<T> Copy for Point<T> {}
impl<T> Copy for Row<T> {}
impl<T> Copy for Column<T> {}
impl<T> Copy for Offset<T> {}

impl<T> Clone for Point<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Clone for Row<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Clone for Column<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Clone for Offset<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Default for Point<T> {
    fn default() -> Self {
        Self::ZERO
    }
}
impl<T> Default for Row<T> {
    fn default() -> Self {
        Self::ZERO
    }
}
impl<T> Default for Column<T> {
    fn default() -> Self {
        Self::ZERO
    }
}
impl<T> Default for Offset<T> {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<T> PartialOrd for Point<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> PartialOrd for Row<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> PartialOrd for Column<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> PartialOrd for Offset<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Point<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.to_delta().cmp(&other.to_delta())
    }
}
impl<T> Ord for Row<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}
impl<T> Ord for Column<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}
impl<T> Ord for Offset<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.position.cmp(&other.position)
    }
}

impl<T> PartialEq for Point<T> {
    fn eq(&self, other: &Self) -> bool {
        self.row == other.row && self.column == other.column
    }
}
impl<T> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}
impl<T> PartialEq for Column<T> {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}
impl<T> PartialEq for Offset<T> {
    fn eq(&self, other: &Self) -> bool {
        self.position == other.position
    }
}

impl<T> Eq for Point<T> {}
impl<T> Eq for Row<T> {}
impl<T> Eq for Column<T> {}
impl<T> Eq for Offset<T> {}

/*
todo! Have display at all?  Should it increment by 1 for display?

impl<T> Display for Offset<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.value, f)
    }
}
impl<T> Display for Row<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.value, f)
    }
}
impl<T> Display for Column<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.value, f)
    }
}
*/

impl<T> Add<DeltaPoint> for Point<T> {
    type Output = Self;
    fn add(self, delta: DeltaPoint) -> Self {
        Self::from_delta(self.to_delta() + delta)
    }
}
impl<T> Add<DeltaRow> for Row<T> {
    type Output = Self;
    fn add(self, delta: DeltaRow) -> Self {
        Self::new(self.number + delta.count)
    }
}
impl<T> Add<DeltaColumn> for Column<T> {
    type Output = Self;
    fn add(self, delta: DeltaColumn) -> Self {
        Self::new(self.number + delta.count)
    }
}
impl<T> Add<DeltaOffset> for Offset<T> {
    type Output = Self;
    fn add(self, delta: DeltaOffset) -> Self {
        Self::new(self.position + delta.count)
    }
}

impl<T> Sub<DeltaPoint> for Point<T> {
    type Output = Self;
    fn sub(self, delta: DeltaPoint) -> Self {
        Self::from_delta(self.to_delta() - delta)
    }
}
impl<T> Sub<DeltaRow> for Row<T> {
    type Output = Self;
    fn sub(self, delta: DeltaRow) -> Self {
        Self::new(self.number - delta.count)
    }
}
impl<T> Sub<DeltaColumn> for Column<T> {
    type Output = Self;
    fn sub(self, delta: DeltaColumn) -> Self {
        Self::new(self.number - delta.count)
    }
}
impl<T> Sub<DeltaOffset> for Offset<T> {
    type Output = Self;
    fn sub(self, delta: DeltaOffset) -> Self {
        Self::new(self.position - delta.count)
    }
}

impl<T> Sub for Point<T> {
    type Output = DeltaPoint;
    fn sub(self, other: Self) -> DeltaPoint {
        self.to_delta() - other.to_delta()
    }
}
impl<T> Sub for Row<T> {
    type Output = DeltaRow;
    fn sub(self, other: Self) -> DeltaRow {
        self.to_delta() - other.to_delta()
    }
}
impl<T> Sub for Column<T> {
    type Output = DeltaColumn;
    fn sub(self, other: Self) -> DeltaRow {
        self.to_delta() - other.to_delta()
    }
}
impl<T> Sub for Offset<T> {
    type Output = DeltaOffset;
    fn sub(self, other: Self) -> DeltaOffset {
        self.to_delta() - other.to_delta()
    }
}

impl<T> SaturatingSub<DeltaPoint> for Point<T> {
    type Output = Self;
    fn saturating_sub(self, delta: DeltaPoint) -> Self {
        Self::from_delta(self.to_delta().saturating_sub(delta))
    }
}
impl<T> SaturatingSub<DeltaRow> for Row<T> {
    type Output = Self;
    fn saturating_sub(self, delta: DeltaRow) -> Self {
        Self::from_delta(self.to_delta().saturating_sub(delta))
    }
}
impl<T> SaturatingSub<DeltaColumn> for Column<T> {
    type Output = Self;
    fn saturating_sub(self, delta: DeltaColumn) -> Self {
        Self::from_delta(self.to_delta().saturating_sub(delta))
    }
}
impl<T> SaturatingSub<DeltaOffset> for Offset<T> {
    type Output = Self;
    fn saturating_sub(self, delta: DeltaOffset) -> Self {
        Self::from_delta(self.to_delta().saturating_sub(delta))
    }
}

impl<T> SaturatingSub for Point<T> {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> DeltaPoint {
        self.to_delta().saturating_sub(other.to_delta())
    }
}
impl<T> SaturatingSub for Row<T> {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> DeltaRow {
        self.to_delta().saturating_sub(other.to_delta())
    }
}
impl<T> SaturatingSub for Column<T> {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> DeltaColumn {
        self.to_delta().saturating_sub(other.to_delta())
    }
}
impl<T> SaturatingSub for Offset {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> DeltaOffset {
        self.to_delta().saturating_sub(other.to_delta())
    }
}

impl<T> AddAssign<DeltaPoint> for Point<T> {
    fn add_assign(&mut self, delta: DeltaPoint) {
        *self = *self + delta;
    }
}
impl<T> AddAssign<DeltaRow> for Row<T> {
    fn add_assign(&mut self, delta: DeltaRow) {
        *self = *self + delta;
    }
}
impl<T> AddAssign<DeltaColumn> for Column<T> {
    fn add_assign(&mut self, delta: DeltaColumn) {
        *self = *self + delta;
    }
}
impl<T> AddAssign<DeltaOffset> for Offset<T> {
    fn add_assign(&mut self, delta: DeltaOffset) {
        *self = *self + delta;
    }
}

impl<T> SubAssign<DeltaPoint> for Point<T> {
    fn sub_assign(&mut self, delta: DeltaPoint) {
        *self = *self - delta;
    }
}
impl<T> SubAssign<DeltaRow> for Row<T> {
    fn sub_assign(&mut self, delta: DeltaRow) {
        *self = *self - delta;
    }
}
impl<T> SubAssign<DeltaColumn> for Column<T> {
    fn sub_assign(&mut self, delta: DeltaColumn) {
        *self = *self - delta;
    }
}
impl<T> SubAssign<DeltaOffset> for Offset<T> {
    fn sub_assign(&mut self, delta: DeltaOffset) {
        *self = *self - delta;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rope;

    fn point_to_delta() {
        assert_eq!(Point::<Rope>::new(1, 2).to_delta(), DeltaPoint::new(1, 2));
    }

    fn point_from_delta() {
        assert_eq!(
            Point::<Rope>::from_delta(DeltaPoint::new(1, 2)),
            Point::<Rope>::new(1, 2),
        );
    }
}
