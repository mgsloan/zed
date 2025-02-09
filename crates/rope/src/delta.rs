use std::{
    cmp::Ordering,
    fmt::{Debug, Display},
    ops::{Add, AddAssign, Sub, SubAssign},
};

use crate::SaturatingSub;

/// A zero-indexed non-negative difference in points in a text buffer consisting of a row and
/// column.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct DeltaPoint {
    pub row: DeltaRow,
    pub column: DeltaColumn,
}

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct DeltaRow {
    pub count: u32,
}

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct DeltaColumn {
    pub count: u32,
}

#[derive(Clone, Copy, Default, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct DeltaOffset<T> {
    pub count: T,
}

impl DeltaPoint {
    pub const ZERO: Self = Self {
        row: DeltaRow { count: 0 },
        column: DeltaColumn { count: 0 },
    };

    pub const MAX: Self = Self {
        row: DeltaRow { count: u32::MAX },
        column: DeltaColumn { count: u32::MAX },
    };

    pub fn new<R, C>(row: R, column: C) -> Self
    where
        R: Into<DeltaRow>,
        C: Into<DeltaColumn>,
    {
        DeltaPoint {
            row: row.into(),
            column: column.into(),
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

impl DeltaRow {
    pub const ZERO: Self = Self { count: 0 };
    pub const MAX: Self = Self { count: u32::MAX };

    pub fn new(row: u32) -> Self {
        DeltaRow { count: row }
    }

    pub fn is_zero(&self) -> bool {
        self.count == 0
    }
}

impl DeltaColumn {
    pub const ZERO: Self = Self { count: 0 };
    pub const MAX: Self = Self { count: u32::MAX };

    pub fn new(column: u32) -> Self {
        DeltaColumn { count: column }
    }

    pub fn is_zero(&self) -> bool {
        self.count == 0
    }
}

impl DeltaOffset {
    pub const ZERO: Self = Self { count: 0 };
    pub const MAX: Self = Self { count: usize::MAX };

    pub fn new(bytes: usize) -> Self {
        DeltaOffset { count: bytes }
    }

    pub fn is_zero(&self) -> bool {
        self.count == 0
    }
}

impl From<(u32, u32)> for DeltaPoint {
    fn from((row, column): (u32, u32)) -> DeltaPoint {
        DeltaPoint::new(row, column)
    }
}
impl From<u32> for DeltaRow {
    fn from(row_count: u32) -> DeltaRow {
        DeltaRow::new(row_count)
    }
}
impl From<u32> for DeltaColumn {
    fn from(column_count: u32) -> DeltaColumn {
        DeltaColumn::new(column_count)
    }
}
impl From<usize> for DeltaOffset {
    fn from(bytes: usize) -> DeltaOffset {
        DeltaOffset::new(bytes)
    }
}

impl PartialOrd for DeltaPoint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DeltaPoint {
    #[cfg(target_pointer_width = "64")]
    fn cmp(&self, other: &DeltaPoint) -> Ordering {
        let a = (self.row.count as usize) << 32 | self.column.count as usize;
        let b = (other.row.count as usize) << 32 | other.column.count as usize;
        a.cmp(&b)
    }

    #[cfg(target_pointer_width = "32")]
    fn cmp(&self, other: &DeltaPoint) -> Ordering {
        match self.row.cmp(&other.row) {
            Ordering::Equal => self.column.cmp(&other.column),
            comparison @ _ => comparison,
        }
    }
}

impl Add for DeltaPoint {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        if other.row.count == 0 {
            DeltaPoint::new(self.row, self.column + other.column)
        } else {
            DeltaPoint::new(self.row + other.row, other.column)
        }
    }
}

impl Sub for DeltaPoint {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        debug_assert!(other <= self);
        if self.row == other.row {
            Self::new(0, self.column - other.column)
        } else {
            Self::new(self.row - other.row, self.column)
        }
    }
}

impl AddAssign<Self> for DeltaPoint {
    fn add_assign(&mut self, other: Self) {
        if other.row.count == 0 {
            self.column += other.column;
        } else {
            self.row += other.row;
            self.column = other.column;
        }
    }
}

impl SubAssign<Self> for DeltaPoint {
    fn sub_assign(&mut self, other: Self) {
        todo!()
    }
}

impl Debug for DeltaPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeltaPoint({}, {})", self.row.count, self.column.count)
    }
}
impl Debug for DeltaRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeltaRow({})", self.count)
    }
}
impl Debug for DeltaColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeltaColumn({})", self.count)
    }
}
impl Debug for DeltaOffset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeltaOffset({})", self.count)
    }
}

impl Add for DeltaRow {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self::new(self.count + other.count)
    }
}
impl Add for DeltaColumn {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self::new(self.count + other.count)
    }
}
impl Add for DeltaOffset {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self::new(self.count + other.count)
    }
}

impl Sub for DeltaRow {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self::new(self.count - other.count)
    }
}
impl Sub for DeltaColumn {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self::new(self.count - other.count)
    }
}
impl Sub for DeltaOffset {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self::new(self.count - other.count)
    }
}

impl SaturatingSub for DeltaPoint {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> Self {
        if self < other {
            Self::ZERO
        } else {
            self - other
        }
    }
}
impl SaturatingSub for DeltaRow {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> Self {
        if self < other {
            Self::ZERO
        } else {
            self - other
        }
    }
}
impl SaturatingSub for DeltaColumn {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> Self {
        if self < other {
            Self::ZERO
        } else {
            self - other
        }
    }
}
impl SaturatingSub for DeltaOffset {
    type Output = Self;
    fn saturating_sub(self, other: Self) -> Self {
        if self < other {
            Self::ZERO
        } else {
            self - other
        }
    }
}

impl AddAssign<Self> for DeltaRow {
    fn add_assign(&mut self, other: Self) {
        *self = *self + delta;
    }
}
impl AddAssign<Self> for DeltaColumn {
    fn add_assign(&mut self, other: Self) {
        *self = *self + delta;
    }
}
impl AddAssign<Self> for DeltaOffset {
    fn add_assign(&mut self, other: Self) {
        *self = *self + delta;
    }
}

impl SubAssign<Self> for DeltaRow {
    fn sub_assign(&mut self, other: Self) {
        *self = *self - delta;
    }
}
impl SubAssign<Self> for DeltaColumn {
    fn sub_assign(&mut self, other: Self) {
        *self = *self - delta;
    }
}
impl SubAssign<Self> for DeltaOffset {
    fn sub_assign(&mut self, other: Self) {
        self.count -= other.count;
    }
}

impl<'a> Add<&'a Self> for DeltaPoint {
    type Output = Self;
    fn add(self, other: &'a Self) -> Self {
        self + *other
    }
}
impl<'a> Add<&'a Self> for DeltaRow {
    type Output = Self;
    fn add(self, other: &'a Self) -> Self {
        self + *other
    }
}
impl<'a> Add<&'a Self> for DeltaColumn {
    type Output = Self;
    fn add(self, other: &'a Self) -> Self {
        self + *other
    }
}
impl<'a> Add<&'a Self> for DeltaOffset {
    type Output = Self;
    fn add(self, other: &'a Self) -> Self {
        self + *other
    }
}

impl<'a> Sub<&'a Self> for DeltaPoint {
    type Output = Self;
    fn sub(self, other: &'a Self) -> Self {
        self - *other
    }
}
impl<'a> Sub<&'a Self> for DeltaRow {
    type Output = Self;
    fn sub(self, other: &'a Self) -> Self {
        self - *other
    }
}
impl<'a> Sub<&'a Self> for DeltaColumn {
    type Output = Self;
    fn sub(self, other: &'a Self) -> Self {
        self - *other
    }
}
impl<'a> Sub<&'a Self> for DeltaOffset {
    type Output = Self;
    fn sub(self, other: &'a Self) -> Self {
        self - *other
    }
}

impl<'a> AddAssign<&'a Self> for DeltaPoint {
    fn add_assign(&mut self, other: &'a Self) {
        *self += *other;
    }
}
impl<'a> AddAssign<&'a Self> for DeltaRow {
    fn add_assign(&mut self, other: &'a Self) {
        *self += *other;
    }
}
impl<'a> AddAssign<&'a Self> for DeltaColumn {
    fn add_assign(&mut self, other: &'a Self) {
        *self += *other;
    }
}
impl<'a> AddAssign<&'a Self> for DeltaOffset {
    fn add_assign(&mut self, other: &'a Self) {
        *self += *other;
    }
}

impl<'a> SubAssign<&'a Self> for DeltaPoint {
    fn sub_assign(&mut self, other: &'a Self) {
        *self -= *other;
    }
}
impl<'a> SubAssign<&'a Self> for DeltaRow {
    fn sub_assign(&mut self, other: &'a Self) {
        *self -= *other;
    }
}
impl<'a> SubAssign<&'a Self> for DeltaColumn {
    fn sub_assign(&mut self, other: &'a Self) {
        *self -= *other;
    }
}
impl<'a> SubAssign<&'a Self> for DeltaOffset {
    fn sub_assign(&mut self, other: &'a Self) {
        *self -= *other;
    }
}
