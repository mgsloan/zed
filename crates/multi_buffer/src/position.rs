use std::{
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::{Add, AddAssign, Sub, SubAssign},
};
use text::Point;

#[repr(transparent)]
pub struct Offset<T> {
    pub value: usize,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub struct Point<T> {
    pub value: Point,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub struct Row<T> {
    pub value: u32,
    _marker: PhantomData<T>,
}

impl<T> Offset<T> {
    pub fn new(offset: usize) -> Self {
        Self {
            value: offset,
            _marker: PhantomData,
        }
    }

    pub fn saturating_sub(self, n: Offset<T>) -> Self {
        Self {
            value: self.value.saturating_sub(n.value),
            _marker: PhantomData,
        }
    }

    pub fn zero() -> Self {
        Self::new(0)
    }

    pub fn is_zero(&self) -> bool {
        self.value == 0
    }
}

impl<T> Point<T> {
    pub fn new(row: u32, column: u32) -> Self {
        Self {
            value: Point::new(row, column),
            _marker: PhantomData,
        }
    }

    pub fn wrap(point: Point) -> Self {
        Self {
            value: point,
            _marker: PhantomData,
        }
    }

    pub fn row(&self) -> u32 {
        self.value.row
    }

    pub fn column(&self) -> u32 {
        self.value.column
    }

    pub fn zero() -> Self {
        Self::wrap(Point::zero())
    }

    pub fn is_zero(&self) -> bool {
        self.value.is_zero()
    }
}

impl<T> Row<T> {
    pub fn new(row: u32) -> Self {
        Self {
            value: row,
            _marker: PhantomData,
        }
    }
}

impl<T> Copy for Offset<T> {}
impl<T> Copy for Point<T> {}
impl<T> Copy for Row<T> {}

impl<T> Clone for Offset<T> {
    fn clone(&self) -> Self {
        *self
    }
}
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

impl<T> Default for Offset<T> {
    fn default() -> Self {
        Self::new(0)
    }
}
impl<T> Default for Point<T> {
    fn default() -> Self {
        Self::wrap(Point::default())
    }
}
impl<T> Default for Row<T> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<T> PartialOrd for Offset<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.value.cmp(&other.value))
    }
}
impl<T> PartialOrd for Point<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.value.cmp(&other.value))
    }
}
impl<T> PartialOrd for Row<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.value.cmp(&other.value))
    }
}

impl<T> Ord for Offset<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}
impl<T> Ord for Point<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}
impl<T> Ord for Row<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<T> PartialEq for Offset<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<T> PartialEq for Point<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<T> PartialEq for Row<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for Offset<T> {}
impl<T> Eq for Point<T> {}
impl<T> Eq for Row<T> {}

impl<T> Debug for Offset<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Offset({})", type_name::<T>(), self.value)
    }
}
impl<T> Debug for Point<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}Point({}, {})",
            type_name::<T>(),
            self.value.row,
            self.value.column
        )
    }
}
impl<T> Debug for Row<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}Row({})", type_name::<T>(), self.value)
    }
}

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

fn type_name<T>() -> &'static str {
    std::any::type_name::<T>().split("::").last().unwrap()
}

impl<T> Add<Offset<T>> for Offset<T> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Offset::new(self.value + other.value)
    }
}
impl<T> Add<Point<T>> for Point<T> {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Point::wrap(self.value + other.value)
    }
}

impl<T> Sub<Offset<T>> for Offset<T> {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Offset::new(self.value - other.value)
    }
}
impl<T> Sub<Point<T>> for Point<T> {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Point::wrap(self.value - other.value)
    }
}

impl<T> AddAssign<Offset<T>> for Offset<T> {
    fn add_assign(&mut self, other: Self) {
        self.value += other.value;
    }
}
impl<T> AddAssign<Point<T>> for Point<T> {
    fn add_assign(&mut self, other: Self) {
        self.value += other.value;
    }
}

impl<T> SubAssign<Self> for Offset<T> {
    fn sub_assign(&mut self, other: Self) {
        self.value -= other.value;
    }
}
impl<T> SubAssign<Self> for Row<T> {
    fn sub_assign(&mut self, other: Self) {
        self.value -= other.value;
    }
}
