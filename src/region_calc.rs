//! Pure arithmetic for resolving a window-placement [`Preset`] against a monitor's
//! geometry into a concrete pixel [`Region`]. No OS calls, no I/O.

/// A window-placement preset. `Custom` carries explicit absolute-screen-coordinate
/// bounds; the other variants are computed relative to a given monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Preset {
    LeftHalf,
    RightHalf,
    FullScreen,
    Custom { x: i32, y: i32, width: i32, height: i32 },
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

/// Reasons a [`Preset::Custom`] region can fail to resolve. See [`resolve_region`]
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

/// Resolves a [`Preset`] against a monitor's geometry into a concrete [`Region`].
///
/// `Custom` regions are given in absolute screen coordinates and are validated
/// against the monitor's bounds (`[monitor.x, monitor.x + monitor.width)` and
/// `[monitor.y, monitor.y + monitor.height)`, i.e. left/top-inclusive,
/// right/bottom-exclusive):
/// - zero or negative `width`/`height` is rejected (`InvalidDimensions`)
/// - an origin before the monitor's start, or at/after its end, is rejected
///   (`OutOfBounds`) since the region would start entirely off-monitor; an
///   origin exactly at the monitor's start (`x == monitor.x` / `y == monitor.y`)
///   is accepted
/// - a region that starts on-monitor but extends past its edge is clamped to
///   fit within the monitor rather than rejected
pub fn resolve_region(preset: &Preset, monitor: &MonitorGeometry) -> Result<Region, RegionCalcError> {
    match preset {
        Preset::FullScreen => Ok(Region {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        }),
        Preset::LeftHalf => Ok(Region {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width / 2,
            height: monitor.height,
        }),
        Preset::RightHalf => {
            let left_width = monitor.width / 2;
            Ok(Region {
                x: monitor.x + left_width,
                y: monitor.y,
                width: monitor.width - left_width,
                height: monitor.height,
            })
        }
        Preset::Custom { x, y, width, height } => {
            if *width <= 0 || *height <= 0 {
                return Err(RegionCalcError::InvalidDimensions);
            }
            // Precondition: `monitor.x + monitor.width` and `monitor.y + monitor.height`
            // are assumed to fit in an `i32` in practice (real monitor geometry never
            // comes close to `i32::MAX`). `saturating_add` guards against a panic in
            // that theoretical case rather than relying purely on the precondition.
            let monitor_right = monitor.x.saturating_add(monitor.width);
            let monitor_bottom = monitor.y.saturating_add(monitor.height);
            if *x < monitor.x || *x >= monitor_right || *y < monitor.y || *y >= monitor_bottom {
                return Err(RegionCalcError::OutOfBounds);
            }

            let clamped_width = (*width).min(monitor_right - *x);
            let clamped_height = (*height).min(monitor_bottom - *y);

            Ok(Region { x: *x, y: *y, width: clamped_width, height: clamped_height })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_screen_resolves_to_entire_monitor() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };

        let region = resolve_region(&Preset::FullScreen, &monitor).unwrap();

        assert_eq!(region, Region { x: 0, y: 0, width: 1920, height: 1080 });
    }

    #[test]
    fn left_half_resolves_to_left_half_of_monitor() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };

        let region = resolve_region(&Preset::LeftHalf, &monitor).unwrap();

        assert_eq!(region, Region { x: 0, y: 0, width: 960, height: 1080 });
    }

    #[test]
    fn right_half_resolves_to_right_half_of_monitor() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };

        let region = resolve_region(&Preset::RightHalf, &monitor).unwrap();

        assert_eq!(region, Region { x: 960, y: 0, width: 960, height: 1080 });
    }

    #[test]
    fn custom_valid_region_passes_through_unchanged() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 100, y: 50, width: 800, height: 600 };

        let region = resolve_region(&preset, &monitor).unwrap();

        assert_eq!(region, Region { x: 100, y: 50, width: 800, height: 600 });
    }

    #[test]
    fn custom_zero_width_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 0, y: 0, width: 0, height: 600 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn custom_negative_position_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: -10, y: 0, width: 800, height: 600 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn custom_negative_width_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 0, y: 0, width: -800, height: 600 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn custom_negative_height_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 0, y: 0, width: 800, height: -600 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::InvalidDimensions));
    }

    #[test]
    fn custom_origin_exactly_at_monitor_left_edge_is_accepted() {
        let monitor = MonitorGeometry { x: 100, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 100, y: 0, width: 800, height: 600 };

        let region = resolve_region(&preset, &monitor).unwrap();

        assert_eq!(region, Region { x: 100, y: 0, width: 800, height: 600 });
    }

    #[test]
    fn custom_origin_exactly_at_monitor_right_edge_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        // monitor_right = 0 + 1920 = 1920; an origin exactly at monitor_right is
        // off-monitor entirely (the interval is right-exclusive).
        let preset = Preset::Custom { x: 1920, y: 0, width: 100, height: 100 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn custom_origin_exactly_at_monitor_top_edge_is_accepted() {
        let monitor = MonitorGeometry { x: 0, y: 100, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 0, y: 100, width: 800, height: 600 };

        let region = resolve_region(&preset, &monitor).unwrap();

        assert_eq!(region, Region { x: 0, y: 100, width: 800, height: 600 });
    }

    #[test]
    fn custom_origin_exactly_at_monitor_bottom_edge_is_rejected() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        // monitor_bottom = 0 + 1080 = 1080; an origin exactly at monitor_bottom is
        // off-monitor entirely (the interval is right-exclusive).
        let preset = Preset::Custom { x: 0, y: 1080, width: 100, height: 100 };

        let result = resolve_region(&preset, &monitor);

        assert_eq!(result, Err(RegionCalcError::OutOfBounds));
    }

    #[test]
    fn custom_region_overflowing_monitor_is_clamped_to_fit() {
        let monitor = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let preset = Preset::Custom { x: 1000, y: 900, width: 5000, height: 5000 };

        let region = resolve_region(&preset, &monitor).unwrap();

        assert_eq!(region, Region { x: 1000, y: 900, width: 920, height: 180 });
    }

    #[test]
    fn monitor_bounds_computation_does_not_panic_on_near_i32_max_geometry() {
        // monitor.x + monitor.width would overflow i32 with plain addition;
        // resolve_region must not panic when computing monitor bounds.
        let monitor = MonitorGeometry { x: i32::MAX - 10, y: i32::MAX - 10, width: 1000, height: 1000 };
        let preset = Preset::Custom { x: i32::MAX - 10, y: i32::MAX - 10, width: 5, height: 5 };

        let region = resolve_region(&preset, &monitor).unwrap();

        assert_eq!(region, Region { x: i32::MAX - 10, y: i32::MAX - 10, width: 5, height: 5 });
    }

    #[test]
    fn region_calc_error_implements_display_and_std_error() {
        fn assert_is_std_error<E: std::error::Error>(_: &E) {}

        let invalid_dimensions = RegionCalcError::InvalidDimensions;
        let out_of_bounds = RegionCalcError::OutOfBounds;

        assert_is_std_error(&invalid_dimensions);
        assert_is_std_error(&out_of_bounds);
        assert_eq!(invalid_dimensions.to_string(), "region width or height was zero or negative");
        assert_eq!(out_of_bounds.to_string(), "region origin lies outside the monitor's bounds");
    }

    /// Table-driven coverage of all four preset variants across multiple monitor
    /// resolutions (and, for the secondary monitor, a non-zero/negative origin
    /// to prove presets are computed relative to the monitor, not the screen
    /// origin).
    #[test]
    fn all_presets_resolve_correctly_across_monitor_resolutions() {
        let primary_1080p = MonitorGeometry { x: 0, y: 0, width: 1920, height: 1080 };
        let secondary_1440p_left_of_primary =
            MonitorGeometry { x: -2560, y: 0, width: 2560, height: 1440 };

        let cases = [
            (primary_1080p, Preset::FullScreen, Region { x: 0, y: 0, width: 1920, height: 1080 }),
            (primary_1080p, Preset::LeftHalf, Region { x: 0, y: 0, width: 960, height: 1080 }),
            (primary_1080p, Preset::RightHalf, Region { x: 960, y: 0, width: 960, height: 1080 }),
            (
                primary_1080p,
                Preset::Custom { x: 10, y: 10, width: 200, height: 100 },
                Region { x: 10, y: 10, width: 200, height: 100 },
            ),
            (
                secondary_1440p_left_of_primary,
                Preset::FullScreen,
                Region { x: -2560, y: 0, width: 2560, height: 1440 },
            ),
            (
                secondary_1440p_left_of_primary,
                Preset::LeftHalf,
                Region { x: -2560, y: 0, width: 1280, height: 1440 },
            ),
            (
                secondary_1440p_left_of_primary,
                Preset::RightHalf,
                Region { x: -1280, y: 0, width: 1280, height: 1440 },
            ),
            (
                secondary_1440p_left_of_primary,
                Preset::Custom { x: -2000, y: 100, width: 500, height: 400 },
                Region { x: -2000, y: 100, width: 500, height: 400 },
            ),
        ];

        for (monitor, preset, expected) in cases {
            let actual = resolve_region(&preset, &monitor)
                .unwrap_or_else(|e| panic!("expected Ok for {preset:?} on {monitor:?}, got {e:?}"));
            assert_eq!(actual, expected, "preset {preset:?} on monitor {monitor:?}");
        }
    }
}
