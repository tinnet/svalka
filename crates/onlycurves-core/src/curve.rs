//! Piecewise-linear fan curves: temperature in, duty out.

use std::fmt;

/// One point of a curve: at `celsius`, run at `duty` percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub celsius: f64,
    pub duty: f64,
}

/// A validated fan curve.
///
/// Points are sorted by temperature and duty never falls as the temperature
/// rises. Below the first point the duty is the first point's; above the last
/// point it is the last point's; in between it is interpolated linearly.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    points: Vec<Point>,
}

/// Why a list of points is not a valid curve.
#[derive(Debug, Clone, PartialEq)]
pub enum CurveError {
    Empty,
    /// A temperature or duty is NaN or infinite.
    NotFinite {
        index: usize,
    },
    /// A duty is below 0 % or above 100 %.
    DutyOutOfRange {
        index: usize,
        duty: f64,
    },
    /// Point `index` is not hotter than the point before it.
    NotSorted {
        index: usize,
        celsius: f64,
        previous: f64,
    },
    /// Point `index` has a lower duty than the point before it.
    DutyFalls {
        index: usize,
        duty: f64,
        previous: f64,
    },
}

impl fmt::Display for CurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Points are numbered from 1 in messages, as a person counts them in the file.
        match *self {
            Self::Empty => write!(f, "a curve needs at least one point"),
            Self::NotFinite { index } => write!(f, "point {} is not a finite number", index + 1),
            Self::DutyOutOfRange { index, duty } => {
                write!(
                    f,
                    "point {} has duty {duty} %; duty must be 0–100 %",
                    index + 1
                )
            }
            Self::NotSorted {
                index,
                celsius,
                previous,
            } => write!(
                f,
                "point {} ({celsius} °C) must be hotter than point {} ({previous} °C)",
                index + 1,
                index
            ),
            Self::DutyFalls {
                index,
                duty,
                previous,
            } => write!(
                f,
                "point {} ({duty} %) has less duty than point {} ({previous} %); duty may never fall as temperature rises",
                index + 1,
                index
            ),
        }
    }
}

impl std::error::Error for CurveError {}

impl Curve {
    /// Builds a curve from points, checking the rules in the type's docs.
    ///
    /// # Errors
    ///
    /// Returns the first rule the points break.
    pub fn new(points: Vec<Point>) -> Result<Self, CurveError> {
        if points.is_empty() {
            return Err(CurveError::Empty);
        }
        for (index, point) in points.iter().enumerate() {
            check_point(index, point)?;
        }
        for (index, pair) in points.windows(2).enumerate() {
            check_order(index + 1, pair[0], pair[1])?;
        }
        Ok(Self { points })
    }

    #[must_use]
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// The duty in percent at `celsius`.
    #[must_use]
    pub fn duty_at(&self, celsius: f64) -> f64 {
        let first = self.points[0];
        if celsius <= first.celsius {
            return first.duty;
        }
        for pair in self.points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if celsius <= b.celsius {
                return interpolate(a, b, celsius);
            }
        }
        self.points[self.points.len() - 1].duty
    }
}

fn check_point(index: usize, point: &Point) -> Result<(), CurveError> {
    if !point.celsius.is_finite() || !point.duty.is_finite() {
        return Err(CurveError::NotFinite { index });
    }
    if !(0.0..=100.0).contains(&point.duty) {
        return Err(CurveError::DutyOutOfRange {
            index,
            duty: point.duty,
        });
    }
    Ok(())
}

/// Checks that `point` (at `index`) comes after `previous` in a valid curve.
fn check_order(index: usize, previous: Point, point: Point) -> Result<(), CurveError> {
    if point.celsius <= previous.celsius {
        return Err(CurveError::NotSorted {
            index,
            celsius: point.celsius,
            previous: previous.celsius,
        });
    }
    if point.duty < previous.duty {
        return Err(CurveError::DutyFalls {
            index,
            duty: point.duty,
            previous: previous.duty,
        });
    }
    Ok(())
}

/// The duty on the straight line from `a` to `b` at `celsius`.
fn interpolate(a: Point, b: Point, celsius: f64) -> f64 {
    let fraction = (celsius - a.celsius) / (b.celsius - a.celsius);
    a.duty + fraction * (b.duty - a.duty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(points: &[(f64, f64)]) -> Result<Curve, CurveError> {
        Curve::new(
            points
                .iter()
                .map(|&(celsius, duty)| Point { celsius, duty })
                .collect(),
        )
    }

    /// The CPU cooler curve from the design doc.
    fn cpu_cooler() -> Curve {
        curve(&[(35.0, 30.0), (55.0, 55.0), (75.0, 90.0), (85.0, 100.0)]).unwrap()
    }

    #[test]
    fn duty_at_follows_the_curve() {
        let cases = [
            (20.0, 30.0),  // below the first point: first duty
            (35.0, 30.0),  // on a point
            (45.0, 42.5),  // halfway between 35 °C/30 % and 55 °C/55 %
            (65.0, 72.5),  // halfway between 55 °C/55 % and 75 °C/90 %
            (85.0, 100.0), // on the last point
            (99.0, 100.0), // above the last point: last duty
        ];
        let curve = cpu_cooler();
        for (celsius, expected) in cases {
            assert!(
                (curve.duty_at(celsius) - expected).abs() < 1e-9,
                "{celsius} °C: expected {expected} %, got {} %",
                curve.duty_at(celsius)
            );
        }
    }

    #[test]
    fn a_single_point_is_a_flat_curve() {
        let flat = curve(&[(50.0, 40.0)]).unwrap();

        assert!((flat.duty_at(0.0) - 40.0).abs() < 1e-9);
        assert!((flat.duty_at(100.0) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_broken_curves() {
        let cases: [(&[(f64, f64)], CurveError); 5] = [
            (&[], CurveError::Empty),
            (&[(f64::NAN, 30.0)], CurveError::NotFinite { index: 0 }),
            (
                &[(35.0, 30.0), (55.0, 120.0)],
                CurveError::DutyOutOfRange {
                    index: 1,
                    duty: 120.0,
                },
            ),
            (
                &[(55.0, 30.0), (35.0, 50.0)],
                CurveError::NotSorted {
                    index: 1,
                    celsius: 35.0,
                    previous: 55.0,
                },
            ),
            (
                &[(35.0, 50.0), (55.0, 30.0)],
                CurveError::DutyFalls {
                    index: 1,
                    duty: 30.0,
                    previous: 50.0,
                },
            ),
        ];
        for (points, expected) in cases {
            assert_eq!(curve(points), Err(expected), "points {points:?}");
        }
    }

    #[test]
    fn equal_temperatures_are_not_sorted() {
        let error = curve(&[(50.0, 30.0), (50.0, 40.0)]).unwrap_err();

        assert!(matches!(error, CurveError::NotSorted { .. }));
    }

    #[test]
    fn messages_number_points_from_one() {
        let error = curve(&[(55.0, 30.0), (35.0, 50.0)]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "point 2 (35 °C) must be hotter than point 1 (55 °C)"
        );
    }
}
