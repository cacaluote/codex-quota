#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use std::ffi::c_void;
use std::ptr;
use std::time::SystemTime;

use windows::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW,
    D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
use windows::core::{Interface, PCWSTR};
use windows_numerics::Vector2;

use super::presentation::{
    PlanColor, classify_quota_windows, format_local_timestamp, format_token_usage, panel_title,
    plan_type_color, plan_type_label,
};
use crate::error::AppError;
use crate::quota::{AppState, QuotaColor, QuotaWindow};

const BACKGROUND: D2D1_COLOR_F = rgba(0x12, 0x17, 0x20, 0.94);
const TRACK: D2D1_COLOR_F = rgba(0x42, 0x4a, 0x57, 0.88);
const PRIMARY_TEXT: D2D1_COLOR_F = rgba(0xf2, 0xf5, 0xf8, 1.0);
const SECONDARY_TEXT: D2D1_COLOR_F = rgba(0xa8, 0xb1, 0xbe, 1.0);
const GREEN: D2D1_COLOR_F = rgba(0x45, 0xd4, 0x83, 1.0);
const YELLOW: D2D1_COLOR_F = rgba(0xf2, 0xc9, 0x4c, 1.0);
const RED: D2D1_COLOR_F = rgba(0xf0, 0x66, 0x66, 1.0);
const UNKNOWN: D2D1_COLOR_F = rgba(0x7d, 0x87, 0x95, 1.0);
const PLAN_NEUTRAL: D2D1_COLOR_F = rgba(0x8f, 0x9b, 0xaa, 1.0);
const PLAN_PLUS: D2D1_COLOR_F = rgba(0x5a, 0xa7, 0xff, 1.0);
const PLAN_PRO: D2D1_COLOR_F = rgba(0xa7, 0x8b, 0xfa, 1.0);
const PLAN_BUSINESS: D2D1_COLOR_F = rgba(0x4f, 0xd1, 0xc5, 1.0);
const PLAN_ENTERPRISE: D2D1_COLOR_F = rgba(0xd8, 0xb4, 0x5c, 1.0);
const PLAN_EDU: D2D1_COLOR_F = rgba(0x56, 0xc7, 0xd9, 1.0);
const TRANSPARENT: D2D1_COLOR_F = rgba(0, 0, 0, 0.0);
const TIME_COLUMN_LEFT: f32 = 140.0;

#[derive(Clone, Copy, Debug)]
pub(super) enum VisualState {
    Collapsed,
    Expanded,
    Transition(TransitionVisual),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TransitionVisual {
    pub expansion: f32,
    pub ball_opacity: f32,
    pub panel_opacity: f32,
    pub ball_center: (f32, f32),
    pub shape_bounds: (f32, f32, f32, f32),
}

const fn rgba(red: u8, green: u8, blue: u8, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: alpha,
    }
}

pub struct Renderer {
    surface: DibSurface,
    factory: ID2D1Factory,
    target: ID2D1DCRenderTarget,
    write_factory: IDWriteFactory,
    brushes: Brushes,
    percent_format: IDWriteTextFormat,
    title_format: IDWriteTextFormat,
    body_format: IDWriteTextFormat,
    dpi: u32,
}

struct Brushes {
    background: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    primary_text: ID2D1SolidColorBrush,
    secondary_text: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    yellow: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
    unknown: ID2D1SolidColorBrush,
    plan_neutral: ID2D1SolidColorBrush,
    plan_plus: ID2D1SolidColorBrush,
    plan_pro: ID2D1SolidColorBrush,
    plan_business: ID2D1SolidColorBrush,
    plan_enterprise: ID2D1SolidColorBrush,
    plan_edu: ID2D1SolidColorBrush,
}

struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Renderer {
    pub fn new(width: i32, height: i32, dpi: u32) -> Result<Self, AppError> {
        let surface = DibSurface::new(width, height)?;
        // SAFETY: Direct2D and DirectWrite factories are created and used only on the UI thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
        // SAFETY: Shared DirectWrite factory creation has no borrowed output lifetime.
        let write_factory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
        let target = create_target(&factory, dpi)?;
        let brushes = create_brushes(&target)?;
        let percent_format = create_text_format(&write_factory, 18.0, true)?;
        let title_format = create_text_format(&write_factory, 14.0, false)?;
        let body_format = create_text_format(&write_factory, 13.0, false)?;

        Ok(Self {
            surface,
            factory,
            target,
            write_factory,
            brushes,
            percent_format,
            title_format,
            body_format,
            dpi,
        })
    }

    pub fn resize(&mut self, width: i32, height: i32, dpi: u32) -> Result<(), AppError> {
        if self.surface.width != width || self.surface.height != height {
            self.surface = DibSurface::new(width, height)?;
        }
        if self.dpi != dpi {
            self.recreate_device_resources(dpi)?;
        }
        Ok(())
    }

    pub fn render(
        &mut self,
        hwnd: HWND,
        destination: POINT,
        visual: VisualState,
        state: &AppState,
    ) -> Result<(), AppError> {
        let result = self.draw(visual, state);
        if result.is_err() {
            self.recreate_device_resources(self.dpi)?;
            self.draw(visual, state)?;
        }
        self.surface.commit(hwnd, destination)
    }

    fn recreate_device_resources(&mut self, dpi: u32) -> Result<(), AppError> {
        self.target = create_target(&self.factory, dpi)?;
        self.brushes = create_brushes(&self.target)?;
        self.percent_format = create_text_format(&self.write_factory, 18.0, true)?;
        self.title_format = create_text_format(&self.write_factory, 14.0, false)?;
        self.body_format = create_text_format(&self.write_factory, 12.0, false)?;
        self.dpi = dpi;
        Ok(())
    }

    fn draw(&mut self, visual: VisualState, state: &AppState) -> Result<(), AppError> {
        self.surface.clear();
        let bounds = RECT {
            left: 0,
            top: 0,
            right: self.surface.width,
            bottom: self.surface.height,
        };
        // SAFETY: the memory DC and its selected DIB remain valid for the full draw operation.
        unsafe {
            self.target.BindDC(self.surface.dc, &bounds)?;
            self.target
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.target.BeginDraw();
            self.target.Clear(Some(&TRANSPARENT));
        }

        match visual {
            VisualState::Collapsed => self.draw_ball(state)?,
            VisualState::Expanded => self.draw_panel(state),
            VisualState::Transition(transition) => {
                self.draw_transition(state, transition)?;
            }
        }

        // SAFETY: EndDraw balances BeginDraw above on the same UI thread.
        unsafe { self.target.EndDraw(None, None) }?;
        Ok(())
    }

    fn draw_ball(&self, state: &AppState) -> Result<(), AppError> {
        let scale = self.dpi as f32 / 96.0;
        let center = Vector2 {
            X: self.surface.width as f32 / (2.0 * scale),
            Y: self.surface.height as f32 / (2.0 * scale),
        };
        self.draw_ball_background(center);
        self.draw_ball_content(state, center, 1.0)
    }

    fn draw_ball_background(&self, center: Vector2) {
        let circle = D2D1_ELLIPSE {
            point: center,
            radiusX: 27.0,
            radiusY: 27.0,
        };
        // SAFETY: the geometry and brush are renderer-owned and valid for this immediate call.
        unsafe {
            self.target.FillEllipse(&circle, &self.brushes.background);
        }
    }

    fn draw_ball_content(
        &self,
        state: &AppState,
        center: Vector2,
        opacity: f32,
    ) -> Result<(), AppError> {
        let opacity = opacity.clamp(0.0, 1.0);
        let track = D2D1_ELLIPSE {
            point: center,
            radiusX: 23.0,
            radiusY: 23.0,
        };
        // SAFETY: the brush is renderer-owned and its opacity is restored before returning.
        unsafe {
            self.brushes.track.SetOpacity(opacity);
            self.target
                .DrawEllipse(&track, &self.brushes.track, 3.0, None);
            self.brushes.track.SetOpacity(1.0);
        }

        let (label, remaining, color) = state.snapshot.as_ref().map_or_else(
            || ("--".to_owned(), 0.0, QuotaColor::Unknown),
            |snapshot| {
                let remaining = snapshot.primary.remaining_percent();
                (
                    format!("{remaining:.0}"),
                    remaining,
                    snapshot.primary.color(),
                )
            },
        );
        if remaining > 0.0 {
            let brush = self.brush_for(color);
            // SAFETY: the brush is renderer-owned and its opacity is restored after the draw.
            unsafe { brush.SetOpacity(opacity) };
            let result = self.draw_progress_arc(center, 23.0, remaining / 100.0, brush);
            // SAFETY: restores the shared brush even if geometry creation failed.
            unsafe { brush.SetOpacity(1.0) };
            result?;
        }
        self.draw_text_with_opacity(
            &label,
            &self.percent_format,
            &self.brushes.primary_text,
            D2D_RECT_F {
                left: center.X - 24.0,
                top: center.Y - 13.0,
                right: center.X + 24.0,
                bottom: center.Y + 13.0,
            },
            opacity,
        );
        Ok(())
    }

    fn draw_panel(&self, state: &AppState) {
        let scale = self.dpi as f32 / 96.0;
        self.draw_rounded_background(
            D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: self.surface.width as f32 / scale,
                bottom: self.surface.height as f32 / scale,
            },
            16.0,
        );
        self.draw_panel_content(state, 1.0);
    }

    fn draw_transition(
        &self,
        state: &AppState,
        transition: TransitionVisual,
    ) -> Result<(), AppError> {
        let radius = 28.0 + (16.0 - 28.0) * transition.expansion.clamp(0.0, 1.0);
        let shape = D2D_RECT_F {
            left: transition.shape_bounds.0,
            top: transition.shape_bounds.1,
            right: transition.shape_bounds.2,
            bottom: transition.shape_bounds.3,
        };
        self.draw_rounded_background(shape, radius);
        if transition.ball_opacity > 0.0 {
            self.draw_ball_content(
                state,
                Vector2 {
                    X: transition.ball_center.0,
                    Y: transition.ball_center.1,
                },
                transition.ball_opacity,
            )?;
        }
        if transition.panel_opacity > 0.0 {
            // SAFETY: the finite clip is popped before this draw call returns.
            unsafe {
                self.target
                    .PushAxisAlignedClip(&shape, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            self.draw_panel_content(state, transition.panel_opacity);
            // SAFETY: balances the PushAxisAlignedClip immediately above.
            unsafe { self.target.PopAxisAlignedClip() };
        }
        Ok(())
    }

    fn draw_rounded_background(&self, bounds: D2D_RECT_F, radius: f32) {
        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bounds.left + 0.5,
                top: bounds.top + 0.5,
                right: bounds.right - 0.5,
                bottom: bounds.bottom - 0.5,
            },
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: the rounded rectangle and brush are valid for this immediate call.
        unsafe {
            self.target
                .FillRoundedRectangle(&panel, &self.brushes.background);
        };
    }

    fn draw_panel_content(&self, state: &AppState, opacity: f32) {
        let scale = self.dpi as f32 / 96.0;
        let width = self.surface.width as f32 / scale;

        let (status, plan_type) = panel_title(state);
        self.draw_text_with_opacity(
            status,
            &self.title_format,
            &self.brushes.primary_text,
            D2D_RECT_F {
                left: 18.0,
                top: 14.0,
                right: if plan_type.is_some() {
                    64.0
                } else {
                    width - 18.0
                },
                bottom: 36.0,
            },
            opacity,
        );
        if let Some(plan_type) = plan_type {
            self.draw_text_with_opacity(
                plan_type_label(plan_type),
                &self.title_format,
                self.brush_for_plan_type(plan_type),
                D2D_RECT_F {
                    left: 64.0,
                    top: 14.0,
                    right: width - 18.0,
                    bottom: 36.0,
                },
                opacity,
            );
        }

        let (five_hour, weekly) = state
            .snapshot
            .as_ref()
            .map_or((None, None), classify_quota_windows);
        self.draw_quota_row("5h额度", five_hour, 42.0, width, opacity);
        self.draw_quota_row("周额度", weekly, 69.0, width, opacity);
        self.draw_token_usage_row("今日使用", state.today_tokens, 96.0, width, opacity);
        self.draw_token_usage_row("累计使用", state.lifetime_tokens, 123.0, width, opacity);
        self.draw_update_row(state, 152.0, width, opacity);
    }

    fn draw_quota_row(
        &self,
        label: &str,
        window: Option<&QuotaWindow>,
        top: f32,
        width: f32,
        opacity: f32,
    ) {
        self.draw_text_with_opacity(
            label,
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: 94.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        let Some(window) = window else {
            self.draw_text_with_opacity(
                "--",
                &self.body_format,
                &self.brushes.unknown,
                D2D_RECT_F {
                    left: TIME_COLUMN_LEFT,
                    top,
                    right: width - 18.0,
                    bottom: top + 23.0,
                },
                opacity,
            );
            return;
        };
        self.draw_text_with_opacity(
            &format!("{:.0}%", window.remaining_percent()),
            &self.body_format,
            self.brush_for(window.color()),
            D2D_RECT_F {
                left: 96.0,
                top,
                right: 140.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        self.draw_text_with_opacity(
            &format_local_timestamp(window.resets_at),
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    fn draw_update_row(&self, state: &AppState, top: f32, width: f32, opacity: f32) {
        self.draw_text_with_opacity(
            "更新时间",
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: 94.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        let now = SystemTime::now();
        let value = state.snapshot.as_ref().map_or_else(
            || "--".to_owned(),
            |snapshot| {
                let timestamp = format_local_timestamp(snapshot.received_at);
                if state.is_stale(now) {
                    format!("{timestamp} · 已过期")
                } else {
                    timestamp
                }
            },
        );
        self.draw_text_with_opacity(
            &value,
            &self.body_format,
            if state.is_stale(now) {
                &self.brushes.yellow
            } else {
                &self.brushes.secondary_text
            },
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    fn draw_token_usage_row(
        &self,
        label: &str,
        tokens: Option<u64>,
        top: f32,
        width: f32,
        opacity: f32,
    ) {
        self.draw_text_with_opacity(
            label,
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: TIME_COLUMN_LEFT,
                bottom: top + 23.0,
            },
            opacity,
        );
        self.draw_text_with_opacity(
            &format_token_usage(tokens),
            &self.body_format,
            if tokens.is_some() {
                &self.brushes.secondary_text
            } else {
                &self.brushes.unknown
            },
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    fn draw_progress_arc(
        &self,
        center: Vector2,
        radius: f32,
        fraction: f64,
        brush: &ID2D1SolidColorBrush,
    ) -> Result<(), AppError> {
        if fraction >= 0.999 {
            let ellipse = D2D1_ELLIPSE {
                point: center,
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: immediate draw with valid geometry and renderer-owned brush.
            unsafe { self.target.DrawEllipse(&ellipse, brush, 3.0, None) };
            return Ok(());
        }

        let angle = std::f64::consts::TAU * fraction;
        let start = Vector2 {
            X: center.X,
            Y: center.Y - radius,
        };
        let end = Vector2 {
            X: center.X + (angle.sin() as f32 * radius),
            Y: center.Y - (angle.cos() as f32 * radius),
        };
        // SAFETY: factory, geometry, and sink are UI-thread COM objects. The sink is closed before
        // the geometry is drawn, and all stack geometry data outlives its immediate COM call.
        unsafe {
            let geometry = self.factory.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            sink.BeginFigure(start, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: end,
                size: D2D_SIZE_F {
                    width: radius,
                    height: radius,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if fraction > 0.5 {
                    D2D1_ARC_SIZE_LARGE
                } else {
                    D2D1_ARC_SIZE_SMALL
                },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            self.target.DrawGeometry(&geometry, brush, 3.0, None);
        }
        Ok(())
    }

    fn draw_text_with_opacity(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
        bounds: D2D_RECT_F,
        opacity: f32,
    ) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: UTF-16 storage and layout rectangle remain alive for the synchronous draw call;
        // the renderer-owned brush opacity is restored immediately afterward.
        unsafe {
            brush.SetOpacity(opacity.clamp(0.0, 1.0));
            self.target.DrawText(
                &utf16,
                format,
                &bounds,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            brush.SetOpacity(1.0);
        }
    }

    fn brush_for(&self, color: QuotaColor) -> &ID2D1SolidColorBrush {
        match color {
            QuotaColor::Healthy => &self.brushes.green,
            QuotaColor::Warning => &self.brushes.yellow,
            QuotaColor::Critical => &self.brushes.red,
            QuotaColor::Unknown => &self.brushes.unknown,
        }
    }

    fn brush_for_plan_type(&self, plan_type: &str) -> &ID2D1SolidColorBrush {
        match plan_type_color(plan_type) {
            PlanColor::Neutral => &self.brushes.plan_neutral,
            PlanColor::Plus => &self.brushes.plan_plus,
            PlanColor::Pro => &self.brushes.plan_pro,
            PlanColor::Business => &self.brushes.plan_business,
            PlanColor::Enterprise => &self.brushes.plan_enterprise,
            PlanColor::Edu => &self.brushes.plan_edu,
            PlanColor::Unknown => &self.brushes.unknown,
        }
    }
}

fn create_target(factory: &ID2D1Factory, dpi: u32) -> Result<ID2D1DCRenderTarget, AppError> {
    let properties = D2D1_RENDER_TARGET_PROPERTIES {
        // This surface is tiny and updated only during short transitions. A software DC target
        // avoids loading a vendor D3D driver stack whose idle memory dwarfs the bitmap itself.
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: dpi as f32,
        dpiY: dpi as f32,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    // SAFETY: properties is valid for the duration of the factory call.
    unsafe { factory.CreateDCRenderTarget(&properties) }.map_err(AppError::from)
}

fn create_brushes(target: &ID2D1DCRenderTarget) -> Result<Brushes, AppError> {
    let render_target: ID2D1RenderTarget = target.cast()?;
    let brush = |color: &D2D1_COLOR_F| {
        // SAFETY: color is borrowed only for this synchronous factory call.
        unsafe { render_target.CreateSolidColorBrush(color, None) }
    };
    Ok(Brushes {
        background: brush(&BACKGROUND)?,
        track: brush(&TRACK)?,
        primary_text: brush(&PRIMARY_TEXT)?,
        secondary_text: brush(&SECONDARY_TEXT)?,
        green: brush(&GREEN)?,
        yellow: brush(&YELLOW)?,
        red: brush(&RED)?,
        unknown: brush(&UNKNOWN)?,
        plan_neutral: brush(&PLAN_NEUTRAL)?,
        plan_plus: brush(&PLAN_PLUS)?,
        plan_pro: brush(&PLAN_PRO)?,
        plan_business: brush(&PLAN_BUSINESS)?,
        plan_enterprise: brush(&PLAN_ENTERPRISE)?,
        plan_edu: brush(&PLAN_EDU)?,
    })
}

fn create_text_format(
    factory: &IDWriteFactory,
    size: f32,
    centered: bool,
) -> Result<IDWriteTextFormat, AppError> {
    let variable = wide("Segoe UI Variable");
    let fallback = wide("Segoe UI");
    let locale = wide("zh-CN");
    // SAFETY: all PCWSTR values reference NUL-terminated vectors that live through each call.
    let format = unsafe {
        factory.CreateTextFormat(
            PCWSTR(variable.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )
    }
    .or_else(|_| unsafe {
        factory.CreateTextFormat(
            PCWSTR(fallback.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )
    })?;
    // SAFETY: alignment setters mutate only this format COM object on the UI thread.
    unsafe {
        format.SetTextAlignment(if centered {
            DWRITE_TEXT_ALIGNMENT_CENTER
        } else {
            DWRITE_TEXT_ALIGNMENT_LEADING
        })?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }
    Ok(format)
}

impl DibSurface {
    fn new(width: i32, height: i32) -> Result<Self, AppError> {
        // SAFETY: a memory DC has no borrowed window lifetime and is released in Drop.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(AppError::Windows("无法创建内存绘图上下文".to_owned()));
        }
        let mut bits = ptr::null_mut::<c_void>();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())
                    .map_err(|_| AppError::Render("位图头大小溢出".to_owned()))?,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: info and output pointer are valid. No section handle is supplied; GDI owns the
        // allocation until bitmap deletion. The returned bits pointer remains valid while selected.
        let bitmap =
            unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
        if bits.is_null() {
            // SAFETY: these objects were successfully created by this function and are not shared.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
            }
            return Err(AppError::Render("无法映射分层窗口位图".to_owned()));
        }
        // SAFETY: bitmap is a valid GDI bitmap and dc is a compatible memory DC.
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
        })
    }

    fn clear(&self) {
        let byte_len = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .map(|height| width * height * 4)
            })
            .unwrap_or(0);
        // SAFETY: bits points to this surface's width*height*4-byte DIB allocation.
        unsafe { ptr::write_bytes(self.bits, 0, byte_len) };
    }

    fn commit(&self, hwnd: HWND, destination: POINT) -> Result<(), AppError> {
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: the source DC contains a selected 32-bit premultiplied BGRA DIB of the stated size.
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&destination),
                Some(&size),
                Some(self.dc),
                Some(&source),
                Default::default(),
                Some(&blend),
                ULW_ALPHA,
            )
        }?;
        Ok(())
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        // SAFETY: the old object is restored before deleting the selected bitmap and its memory DC.
        unsafe {
            let _ = SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
