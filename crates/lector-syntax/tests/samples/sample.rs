//! Crate docs.
use std::collections::HashMap;
use std::fmt::{self, Display};

/// A point in 2D space.
#[derive(Debug, Clone, PartialEq)]
pub struct Point<T = f64> {
    pub x: T,
    pub y: T,
}

pub enum Shape {
    Circle { center: Point, radius: f64 },
    Rect(Point, Point),
    Empty,
}

pub trait Area {
    const UNITS: &'static str;
    fn area(&self) -> f64;
    fn describe(&self) -> String {
        format!("area = {:.2} {}", self.area(), Self::UNITS)
    }
}

impl Area for Shape {
    const UNITS: &'static str = "m^2";

    fn area(&self) -> f64 {
        match self {
            Shape::Circle { radius, .. } => std::f64::consts::PI * radius * radius,
            Shape::Rect(a, b) => ((b.x - a.x) * (b.y - a.y)).abs(),
            Shape::Empty => 0.0,
        }
    }
}

impl Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

pub mod util {
    pub type Registry = std::collections::HashMap<String, u32>;
    pub const MAX_ITEMS: usize = 1_000;
    static GREETING: &str = "hello\n";

    macro_rules! square {
        ($x:expr) => { $x * $x };
    }

    pub fn count_words(text: &str) -> Registry {
        let mut counts = Registry::new();
        for word in text.split_whitespace() {
            *counts.entry(word.to_lowercase()).or_insert(0) += 1;
        }
        counts
    }
}

fn main() {
    let shapes = vec![Shape::Empty, Shape::Rect(Point { x: 0.0, y: 0.0 }, Point { x: 2.0, y: 3.0 })];
    let total: f64 = shapes.iter().map(|s| s.area()).sum();
    println!("{total}");
    let _ = HashMap::<String, i32>::new();
    if total > 1.0 && true {
        let closure = |a: i32| a + 1;
        let _ = closure(1);
    } else {
        return;
    }
}
