//! Pure arithmetic for resolving the [`Zone`] against a monitor's geometry
//! into a concrete pixel [`Region`]. No OS calls, no I/O.

/// The Zone: the single optional user-drawn area, in absolute screen
/// coordinates, that every tracked game's window is placed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The geometry of a monitor in absolute screen coordinates (origin can be
/// negative for monitors positioned left of / above the primary monitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A resolved pixel region, in absolute screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Reasons a [`Zone`] can fail to resolve. See [`resolve_region`]
/// for the exact validation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionCalcError {
    /// `width` or `height` was zero or negative.
    InvalidDimensions,
    /// The region's origin lies entirely outside the monitor's bounds.
    OutOfBounds,
}

impl std::fmt::Display for RegionCalcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegionCalcError::InvalidDimensions => {
                write!(f, "region width or height was zero or negative")
            }
            RegionCalcError::OutOfBounds => {
                write!(f, "region origin lies outside the monitor's bounds")
            }
        }
    }
}

impl std::error::Error for RegionCalcError {}

/// Resolves the [`Zone`] against a monitor's geometry into a concrete [`Region`].
///
/// The Zone is given in absolute screen coordinates and is validated against
/// the monitor's bounds (`[monitor.x, monitor.x + monitor.width)` and
/// `[monitor.y, monitor.y + monitor.height)`, i.e. left/top-inclusive,
/// right/bottom-exclusive):
/// - zero or negative `width`/`height` is rejected (`InvalidDimensions`)
/// - an origin before the monitor's start, or at/after its end, is rejected
///   (`OutOfBounds`) since the region would start entirely off-monitor; an
///   origin exactly at the monitor's start (`x == monitor.x` / `y == monitor.y`)
///   is accepted
/// - a region that starts on-monitor but extends past its edge is clamped to
///   fit within the monitor rather than rejected
pub fn resolve_region(zone: &Zone, monitor: &MonitorGeometry) -> Result<Region, RegionCalcError> {
    let Zone {
        x,
        y,
        width,
        height,
    } = *zone;
    if width <= 0 || height <= 0 {
        return Err(RegionCalcError::InvalidDimensions);
    }
    // Precondition: `monitor.x + monitor.width` and `monitor.y + monitor.height`
    // are assumed to fit in an `i32` in practice (real monitor geometry never
    // comes close to `i32::MAX`). `saturating_add` guards against a panic in
    // that theoretical case rather than relying purely on the precondition.
    let monitor_right = monitor.x.saturating_add(monitor.width);
    let monitor_bottom = monitor.y.saturating_add(monitor.height);
    if x < monitor.x || x >= monitor_right || y < monitor.y || y >= monitor_bottom {
        return Err(RegionCalcError::OutOfBounds);
    }

    let clamped_width = width.min(monitor_right - x);
    let clamped_height = height.min(monitor_bottom - y);

    Ok(Region {
        x,
        y,
        width: clamped_width,
        height: clamped_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_valid_region_passes_through_unchanged() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 100,
            y: 50,
            width: 800,
            height: 600,
        };

        let region = resolve_region(&zone, &monitor).unwrap();

        assert_eq!(
            region,
            Region {
                x: 100,
                y: 50,
                width: 800,
                height: 600
            }
        );
    }

    #[test]
    fn zone_zero_width_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 0,
            y: 0,
            width: 0,
            height: 600,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn zone_negative_position_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: -10,
            y: 0,
            width: 800,
            height: 600,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn zone_negative_width_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 0,
            y: 0,
            width: -800,
            height: 600,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn zone_negative_height_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 0,
            y: 0,
            width: 800,
            height: -600,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn zone_origin_exactly_at_monitor_left_edge_is_accepted() {
        let monitor = MonitorGeometry {
            x: 100,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 100,
            y: 0,
            width: 800,
            height: 600,
        };

        let region = resolve_region(&zone, &monitor).unwrap();

        assert_eq!(
            region,
            Region {
                x: 100,
                y: 0,
                width: 800,
                height: 600
            }
        );
    }

    #[test]
    fn zone_origin_exactly_at_monitor_right_edge_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        // monitor_right = 0 + 1920 = 1920; an origin exactly at monitor_right is
        // off-monitor entirely (the interval is right-exclusive).
        let zone = Zone {
            x: 1920,
            y: 0,
            width: 100,
            height: 100,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn zone_origin_exactly_at_monitor_top_edge_is_accepted() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 100,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 0,
            y: 100,
            width: 800,
            height: 600,
        };

        let region = resolve_region(&zone, &monitor).unwrap();

        assert_eq!(
            region,
            Region {
                x: 0,
                y: 100,
                width: 800,
                height: 600
            }
        );
    }

    #[test]
    fn zone_origin_exactly_at_monitor_bottom_edge_is_rejected() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        // monitor_bottom = 0 + 1080 = 1080; an origin exactly at monitor_bottom is
        // off-monitor entirely (the interval is right-exclusive).
        let zone = Zone {
            x: 0,
            y: 1080,
            width: 100,
            height: 100,
        };

        let result = resolve_region(&zone, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn zone_region_overflowing_monitor_is_clamped_to_fit() {
        let monitor = MonitorGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let zone = Zone {
            x: 1000,
            y: 900,
            width: 5000,
            height: 5000,
        };

        let region = resolve_region(&zone, &monitor).unwrap();

        assert_eq!(
            region,
            Region {
                x: 1000,
                y: 900,
                width: 920,
                height: 180
            }
        );
    }

    #[test]
    fn monitor_bounds_computation_does_not_panic_on_near_i32_max_geometry() {
        // monitor.x + monitor.width would overflow i32 with plain addition;
        // resolve_region must not panic when computing monitor bounds.
        let monitor = MonitorGeometry {
            x: i32::MAX - 10,
            y: i32::MAX - 10,
            width: 1000,
            height: 1000,
        };
        let zone = Zone {
            x: i32::MAX - 10,
            y: i32::MAX - 10,
            width: 5,
            height: 5,
        };

        let region = resolve_region(&zone, &monitor).unwrap();

        assert_eq!(
            region,
            Region {
                x: i32::MAX - 10,
                y: i32::MAX - 10,
                width: 5,
                height: 5
            }
        );
    }

    #[test]
    fn region_calc_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let invalid_dimensions = RegionCalcError::InvalidDimensions;
        let out_of_bounds = RegionCalcError::OutOfBounds;

        assert_is_std_error(&invalid_dimensions);
        assert_is_std_error(&out_of_bounds);
        assert_eq!(
            invalid_dimensions.to_string(),
            "region width or height was zero or negative"
        );
        assert_eq!(
            out_of_bounds.to_string(),
            "region origin lies outside the monitor's bounds"
        );
    }

    #[test]
    fn zone_resolves_relative_to_a_monitor_with_a_negative_origin() {
        let secondary_1440p_left_of_primary = MonitorGeometry {
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let zone = Zone {
            x: -2000,
            y: 100,
            width: 500,
            height: 400,
        };

        let region = resolve_region(&zone, &secondary_1440p_left_of_primary).unwrap();

        assert_eq!(
            region,
            Region {
                x: -2000,
                y: 100,
                width: 500,
                height: 400
            }
        );
    }
}
