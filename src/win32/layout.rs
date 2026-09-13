use std::mem::size_of;
use std::ptr;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MONITORINFOEXW, MonitorFromPoint,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

use super::{COLLAPSED_DIP, PANEL_WIDTH_DIP};
use crate::config::{AnchorEdge, AppConfigV1};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExpansionAlignment {
    #[default]
    Start,
    End,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PanelAnimation {
    pub(super) started_at: Instant,
    pub(super) duration: Duration,
    pub(super) anchor_rect: RECT,
    pub(super) work_area: RECT,
    pub(super) canvas_destination: POINT,
    pub(super) ball_center_screen: POINT,
    pub(super) expanding: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AnimationSample {
    pub(super) expansion: f32,
    pub(super) ball_opacity: f32,
    pub(super) panel_opacity: f32,
    pub(super) finished: bool,
}

impl PanelAnimation {
    pub(super) fn sample(self, now: Instant) -> AnimationSample {
        let elapsed = now.saturating_duration_since(self.started_at);
        let timeline = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        let mut sample = animation_sample(self.expanding, timeline);
        sample.finished = elapsed >= self.duration;
        sample
    }
}

pub(super) struct MonitorDetails {
    pub(super) info: MONITORINFOEXW,
}

pub(super) fn monitor_info(monitor: HMONITOR) -> Result<MonitorDetails, AppError> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFOEXW>())
                .map_err(|_| AppError::Windows("显示器结构大小溢出".to_owned()))?,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: MONITORINFOEXW begins with MONITORINFO and cbSize identifies the extended structure.
    let ok = unsafe { GetMonitorInfoW(monitor, ptr::from_mut(&mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return Err(AppError::Windows("无法读取显示器工作区".to_owned()));
    }
    Ok(MonitorDetails { info })
}

pub(super) fn primary_monitor() -> HMONITOR {
    // SAFETY: the default flag guarantees a monitor handle even if the point lies outside a display.
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

pub(super) fn monitor_for_device(device: &str) -> Option<HMONITOR> {
    if device.is_empty() {
        return None;
    }
    struct Search<'a> {
        target: &'a str,
        found: Option<HMONITOR>,
    }
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: data points to Search for the synchronous duration of EnumDisplayMonitors.
        let search = unsafe { &mut *(data.0 as *mut Search<'_>) };
        if monitor_info(monitor)
            .is_ok_and(|details| monitor_device(&details).eq_ignore_ascii_case(search.target))
        {
            search.found = Some(monitor);
            return false.into();
        }
        true.into()
    }
    let mut search = Search {
        target: device,
        found: None,
    };
    // SAFETY: callback is synchronous and data points to search until enumeration returns.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(ptr::from_mut(&mut search) as isize),
        )
    };
    search.found
}

pub(super) fn monitor_device(details: &MonitorDetails) -> String {
    String::from_utf16_lossy(&details.info.szDevice)
        .trim_end_matches('\0')
        .to_owned()
}

pub(super) fn position_from_config(
    config: &AppConfigV1,
    monitor: &MonitorDetails,
    side: i32,
    dpi: u32,
) -> POINT {
    let work = monitor.info.monitorInfo.rcWork;
    let offset = dip_to_px(config.placement.offset_dip.max(0.0), dpi);
    match config.placement.edge {
        AnchorEdge::Left => POINT {
            x: work.left,
            y: (work.top + offset).clamp(work.top, work.bottom - side),
        },
        AnchorEdge::Right => POINT {
            x: work.right - side,
            y: (work.top + offset).clamp(work.top, work.bottom - side),
        },
        AnchorEdge::Top => POINT {
            x: (work.left + offset).clamp(work.left, work.right - side),
            y: work.top,
        },
        AnchorEdge::Bottom => POINT {
            x: (work.left + offset).clamp(work.left, work.right - side),
            y: work.bottom - side,
        },
    }
}

pub(super) fn expanded_destination(
    rect: RECT,
    edge: AnchorEdge,
    width: i32,
    height: i32,
    work: RECT,
) -> (POINT, ExpansionAlignment) {
    match edge {
        AnchorEdge::Left | AnchorEdge::Right => {
            let (y, alignment) =
                expansion_axis(rect.top, rect.bottom, height, work.top, work.bottom);
            let x = if edge == AnchorEdge::Left {
                rect.left
            } else {
                rect.right.saturating_sub(width)
            };
            (
                POINT {
                    x: clamp_axis(x, width, work.left, work.right),
                    y,
                },
                alignment,
            )
        }
        AnchorEdge::Top | AnchorEdge::Bottom => {
            let (x, alignment) =
                expansion_axis(rect.left, rect.right, width, work.left, work.right);
            let y = if edge == AnchorEdge::Top {
                rect.top
            } else {
                rect.bottom.saturating_sub(height)
            };
            (
                POINT {
                    x,
                    y: clamp_axis(y, height, work.top, work.bottom),
                },
                alignment,
            )
        }
    }
}

pub(super) fn anchored_destination(
    rect: RECT,
    edge: AnchorEdge,
    width: i32,
    height: i32,
    work: RECT,
    alignment: ExpansionAlignment,
) -> POINT {
    let mut x = match alignment {
        ExpansionAlignment::Start => rect.left,
        ExpansionAlignment::End => rect.right.saturating_sub(width),
    };
    let mut y = match alignment {
        ExpansionAlignment::Start => rect.top,
        ExpansionAlignment::End => rect.bottom.saturating_sub(height),
    };
    match edge {
        AnchorEdge::Left => x = rect.left,
        AnchorEdge::Right => x = rect.right.saturating_sub(width),
        AnchorEdge::Top => y = rect.top,
        AnchorEdge::Bottom => y = rect.bottom.saturating_sub(height),
    }
    POINT {
        x: clamp_axis(x, width, work.left, work.right),
        y: clamp_axis(y, height, work.top, work.bottom),
    }
}

fn expansion_axis(
    start: i32,
    end: i32,
    extent: i32,
    work_start: i32,
    work_end: i32,
) -> (i32, ExpansionAlignment) {
    let end_aligned = end.saturating_sub(extent);
    let start_fits = start >= work_start && extent <= work_end.saturating_sub(start);
    let end_fits = end_aligned >= work_start && end <= work_end;
    if start_fits || !end_fits {
        (
            clamp_axis(start, extent, work_start, work_end),
            ExpansionAlignment::Start,
        )
    } else {
        (
            clamp_axis(end_aligned, extent, work_start, work_end),
            ExpansionAlignment::End,
        )
    }
}

fn clamp_axis(position: i32, extent: i32, work_start: i32, work_end: i32) -> i32 {
    let available = work_end.saturating_sub(work_start);
    if extent >= available {
        work_start
    } else {
        position.clamp(work_start, work_end - extent)
    }
}

pub(super) fn dip_to_px(value: f32, dpi: u32) -> i32 {
    (value * dpi as f32 / 96.0).round() as i32
}

pub(super) fn px_to_dip(value: i32, dpi: u32) -> f32 {
    value as f32 * 96.0 / dpi.max(1) as f32
}

pub(super) fn lerp(start: f32, end: f32, progress: f32) -> f32 {
    start + (end - start) * progress.clamp(0.0, 1.0)
}

fn animation_sample(expanding: bool, timeline: f32) -> AnimationSample {
    let timeline = timeline.clamp(0.0, 1.0);
    let inverse = 1.0 - timeline;
    let eased = 1.0 - inverse * inverse * inverse;
    let (expansion, ball_opacity, panel_opacity) = if expanding {
        (
            eased,
            1.0 - (timeline / 0.3).clamp(0.0, 1.0),
            ((timeline - 0.4) / 0.6).clamp(0.0, 1.0),
        )
    } else {
        (
            1.0 - eased,
            ((timeline - 0.55) / 0.45).clamp(0.0, 1.0),
            1.0 - (timeline / 0.45).clamp(0.0, 1.0),
        )
    };
    AnimationSample {
        expansion,
        ball_opacity,
        panel_opacity,
        finished: timeline >= 1.0,
    }
}

pub(super) fn animation_shape_rect(
    animation: PanelAnimation,
    expansion: f32,
    dpi: u32,
    panel_height_dip: f32,
    edge: AnchorEdge,
    alignment: ExpansionAlignment,
) -> RECT {
    let width = dip_to_px(lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, expansion), dpi);
    let height = dip_to_px(lerp(COLLAPSED_DIP, panel_height_dip, expansion), dpi);
    let destination = anchored_destination(
        animation.anchor_rect,
        edge,
        width,
        height,
        animation.work_area,
        alignment,
    );
    RECT {
        left: destination.x,
        top: destination.y,
        right: destination.x + width,
        bottom: destination.y + height,
    }
}

pub(super) fn system_animations_enabled() -> bool {
    let mut enabled = 1_i32;
    // SAFETY: pvParam points to writable BOOL-compatible storage for the synchronous query.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(ptr::from_mut(&mut enabled).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    result.map_or(true, |()| enabled != 0)
}

pub(super) fn point_in_rounded_rect(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    if x < 0 || y < 0 || x >= width || y >= height {
        return false;
    }
    let corner_x = if x < radius {
        radius
    } else if x >= width - radius {
        width - radius - 1
    } else {
        return true;
    };
    let corner_y = if y < radius {
        radius
    } else if y >= height - radius {
        height - radius - 1
    } else {
        return true;
    };
    let dx = x - corner_x;
    let dy = y - corner_y;
    dx * dx + dy * dy <= radius * radius
}

pub(super) fn point_is_outside_rounded_rect(point: POINT, rect: RECT, radius: i32) -> bool {
    !point_in_rounded_rect(
        point.x - rect.left,
        point.y - rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        radius,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnchorEdge;
    use crate::win32::{COLLAPSE_ANIMATION_DURATION, EXPAND_ANIMATION_DURATION};

    #[test]
    fn expanding_animation_starts_with_only_ball_content_visible() {
        let sample = animation_sample(true, 0.0);
        assert!(
            sample.expansion.abs() < f32::EPSILON
                && (sample.ball_opacity - 1.0).abs() < f32::EPSILON
                && sample.panel_opacity.abs() < f32::EPSILON
                && !sample.finished
        );
    }

    #[test]
    fn expanding_animation_finishes_with_only_panel_content_visible() {
        let sample = animation_sample(true, 1.0);
        assert!(
            (sample.expansion - 1.0).abs() < f32::EPSILON
                && sample.ball_opacity.abs() < f32::EPSILON
                && (sample.panel_opacity - 1.0).abs() < f32::EPSILON
                && sample.finished
        );
    }

    #[test]
    fn collapsing_animation_finishes_with_only_ball_content_visible() {
        let sample = animation_sample(false, 1.0);
        assert!(
            sample.expansion.abs() < f32::EPSILON
                && (sample.ball_opacity - 1.0).abs() < f32::EPSILON
                && sample.panel_opacity.abs() < f32::EPSILON
                && sample.finished
        );
    }

    #[test]
    fn panel_width_interpolation_reaches_midpoint_at_half_expansion() {
        assert!((lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, 0.5) - 172.0).abs() < f32::EPSILON);
    }

    #[test]
    fn collapsed_animation_shape_stays_inside_fixed_right_anchored_canvas() {
        let animation = PanelAnimation {
            started_at: Instant::now(),
            duration: COLLAPSE_ANIMATION_DURATION,
            anchor_rect: RECT {
                left: 1_431,
                top: 407,
                right: 1_707,
                bottom: 555,
            },
            work_area: RECT {
                left: 0,
                top: 0,
                right: 1_920,
                bottom: 1_040,
            },
            canvas_destination: POINT { x: 1_431, y: 407 },
            ball_center_screen: POINT { x: 1_679, y: 435 },
            expanding: false,
        };
        let shape = animation_shape_rect(
            animation,
            0.0,
            96,
            308.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );
        assert_eq!(
            (shape.left, shape.top, shape.right, shape.bottom),
            (1_651, 407, 1_707, 463)
        );
    }

    #[test]
    fn expanded_animation_shape_follows_the_dynamic_panel_height() {
        // 展开终点高度由调用方按数据传入：无超额 254、双行超额 308。
        let animation = PanelAnimation {
            started_at: Instant::now(),
            duration: EXPAND_ANIMATION_DURATION,
            anchor_rect: RECT {
                left: 1_431,
                top: 407,
                right: 1_487,
                bottom: 463,
            },
            work_area: RECT {
                left: 0,
                top: 0,
                right: 1_920,
                bottom: 1_040,
            },
            canvas_destination: POINT { x: 1_431, y: 407 },
            ball_center_screen: POINT { x: 1_459, y: 435 },
            expanding: true,
        };
        let compact = animation_shape_rect(
            animation,
            1.0,
            96,
            254.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );
        let tall = animation_shape_rect(
            animation,
            1.0,
            96,
            308.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );

        assert_eq!(compact.bottom - compact.top, dip_to_px(254.0, 96));
        assert_eq!(tall.bottom - tall.top, dip_to_px(308.0, 96));
        assert_eq!(tall.right - tall.left, compact.right - compact.left);
    }

    #[test]
    fn rounded_hit_test_excludes_transparent_corner() {
        assert!(!point_in_rounded_rect(0, 0, 100, 50, 16));
        assert!(point_in_rounded_rect(16, 2, 100, 50, 16));
    }

    #[test]
    fn point_beyond_card_requests_outside_collapse() {
        assert!(point_is_outside_rounded_rect(
            POINT { x: 300, y: 80 },
            RECT {
                left: 0,
                top: 0,
                right: 276,
                bottom: 148,
            },
            16,
        ));
    }

    #[test]
    fn point_inside_card_does_not_request_outside_collapse() {
        assert!(!point_is_outside_rounded_rect(
            POINT { x: 146, y: 74 },
            RECT {
                left: 0,
                top: 0,
                right: 276,
                bottom: 148,
            },
            16,
        ));
    }

    #[test]
    fn placement_is_clamped_inside_work_area() {
        let mut config = AppConfigV1::default();
        config.placement.offset_dip = 50_000.0;
        let details = MonitorDetails {
            info: MONITORINFOEXW {
                monitorInfo: MONITORINFO {
                    rcWork: RECT {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1040,
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let point = position_from_config(&config, &details, 56, 96);
        assert_eq!(point, POINT { x: 1864, y: 984 });
    }

    #[test]
    fn right_anchored_expansion_preserves_outer_edge() {
        let (destination, alignment) = expanded_destination(
            RECT {
                left: 1800,
                top: 100,
                right: 1856,
                bottom: 156,
            },
            AnchorEdge::Right,
            276,
            148,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
        );
        assert_eq!(
            (destination, alignment),
            (POINT { x: 1580, y: 100 }, ExpansionAlignment::Start)
        );
    }

    #[test]
    fn top_anchored_expansion_flips_left_when_right_side_does_not_fit() {
        let (destination, alignment) = expanded_destination(
            RECT {
                left: 1860,
                top: 0,
                right: 1916,
                bottom: 56,
            },
            AnchorEdge::Top,
            276,
            148,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
        );
        assert_eq!(
            (destination, alignment),
            (POINT { x: 1640, y: 0 }, ExpansionAlignment::End)
        );
    }

    #[test]
    fn collapse_restores_far_edge_after_leftward_expansion() {
        let destination = anchored_destination(
            RECT {
                left: 1640,
                top: 0,
                right: 1916,
                bottom: 148,
            },
            AnchorEdge::Top,
            56,
            56,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            ExpansionAlignment::End,
        );
        assert_eq!(destination, POINT { x: 1860, y: 0 });
    }
}
