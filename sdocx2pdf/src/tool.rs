use std::{borrow::Borrow, rc::Rc};

use itertools::{Either, Itertools};
use krilla::{
    color::rgb,
    geom::{Path, PathBuilder},
    num::NormalizedF32,
    paint,
    surface::Surface,
};
use lerp::Lerp;
use log::warn;
use ordered_float::OrderedFloat;
use sdocx::page::object::stroke::{Event, LineType, Stroke};
use thiserror::Error;

use crate::{
    pdf::{self, Point2d, Vector2d},
    stroke::{ContinuousStroke, FilteringMode, StrokeOrDot},
};

/// A group of stroke events.
pub enum EventGroup<'e> {
    /// A group of events from a normal stroke (e.g. handwriting).
    Normal(&'e [Event]),

    /// A group of events from a shapeified stroke (e.g. a straight line created by pausing at the
    /// end of a freehand line).
    Shapeified(&'e [Event]),
}

impl<'e> EventGroup<'e> {
    pub fn from_stroke(stroke: &'e Stroke) -> EventGroup<'e> {
        let ev = stroke.events();

        if stroke.is_shapeified() {
            EventGroup::Shapeified(ev)
        } else {
            EventGroup::Normal(ev)
        }
    }

    fn events(&self) -> &'e [Event] {
        match self {
            EventGroup::Normal(events) | EventGroup::Shapeified(events) => events,
        }
    }
}

/// Basic information used by all tools.
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub struct Basics {
    // The app is consistent in how it computes this, in that selecting a particular pen size from
    // the discrete scale in the app will produce the same floating-point size value. Thus, it
    // makes sense for this to be an `OrderedFloat` and therefore hashable.
    //
    // It still makes sense to hash this after multiplication by a constant as long as the hash is
    // used for comparisons involving other tools whose widths were multiplied by the same
    // constant.
    size: OrderedFloat<f32>,

    colour_bgra: [u8; 4],
    line_type: LineType,
}

impl Basics {
    fn with_scaled_width(self, mul: f32) -> Basics {
        Basics {
            size: self.size * mul,
            ..self
        }
    }
}

/// Options affecting the way tools are drawn.
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub struct ToolOptions {
    /// Multiplier for the widths of the handwriting pens.
    pub pen_width_mul: OrderedFloat<f32>,

    /// Multiplier for the widths of the highlighters and marker pens.
    pub marker_width_mul: OrderedFloat<f32>,

    /// The angle of the calligraphy pen's nib edge, measured anticlockwise from the horizontal as
    /// the page is seen by the reader, in degrees.
    pub nib_angle_deg: OrderedFloat<f32>,
}

/// The length of the calligraphy pen's nib edge, as a multiple of the pen size.
///
/// This is derived from Samsung's implementation (see `libSPenObliquePen.so`): the stroke body is
/// a ribbon whose edges are offset from the centreline by a fixed diagonal vector of length equal
/// to the pen size, making the widest strokes (those perpendicular to the nib edge) `sqrt(2)`
/// times the pen size wide.
///
/// A decompiled Samsung Notes APK with these libraries is at
/// <https://github.com/Yanndroid/Samsung-Notes-Port> (each preload pen has its own
/// `libSPen<PenName>.so`). The ribbon construction is in
/// `SPen::ObliquePenStrokeDrawableRTV1::lineToMesh`: the four segment corners are the endpoints
/// offset by `(±penSize / 2, ∓penSize / 2)`, widened by miter-style skirts. The stroke direction
/// is the only input to the width; pressure, tilt and orientation are used only by the end-cap
/// (`ObliquePenStrokeTipDrawableRT`).
const NIB_LENGTH_PER_PEN_SIZE: f64 = 1.35;

/// The thickness of the calligraphy pen's nib, as a fraction of the length of its edge. This is
/// what stops strokes drawn along the nib angle from disappearing entirely. Samsung's ribbon has
/// zero ideal width in that direction, but the miter skirts of its mesh keep a minimum across-
/// width of roughly 0.28 times half the pen size, so this is set to reproduce that floor.
const NIB_THICKNESS_RATIO: f64 = 0.22;

/// The shape and orientation of the chisel-shaped nib of a calligraphy pen.
///
/// The nib is modelled as Samsung models it: a line segment (the nib edge) held at a fixed angle,
/// whose sweep along the stroke is the mark (see [`append_events_chisel`]). The nib thickness only
/// sets a minimum stroke width, so that strokes drawn along the nib edge don't vanish entirely.
#[derive(Clone, Copy)]
struct Nib {
    /// Unit vector along the nib edge, in document space.
    edge_direction: Vector2d,

    /// Half the length of the nib edge.
    half_length: f64,

    /// Half the thickness of the nib.
    half_thickness: f64,

    /// The pen size, as `SmPath`'s divisor table reads it (`getSize()`).
    size: f64,
}

impl Nib {
    fn new(angle_deg: f64, pen_size: f64) -> Nib {
        let angle = angle_deg.to_radians();
        let half_length = 0.5 * NIB_LENGTH_PER_PEN_SIZE * pen_size;

        Nib {
            // The y axis of document space points down the page, so an angle measured
            // anticlockwise as the reader sees it is clockwise in document space.
            edge_direction: Vector2d::new(angle.cos(), -angle.sin()),
            half_length,
            half_thickness: half_length * NIB_THICKNESS_RATIO,
            size: pen_size,
        }
    }

    /// Appends the mark left by the nib resting at `centre` to `path_builder`. This is a
    /// parallelogram shaped like the nib itself: a line along the nib edge, thickened by the nib
    /// thickness.
    fn append_mark(&self, centre: Point2d, path_builder: &mut PathBuilder) {
        let half_edge = self.edge_direction * self.half_length;

        append_simple_line(
            [
                (centre - half_edge, self.half_thickness),
                (centre + half_edge, self.half_thickness),
            ],
            true,
            path_builder,
        );
    }
}

/// How the width of the mark made by a tool is determined at a point along a stroke.
#[derive(Clone, Copy)]
enum Spread {
    /// A round nib, whose radius is determined by the pressure (or by `pressure_override` in place
    /// of it, for tools that ignore pressure).
    Round {
        pen_size: f64,
        pressure_override: Option<f64>,
    },
}

impl Spread {
    /// Returns the half-width of the mark made by the tool, measured across the stroke, when it is
    /// moved in the direction of the unit vector `tangent` under pressure `pressure`.
    fn half_width_across(&self, pressure: f64, _tangent: Vector2d) -> f64 {
        let Spread::Round {
            pen_size,
            pressure_override,
        } = *self;

        pressure_to_circle_radius(pressure_override.unwrap_or(pressure), pen_size)
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum Tool {
    FountainPen(Basics),
    CalligraphyPen {
        basics: Basics,
        /// Angle of the nib edge in degrees. See [`ToolOptions::nib_angle_deg`].
        nib_angle_deg: OrderedFloat<f32>,
    },
    InkPen { basics: Basics, fixed_width: bool },
    Pencil { basics: Basics, fixed_opacity: bool },
    CalligraphyBrush(Basics),
    Highlighter { basics: Basics, straight: bool },
    Marker { basics: Basics, straight: bool },
    Tape(Basics),
}

#[derive(Error, Debug)]
enum ToolPropertyError {
    #[error("missing name")]
    NoName,

    #[error("unknown name '{0}'")]
    UnknownName(Rc<str>),

    #[error("missing size")]
    NoSize,
}

impl Tool {
    fn try_for_stroke(stroke: &Stroke, options: ToolOptions) -> Result<Tool, ToolPropertyError> {
        let name = stroke.pen_name().ok_or(ToolPropertyError::NoName)?;

        let basics = Basics {
            size: stroke.pen_size().ok_or(ToolPropertyError::NoSize)?.into(),
            colour_bgra: stroke.colour(),
            line_type: stroke.line_type(),
        };

        Ok(match name.as_ref() {
            "com.samsung.android.sdk.pen.pen.preload.FountainPen" => Tool::FountainPen(basics),
            "com.samsung.android.sdk.pen.pen.preload.ObliquePen" => Tool::CalligraphyPen {
                basics,
                nib_angle_deg: options.nib_angle_deg,
            },
            "com.samsung.android.sdk.pen.pen.preload.InkPen2" => Tool::InkPen {
                basics,
                fixed_width: stroke.is_fixed_width(),
            },
            "com.samsung.android.sdk.pen.pen.preload.Pencil2" => Tool::Pencil {
                basics,
                fixed_opacity: stroke.is_fixed_opacity(),
            },
            "com.samsung.android.sdk.pen.pen.preload.BrushPen" => Tool::CalligraphyBrush(basics),
            "com.samsung.android.sdk.pen.pen.preload.Marker4" => Tool::Highlighter {
                basics,
                straight: false,
            },
            "com.samsung.android.sdk.pen.pen.preload.StraightHighlighter" => Tool::Highlighter {
                basics,
                straight: true,
            },
            "com.samsung.android.sdk.pen.pen.preload.Marker3" => Tool::Marker {
                basics,
                straight: false,
            },
            "com.samsung.android.sdk.pen.pen.preload.StraightMarker" => Tool::Marker {
                basics,
                straight: true,
            },
            "com.samsung.android.sdk.pen.pen.preload.TapePen" => Tool::Tape(basics),
            _ => return Err(ToolPropertyError::UnknownName(Rc::clone(name))),
        })
    }

    pub fn for_stroke(stroke: &Stroke, options: ToolOptions) -> Tool {
        Self::try_for_stroke(stroke, options)
            .unwrap()
            .with_scaled_width(options)
    }

    fn with_scaled_width(mut self, options: ToolOptions) -> Tool {
        let like_hl = self.is_like_highlighter();
        let basics = self.basics_mut();

        *basics = if like_hl {
            basics.with_scaled_width(options.marker_width_mul.0)
        } else {
            basics.with_scaled_width(options.pen_width_mul.0)
        };

        self
    }

    fn basics(&self) -> &Basics {
        match self {
            Tool::FountainPen(basics)
            | Tool::CalligraphyPen { basics, .. }
            | Tool::InkPen { basics, .. }
            | Tool::Pencil { basics, .. }
            | Tool::CalligraphyBrush(basics)
            | Tool::Highlighter { basics, .. }
            | Tool::Marker { basics, .. }
            | Tool::Tape(basics) => basics,
        }
    }

    fn basics_mut(&mut self) -> &mut Basics {
        match self {
            Tool::FountainPen(basics)
            | Tool::CalligraphyPen { basics, .. }
            | Tool::InkPen { basics, .. }
            | Tool::Pencil { basics, .. }
            | Tool::CalligraphyBrush(basics)
            | Tool::Highlighter { basics, .. }
            | Tool::Marker { basics, .. }
            | Tool::Tape(basics) => basics,
        }
    }

    /// Returns whether this tool is a highlighter or related tool which is typically used for
    /// drawing over existing text.
    fn is_like_highlighter(&self) -> bool {
        // !!! - Do not change the tool classification without updating the help text for the width
        // multipliers.
        matches!(self, Tool::Highlighter { .. } | Tool::Marker { .. })
    }

    fn is_pressure_sensitive(&self) -> bool {
        matches!(
            self,
            Tool::FountainPen(_)
                | Tool::InkPen {
                    fixed_width: false,
                    ..
                }
                | Tool::Pencil {
                    fixed_opacity: false,
                    ..
                }
        )
    }

    /// Returns whether strokes drawn using this tool are always straight.
    fn is_straight_only(&self) -> bool {
        matches!(
            self,
            Tool::Highlighter { straight: true, .. } | Tool::Marker { straight: true, .. }
        )
    }

    fn draw_paths<'p>(
        &self,
        paths: impl IntoIterator<Item = impl Borrow<Path> + 'p>,
        fill_boundary: &Path,
        surface: &mut Surface<'_>,
    ) {
        let &Basics {
            size: OrderedFloat(size),
            colour_bgra: [b, g, r, a],
            line_type,
        } = self.basics();

        if !line_type.is_continuous() {
            warn!("Dashed and dotted lines are not yet supported; will be drawn solid");
        }

        // todo: Soft masks for pencil and calligraphy brush
        // todo: Allow dashed/dotted lines

        if self.is_like_highlighter() {
            surface.push_blend_mode(krilla::blend::BlendMode::Multiply);
        }

        let paint: paint::Paint = rgb::Color::new(r, g, b).into();
        let opacity = NormalizedF32::new(a as f32 / 255.0).unwrap();
        let is_opaque = a == 255;

        if self.is_straight_only() {
            // Will be drawn as a single path consisting of straight line segments, so we don't
            // need to fill anything.
            surface.set_fill(None);
            surface.set_stroke(Some(paint::Stroke {
                paint,
                width: size,
                line_cap: krilla::paint::LineCap::Round,
                opacity,
                ..paint::Stroke::default()
            }));

            for path in paths {
                surface.draw_path(path.borrow());
            }

            if self.is_like_highlighter() {
                // Pop blend mode
                surface.pop();
            }

            return;
        }

        surface.set_stroke(None);
        surface.set_fill(Some(paint::Fill {
            paint,
            opacity,
            rule: paint::FillRule::NonZero,
        }));

        for path in paths {
            // Self-intersecting strokes from translucent tools should not blend on top of
            // themselves. To achieve this effect, we can use the stroke path to clip a translucent
            // rectangle (that is guaranteed to cover the area containing the stroke) rather than
            // filling the path directly. Non-zero filling doesn't care about the
            // self-intersections in the clipping path, and since there is only ever one path
            // filled - the rectangle - we end up with the self-intersections being invisible in
            // the result.
            //
            // This method works for opaque paths as well, so in theory we could use it as our only
            // mode of drawing. However, some PDF viewers optimise the two modes differently. When
            // not zoomed in closely, Firefox (pdf.js) appears to disable antialiasing for clipping
            // paths for performance, and the result is that strokes drawn in that way do not look
            // as good as ones filled directly. For translucent strokes we don't really have a
            // choice, but for opaque strokes we do, and so we fill the latter directly.
            if is_opaque {
                surface.draw_path(path.borrow());
            } else {
                surface.push_clip_path(path.borrow(), &paint::FillRule::NonZero);
                surface.draw_path(fill_boundary);
                surface.pop();
            }
        }

        if self.is_like_highlighter() {
            // Pop blend mode
            surface.pop();
        }
    }

    /// Extends `ops` with the necessary operations to draw each slice of stroke events in
    /// `strokes` using this tool. The strokes are drawn in order. Some strokes are turned into
    /// clipping paths and the actual drawing is done by filling `fill_boundary` with the required
    /// colour. Thus, `fill_boundary` should enclose area in which the strokes must be visible
    /// (the boundary of the whole page is a good choice).
    pub fn draw_events<'e>(
        &self,
        strokes: impl IntoIterator<Item = EventGroup<'e>>,
        fill_boundary: &Path,
        surface: &mut Surface<'_>,
    ) -> Result<(), ()> {
        if matches!(self, Tool::Tape(_)) {
            warn!("Tape is not yet fully supported and will likely look incorrect");
        }

        let &Basics {
            size: OrderedFloat(size),
            line_type,
            ..
        } = self.basics();

        if !line_type.is_continuous() {
            warn!("Dashed and dotted lines are not yet supported; will be drawn solid");
        }

        let mut paths: Vec<Path> = Vec::new();

        if self.is_straight_only() {
            for events in strokes {
                let mut pb = PathBuilder::new();
                append_events_straight(events, &mut pb);
                paths.push(pb.finish().unwrap());
            }
        } else {
            // Increase the width for non-straight highlighters/markers.
            let size = if self.is_like_highlighter() {
                f64::from(size) * 2.5
            } else {
                f64::from(size)
            };

            // The calligraphy pen has a chisel-shaped nib held at a fixed angle, so its width
            // comes from the direction of travel rather than from the pressure. Every other
            // non-straight tool has a round nib.
            let nib = match *self {
                Tool::CalligraphyPen { nib_angle_deg, .. } => {
                    Some(Nib::new(nib_angle_deg.0.into(), size))
                }
                _ => None,
            };

            let spread = Spread::Round {
                pen_size: size,
                pressure_override: (!self.is_pressure_sensitive()).then_some(0.45),
            };

            let mut bodies: Vec<Path> = Vec::new();

            for events in strokes {
                let mut pb = PathBuilder::new();

                if let Some(nib) = nib {
                    append_events_chisel(events, nib, &mut pb);
                } else {
                    append_events_basic(events, spread, ArcMode::All, f64::to_radians(40.0), &mut pb);
                }

                if let Some(path) = pb.finish() {
                    bodies.push(path);
                }
            }

            paths.append(&mut bodies);
        }

        self.draw_paths(&paths, fill_boundary, surface);

        Ok(())
    }
}

fn bezier_arc_control_points(a: Point2d, b: Point2d, centre: Point2d) -> Option<[Point2d; 2]> {
    // fixme: This
    let x1 = a.x;
    let y1 = a.y;
    let x4 = b.x;
    let y4 = b.y;
    let xc = centre.x;
    let yc = centre.y;
    let ax = x1 - xc;
    let ay = y1 - yc;
    let bx = x4 - xc;
    let by = y4 - yc;
    let q1 = ax * ax + ay * ay;
    let q2 = q1 + ax * bx + ay * by;
    let k2 = (((q1 * q2 * 2.0).sqrt() - q2) * 4.0) / ((ax * by - ay * bx) * 3.0);
    let x2 = xc + ax - k2 * ay;
    let y2 = yc + ay + k2 * ax;
    let x3 = xc + bx + k2 * by;
    let y3 = yc + by - k2 * bx;

    if !x2.is_finite() || !y2.is_finite() || !x3.is_finite() || !y3.is_finite() {
        return None;
    }

    Some([(x2, y2).into(), (x3, y3).into()])
}

/// Returns the control points `(p1, p2)` for a cubic Bézier from `p0` to `p3` that passes through
/// `a` at `t = 1/3` and `b` at `t = 2/3`.
fn bezier_control_pts_for_intersections(
    p0: Point2d,
    p3: Point2d,
    a: Point2d,
    b: Point2d,
) -> (Point2d, Point2d) {
    let (p0, p3, a, b) = (p0.to_vector(), p3.to_vector(), a.to_vector(), b.to_vector());

    // Solution to the linear system formed by `f(1/3) = a` and `f(2/3) = b` where f is the
    // function describing the cubic Bézier from `p0` to `p3`.
    let p2 = (b * 18.0 - a * 9.0 + p0 * 2.0 - p3 * 5.0) / 6.0;
    let p1 = (a * 27.0 - p0 * 8.0 - p3 - p2 * 6.0) / 12.0;

    (p1.to_point(), p2.to_point())
}

fn pressure_to_circle_radius(pressure: f64, pen_size: f64) -> f64 {
    0.5 * pen_size * pressure.clamp(0.4, 0.7)
}

fn append_bezier_pulley(
    points_tangents_radii: [(Point2d, Vector2d, f64); 4],
    arcs: SegmentArcs,
    last_segment_tan: &mut Option<Vector2d>,
    path_builder: &mut PathBuilder,
) -> Result<(), ()> {
    let [
        (start_pos, start_tangent, start_spread),
        (pos_first_third, tangent_first_third, spread_first_third),
        (pos_second_third, tangent_second_third, spread_second_third),
        (end_pos, end_tangent, end_spread),
    ] = points_tangents_radii;

    let (
        scaled_tangent_start,
        scaled_tangent_first_third,
        scaled_tangent_second_third,
        scaled_tangent_end,
    ) = (
        start_tangent * start_spread,
        tangent_first_third * spread_first_third,
        tangent_second_third * spread_second_third,
        end_tangent * end_spread,
    );

    if !scaled_tangent_start.is_finite()
        || !scaled_tangent_first_third.is_finite()
        || !scaled_tangent_second_third.is_finite()
        || !scaled_tangent_end.is_finite()
    {
        return Err(());
    }

    let start_to_first_third = (pos_first_third - start_pos).normalize();

    // If the computed start direction is in opposition to the direction to the first third
    // position, use the latter for the tangent so we aren't going back on ourselves.
    // `start_to_first_third` should be finite, but we check the partial ordering so that if it is
    // not, we retain the current finite tangent.
    let scaled_tangent_start = if let Some(std::cmp::Ordering::Less) = scaled_tangent_start
        .dot(start_to_first_third)
        .partial_cmp(&0.0)
    {
        start_to_first_third * start_spread
    } else {
        scaled_tangent_start
    };

    let bottom_left = start_pos + Vector2d::new(-scaled_tangent_start.y, scaled_tangent_start.x);

    let bottom_right = start_pos + Vector2d::new(scaled_tangent_start.y, -scaled_tangent_start.x);

    let lower_mid_left = pos_first_third
        + Vector2d::new(-scaled_tangent_first_third.y, scaled_tangent_first_third.x);

    let lower_mid_right = pos_first_third
        + Vector2d::new(scaled_tangent_first_third.y, -scaled_tangent_first_third.x);

    let upper_mid_left = pos_second_third
        + Vector2d::new(
            -scaled_tangent_second_third.y,
            scaled_tangent_second_third.x,
        );

    let upper_mid_right = pos_second_third
        + Vector2d::new(
            scaled_tangent_second_third.y,
            -scaled_tangent_second_third.x,
        );

    let top_left = end_pos + Vector2d::new(-scaled_tangent_end.y, scaled_tangent_end.x);

    let top_right = end_pos + Vector2d::new(scaled_tangent_end.y, -scaled_tangent_end.x);

    let (cp_lower_mid_left, cp_upper_mid_left) =
        bezier_control_pts_for_intersections(bottom_left, top_left, lower_mid_left, upper_mid_left);

    let (cp_upper_mid_right, cp_lower_mid_right) = bezier_control_pts_for_intersections(
        top_right,
        bottom_right,
        upper_mid_right,
        lower_mid_right,
    );

    use pdf::PolygonPoint::{Control, Normal};

    let top_to_bottom_left_points = [
        Normal(top_left),
        Control(cp_upper_mid_left),
        Control(cp_lower_mid_left),
        Normal(bottom_left),
    ];

    let bottom_to_top_right_points = [
        Normal(bottom_right),
        Control(cp_lower_mid_right),
        Control(cp_upper_mid_right),
        Normal(top_right),
    ];

    let bottom_arc_lowest = start_pos - scaled_tangent_start;
    let top_arc_highest = end_pos + scaled_tangent_end;

    // The arc that rounds off the end of the segment, if we want one. Without it, the outline runs
    // straight from one side of the segment to the other, giving a flat end.
    let end_arc = if arcs.end {
        let top_right_to_arc_highest_cps =
            bezier_arc_control_points(top_right, top_arc_highest, end_pos).ok_or(())?;
        let top_arc_highest_to_left_cps =
            bezier_arc_control_points(top_arc_highest, top_left, end_pos).ok_or(())?;

        Some(
            top_right_to_arc_highest_cps
                .map(Control)
                .into_iter()
                .chain(std::iter::once(Normal(top_arc_highest)))
                .chain(top_arc_highest_to_left_cps.map(Control)),
        )
    } else {
        None
    };

    // Whether to round off the beginning of the segment. We do so if there is a significant
    // difference in tangent angle between the final third of the previous segment and the start of
    // this one, because that makes the connection look cleaner, or if this is the first segment
    // and the caller wants the stroke to begin with a round end.
    let want_start_arc = arcs.start
        && last_segment_tan.is_none_or(|tangent| {
            tangent.angle_to(scaled_tangent_start).radians.abs() > f64::to_radians(20.0)
        });

    let start_arc = if want_start_arc {
        let bottom_left_to_arc_lowest_cps =
            bezier_arc_control_points(bottom_left, bottom_arc_lowest, start_pos).ok_or(())?;
        let bottom_arc_lowest_to_right_cps =
            bezier_arc_control_points(bottom_arc_lowest, bottom_right, start_pos).ok_or(())?;

        Some(
            bottom_left_to_arc_lowest_cps
                .map(Control)
                .into_iter()
                .chain(std::iter::once(Normal(bottom_arc_lowest)))
                .chain(bottom_arc_lowest_to_right_cps.map(Control))
                .chain(std::iter::once(Normal(bottom_right))),
        )
    } else {
        None
    };

    *last_segment_tan = Some(end_pos - pos_second_third);

    pdf::specify_polygon(
        bottom_to_top_right_points
            .into_iter()
            .chain(end_arc.into_iter().flatten())
            .chain(top_to_bottom_left_points)
            .chain(start_arc.into_iter().flatten()),
        path_builder,
    );

    Ok(())
}

fn calc_pulley_line_points_acw_from_lower_right(
    c1: Point2d,
    r1: f64,
    c2: Point2d,
    r2: f64,
) -> Option<[Point2d; 4]> {
    let d = c1.distance_to(c2);

    if d == 0.0 {
        return None;
    }

    let alpha = (c2 - c1).angle_from_x_axis().radians;
    let beta = ((r1 - r2) / d).acos();

    if !beta.is_finite() {
        // (r1-r2)/d is not in [-1,1], i.e. one circle is inside the other.
        return None;
    }

    let (apb_s, apb_c) = (alpha + beta).sin_cos();
    let (amb_s, amb_c) = (alpha - beta).sin_cos();

    let apb = Vector2d::new(apb_c, apb_s);
    let amb = Vector2d::new(amb_c, amb_s);

    let right_start = c1 + amb * r1;
    let right_end = c2 + amb * r2;

    let left_start = c2 + apb * r2;
    let left_end = c1 + apb * r1;

    Some([right_start, right_end, left_start, left_end])
}

fn append_simple_pulley(
    [(a, radius_a), (b, radius_b)]: [(Point2d, f64); 2],
    use_arcs: bool,
    path_builder: &mut PathBuilder,
) -> Result<(), ()> {
    let [a_right, b_right, b_left, a_left] =
        calc_pulley_line_points_acw_from_lower_right(a, radius_a, b, radius_b).ok_or(())?;

    let direction = (b - a).normalize();

    if !direction.is_finite() {
        return Err(());
    }

    use pdf::PolygonPoint::{Control, Normal};

    let points = if use_arcs {
        let a_arc_midpoint = a - direction * radius_a;
        let b_arc_midpoint = b + direction * radius_b;

        let [b_right_arc_cp1, b_right_arc_cp2] =
            bezier_arc_control_points(b_right, b_arc_midpoint, b).ok_or(())?;
        let [b_left_arc_cp1, b_left_arc_cp2] =
            bezier_arc_control_points(b_arc_midpoint, b_left, b).ok_or(())?;
        let [a_left_arc_cp1, a_left_arc_cp2] =
            bezier_arc_control_points(a_left, a_arc_midpoint, a).ok_or(())?;
        let [a_right_arc_cp1, a_right_arc_cp2] =
            bezier_arc_control_points(a_arc_midpoint, a_right, a).ok_or(())?;

        Either::Left(
            [
                Normal(b_right),
                Control(b_right_arc_cp1),
                Control(b_right_arc_cp2),
                Normal(b_arc_midpoint),
                Control(b_left_arc_cp1),
                Control(b_left_arc_cp2),
                Normal(b_left),
                Normal(a_left),
                Control(a_left_arc_cp1),
                Control(a_left_arc_cp2),
                Normal(a_arc_midpoint),
                Control(a_right_arc_cp1),
                Control(a_right_arc_cp2),
                Normal(a_right),
            ]
            .into_iter(),
        )
    } else {
        Either::Right(
            [
                Normal(b_right),
                Normal(b_left),
                Normal(a_left),
                Normal(a_right),
            ]
            .into_iter(),
        )
    };

    pdf::specify_polygon(points, path_builder);
    Ok(())
}

fn append_simple_line(
    [(a, radius_a), (b, radius_b)]: [(Point2d, f64); 2],
    round_ends: bool,
    path_builder: &mut PathBuilder,
) {
    // if specify_only {
    use pdf::PolygonPoint::{Control, Normal};

    let forwards = (b - a).normalize();

    let left: Vector2d = (-forwards.y, forwards.x).into();
    let right = -left;

    if !forwards.is_finite() {
        if !round_ends {
            // A zero-length line with butt caps is invisible.
            return;
        }

        // The points are equal. We need to draw a circle, but as we are specifying it as a
        // path, we can only approximate it using Bézier curves.
        let radius = (radius_a + radius_b) / 2.0;

        let left = a + Vector2d::new(-radius, 0.0);
        let right = a + Vector2d::new(radius, 0.0);
        let top = a + Vector2d::new(0.0, radius);
        let bottom = a + Vector2d::new(0.0, -radius);

        // Calculate the control points for the arc in each quadrant.
        // todo: Precompute these for the unit circle and translate as needed instead of
        // calculating them every time.
        let [q1_c1, q1_c2] = bezier_arc_control_points(right, top, a).unwrap();
        let [q2_c1, q2_c2] = bezier_arc_control_points(top, left, a).unwrap();
        let [q3_c1, q3_c2] = bezier_arc_control_points(left, bottom, a).unwrap();
        let [q4_c1, q4_c2] = bezier_arc_control_points(bottom, right, a).unwrap();

        pdf::specify_polygon(
            [
                Normal(right),
                Control(q1_c1),
                Control(q1_c2),
                Normal(top),
                Control(q2_c1),
                Control(q2_c2),
                Normal(left),
                Control(q3_c1),
                Control(q3_c2),
                Normal(bottom),
                Control(q4_c1),
                Control(q4_c2),
                Normal(right),
            ],
            path_builder,
        );

        return;
    }

    let bottom_right = a + right * radius_a;
    let top_right = b + right * radius_b;
    let top_left = b + left * radius_b;
    let bottom_left = a + left * radius_a;

    if !round_ends {
        // A zero-length line with butt caps is invisible.
        if !forwards.is_finite() {
            return;
        }

        pdf::specify_polygon(
            [
                Normal(bottom_right),
                Normal(top_right),
                Normal(top_left),
                Normal(bottom_left),
            ],
            path_builder,
        );

        return;
    }

    // Non-zero length line with round ends. We could make this look like the normal case where
    // we take the mean radius, but since we have to draw a path instead of a line, we might as
    // well take advantage of the fact we can use different widths at the start and end.
    let bottom_arc_lowest = a - forwards * radius_a;
    let top_arc_highest = b + forwards * radius_b;

    let (
        [top_right_to_arc_highest_cp1, top_right_to_arc_highest_cp2],
        [top_arc_highest_to_left_cp1, top_arc_highest_to_left_cp2],
        [bot_left_to_arc_lowest_cp1, bot_left_to_arc_lowest_cp2],
        [bot_arc_lowest_to_right_cp1, bot_arc_lowest_to_right_cp2],
    ) = (
        bezier_arc_control_points(top_right, top_arc_highest, b).unwrap(),
        bezier_arc_control_points(top_arc_highest, top_left, b).unwrap(),
        bezier_arc_control_points(bottom_left, bottom_arc_lowest, a).unwrap(),
        bezier_arc_control_points(bottom_arc_lowest, bottom_right, a).unwrap(),
    );

    pdf::specify_polygon(
        [
            Normal(top_right),
            Control(top_right_to_arc_highest_cp1),
            Control(top_right_to_arc_highest_cp2),
            Normal(top_arc_highest),
            Control(top_arc_highest_to_left_cp1),
            Control(top_arc_highest_to_left_cp2),
            Normal(top_left),
            Normal(bottom_left),
            Control(bot_left_to_arc_lowest_cp1),
            Control(bot_left_to_arc_lowest_cp2),
            Normal(bottom_arc_lowest),
            Control(bot_arc_lowest_to_right_cp1),
            Control(bot_arc_lowest_to_right_cp2),
            Normal(bottom_right),
        ],
        path_builder,
    );
}

#[derive(Clone, Copy)]
enum ArcMode {
    /// Draw arcs only on the first and last segments of the stroke.
    #[expect(dead_code)]
    FirstLastOnly,

    /// Draw arcs on all segments.
    All,
}

/// Which ends of a stroke segment are to be rounded off with an arc.
#[derive(Clone, Copy)]
struct SegmentArcs {
    start: bool,
    end: bool,
}

impl SegmentArcs {
    fn for_segment(mode: ArcMode, position: itertools::Position) -> SegmentArcs {
        use itertools::Position::{First, Last, Only};

        let (start, end) = match mode {
            ArcMode::All => (true, true),

            ArcMode::FirstLastOnly => {
                let v = matches!(position, First | Last | Only);
                (v, v)
            }
        };

        SegmentArcs { start, end }
    }

    fn any(self) -> bool {
        self.start || self.end
    }
}

/// Draws `events` into `ops` using the basic unmodified stroke segmentation algorithm, with the
/// width of the mark at each point determined by `spread`.
///
/// The event positions must be in PDF space. It is assumed that the outline colour and line cap
/// style are set up for drawing dots, and that the fill colour is set up for drawing segments.
/// This function does not change the colours at any point, and is therefore unsuitable for drawing
/// strokes with varying opacity. Tilt data, if present, is ignored.
///
/// `arc_mode` determines whether stroke segments are rounded at the end(s). Arcs allow for
/// smoother connections, but increase the file size. They also necessarily overlap the next
/// segment, making them inappropriate for strokes with transparency (because you can see the
/// arcs).
///
/// `target_angle` is the change in tangent angle that the stroke is segmented to achieve.
fn append_events_basic<'e>(
    events: EventGroup<'e>,
    spread: Spread,
    arc_mode: ArcMode,
    target_angle: f64,
    path_builder: &mut PathBuilder,
) {
    let (events, filtering) = match events {
        EventGroup::Normal(events) => (events, FilteringMode::Raw),
        EventGroup::Shapeified(events) => (events, FilteringMode::Preprocessed),
    };

    let smooth = match StrokeOrDot::from_events(events) {
        StrokeOrDot::Stroke(stroke) => ContinuousStroke::new(&stroke, filtering),

        StrokeOrDot::Dot { x, y, pressure } => {
            let pos: Point2d = (x, y).into();

            // Draw a filled circle.
            let radius = spread.half_width_across(pressure, Vector2d::zero());
            append_simple_line([(pos, radius), (pos, radius)], true, path_builder);

            return;
        }
    };

    let sample_arc_lengths = smooth.sample_points(target_angle);

    let mut last_segment_tan: Option<Vector2d> = None;

    // The tangent at an arc length, falling back to `fallback` if the interpolated tangent is
    // degenerate.
    let tangent_at = |s: f64, fallback: Vector2d| -> Vector2d {
        Some(Vector2d::from(smooth.unit_tangent(s)).normalize())
            .filter(|v| v.is_finite())
            .unwrap_or(fallback)
    };

    for (iter_pos, (start_s, end_s)) in sample_arc_lengths.tuple_windows().with_position() {
        let first_third_s = start_s.lerp(end_s, 1.0 / 3.0);
        let second_third_s = start_s.lerp(end_s, 2.0 / 3.0);

        let start_pos: Point2d = smooth.position(start_s).into();
        let first_third_pos: Point2d = smooth.position(first_third_s).into();
        let second_third_pos: Point2d = smooth.position(second_third_s).into();
        let end_pos: Point2d = smooth.position(end_s).into();

        // Note that we use the same fallback tangent for both middle thirds.
        let middle_fallback_tan = (second_third_pos - first_third_pos).normalize();

        let start_tan = tangent_at(start_s, (first_third_pos - start_pos).normalize());
        let first_third_tan = tangent_at(first_third_s, middle_fallback_tan);
        let second_third_tan = tangent_at(second_third_s, middle_fallback_tan);
        let end_tan = tangent_at(end_s, (end_pos - second_third_pos).normalize());

        // The width of the mark can depend on the direction of travel, so the tangents have to be
        // known before the spreads can be.
        let spread_at =
            |s: f64, tangent: Vector2d| spread.half_width_across(smooth.pressure(s), tangent);

        let start_spread = spread_at(start_s, start_tan);
        let end_spread = spread_at(end_s, end_tan);

        let visual_length = 0.5 * start_spread + (end_pos - start_pos).length() + 0.5 * end_spread;

        let is_very_short =
            visual_length < start_spread.max(end_spread) + start_spread.min(end_spread) * 0.1;

        let is_quite_short = is_very_short || visual_length < 0.7 * (start_spread + end_spread);

        let arcs = SegmentArcs::for_segment(arc_mode, iter_pos);

        // If the segment is not short, try drawing a Bézier pulley. If that doesn't work, or
        // if the segment is short, use a simpler method.
        if is_quite_short
            || append_bezier_pulley(
                [
                    (start_pos, start_tan, start_spread),
                    (
                        first_third_pos,
                        first_third_tan,
                        spread_at(first_third_s, first_third_tan),
                    ),
                    (
                        second_third_pos,
                        second_third_tan,
                        spread_at(second_third_s, second_third_tan),
                    ),
                    (end_pos, end_tan, end_spread),
                ],
                arcs,
                &mut last_segment_tan,
                path_builder,
            )
            .is_err()
        {
            last_segment_tan = Some(end_pos - start_pos);

            let points = [(start_pos, start_spread), (end_pos, end_spread)];

            // If the segment is quite short but not very short, try drawing a simple pulley.
            // If that doesn't work, or if the segment is very short, draw a simple line.
            if is_very_short || append_simple_pulley(points, arcs.any(), path_builder).is_err() {
                append_simple_line(points, arcs.any(), path_builder);
            }
        }
    }
}

/// Draws `events` into `path_builder` exactly the way Samsung's own calligraphy pen renderer does
/// (the redraw path of `libSPenObliquePen.so`:
/// `ObliquePenStrokeDrawableGLV1::GetDrawnPointData → redraw → redrawLine → drawPoint →
/// ObliquePenStrokeDrawableRTV1::AddPoint`, with the live-path skirt geometry from
/// `ObliquePenStrokeDrawableRTV1::lineToMesh`):
///
/// 1. **Point filter** (`redrawLine` entry check): a recorded point is only accepted when it is
///    at least 1.5 pt away from the previously accepted point on either axis.
/// 2. **Decimation** (`redrawLine`): movements closer than 5 pt (50 for finger input) are drawn
///    only every other interval; the first movement after initialization only sets the control
///    point. The control point slides on skipped intervals and is only committed on drawn ones.
/// 3. **Quadratic smoothing** (`SmPath::rewind/moveTo/quadTo`): each drawn interval sweeps a
///    quadratic Bézier from the previous anchor (the last sampled position) with the previous
///    accepted point as control to the midpoint between it and the new point.
/// 4. **Tessellation** (`SmPath::getLength`/`getPosTan` + the divisor table in `redrawLine`):
///    each interval is divided into `max(int(L / divisor), 2) + 10` samples, where the divisor
///    is selected by the pen size: `L/2` for sizes up to 5, then `3`, `L/4`, `5`, `L/8`, `7`,
///    `9`, `10`. For a pen of size ~8 that is a constant 12 samples per interval.
/// 5. **Nib sweep** (`ObliquePenStrokeDrawableRTV1::lineToMesh`): each sample interval sweeps
///    out a parallelogram with corners at the interval endpoints offset by the constant diagonal
///    `(±penSize / 2, ∓penSize / 2)`, joined to its neighbours along the shared cross-section.
///    Two "skirt" quads (the live mesh's anti-aliasing gradients, filled opaquely here) extend
///    the base edges by `2 × normalize(w ∓ u)` where `w` is the unit diagonal and `u` the
///    direction of travel; they provide the minimum across-width on strokes drawn along the nib
///    edge (≈ 0.28 × half the pen size, matching the constants `NIB_*` below). As in Samsung's
///    renderer, event pressure and tilt play no part in the shape of the stroke body.
fn append_events_chisel<'e>(
    events: EventGroup<'e>,
    nib: Nib,
    path_builder: &mut PathBuilder,
) {
    let (events, _filtering) = match events {
        EventGroup::Normal(events) => (events, FilteringMode::Raw),
        EventGroup::Shapeified(events) => (events, FilteringMode::Preprocessed),
    };

    let raw_points = match StrokeOrDot::from_events(events) {
        StrokeOrDot::Stroke(stroke) => stroke.points(),

        StrokeOrDot::Dot { x, y, .. } => {
            // A chisel nib resting on the page leaves a mark shaped like the nib itself.
            nib.append_mark((x, y).into(), path_builder);

            return;
        }
    };

    // 1. Point filter.
    let mut points = vec![raw_points[0]];

    for &point in &raw_points[1..] {
        let last = *points.last().expect("just pushed");

        if (point.0 - last.0).abs() >= 1.5 || (point.1 - last.1).abs() >= 1.5 {
            points.push(point);
        }
    }

    // 2. + 3. The `redrawLine` state machine and per-interval tessellation. State: anchor (the
    // path start of the next interval — re-anchored at the last *sampled* position), control
    // (the Bézier control point, committed only on drawn intervals), prev (the last accepted
    // raw point, used by both filters), and the decimation toggle.
    let mut anchor = [points[0].0, points[0].1];
    let mut control = anchor;
    let mut prev = anchor;
    let mut moved = true;
    let mut first = true;

    let mut intervals: Vec<([f64; 2], [f64; 2], [f64; 2], bool)> = Vec::new();

    // The divisor table of `redrawLine`, indexed by the pen size (`getSize()`).
    let divisor = |l: f64| -> f64 {
        let size = nib.size;

        if size <= 5.0 {
            l / 2.0
        } else if size <= 10.0 {
            3.0
        } else if size <= 15.0 {
            l / 4.0
        } else if size <= 20.0 {
            5.0
        } else if size <= 30.0 {
            l / 8.0
        } else if size <= 40.0 {
            7.0
        } else if size <= 55.0 {
            9.0
        } else {
            10.0
        }
    };

    for &point in &points[1..] {
        let q = [point.0, point.1];
        let (dx, dy) = (q[0] - prev[0], q[1] - prev[1]);

        if dx.abs() < 1.5 && dy.abs() < 1.5 {
            continue;
        }

        let dist = dx.hypot(dy);
        let mut draw = false;

        if dist >= 5.0 {
            moved = true;

            if first {
                control = q;
                first = false;
            } else {
                draw = true;
            }
        } else {
            let was_moved = moved;
            moved = !was_moved;

            if !was_moved {
                if first {
                    control = q;
                    first = false;
                    prev = q;
                    continue;
                }

                draw = true;
            }
        }

        prev = q;

        if draw {
            let anchor_mid = [(control[0] + q[0]) / 2.0, (control[1] + q[1]) / 2.0];
            intervals.push((anchor, control, anchor_mid, false));
            anchor = anchor_mid;
            control = q;
        }
    }

    // `endPen`: the stroke always extends to the final *raw* event position — a quadratic when
    // the pen was moving, a cubic through the control midpoint when it was stalled — subdivided
    // with the same density table, inclusive of the end point.
    let raw_last = [raw_points[raw_points.len() - 1].0, raw_points[raw_points.len() - 1].1];

    if moved {
        intervals.push((anchor, control, raw_last, true));
    } else {
        let mid = [(control[0] + raw_last[0]) / 2.0, (control[1] + raw_last[1]) / 2.0];
        intervals.push((anchor, control, mid, false));
        intervals.push((mid, mid, raw_last, true));
    }

    if intervals.is_empty() {
        // Every point was within the decimation distance of the first: a dot.
        nib.append_mark((points[0].0, points[0].1).into(), path_builder);

        return;
    }

    // 4. Flatten each drawn interval. `redrawLine` samples `N = max(int(L / divisor), 2) + 10`
    // positions at s = k·L/N for k = 0..N (the end included only on the final `endPen` stretch,
    // whose intervals carry `inclusive_end`).
    let mut samples: Vec<Point2d> = Vec::with_capacity(intervals.len() * 13);
    samples.push(Point2d::new(intervals[0].0[0], intervals[0].0[1]));

    for &(start, control, anchor, inclusive_end) in &intervals {
        let l = ((control[0] - start[0]) * 2.0 + (anchor[0] - control[0]))
            .hypot((control[1] - start[1]) * 2.0 + (anchor[1] - control[1]));
        let n = ((l / divisor(l)) as usize).max(2) + 10;
        let last_k = if inclusive_end { n } else { n - 1 };

        for k in 1..=last_k {
            let t = k as f64 / n as f64;
            let u = 1.0 - t;

            samples.push(Point2d::new(
                u * u * start[0] + 2.0 * u * t * control[0] + t * t * anchor[0],
                u * u * start[1] + 2.0 * u * t * control[1] + t * t * anchor[1],
            ));
        }
    }

    // Drop consecutive samples that coincide (the shared interval endpoints).
    let mut deduped: Vec<Point2d> = Vec::with_capacity(samples.len());
    deduped.push(samples[0]);
    let eps = 0.05;

    for &point in &samples[1..] {
        let last = *deduped.last().expect("just pushed");

        if (point.x - last.x).hypot(point.y - last.y) >= eps {
            deduped.push(point);
        }
    }

    let samples = deduped;

    // 5. The nib sweep. `e` is the constant half-diagonal; each interval emits its base
    // parallelogram plus the two skirt quads.
    let half_edge = nib.edge_direction * nib.half_length;
    let unit = |v: Vector2d| -> Vector2d {
        let len = v.x.hypot(v.y);

        if len > 0.0 {
            Vector2d::new(v.x / len, v.y / len)
        } else {
            Vector2d::zero()
        }
    };

    let mut last_sample: Option<Point2d> = None;

    for &point in &samples {
        if let Some(prev) = last_sample {
            let u = unit(Vector2d::new(point.x - prev.x, point.y - prev.y));

            if u.x == 0.0 && u.y == 0.0 {
                continue;
            }

            let x3 = prev + half_edge;
            let x4 = prev - half_edge;
            let x5 = point + half_edge;
            let x6 = point - half_edge;
            let emit = |corners: [Point2d; 4], path_builder: &mut PathBuilder| {
                // Normalize the winding so overlapping subpaths can't cancel under the non-zero
                // fill rule.
                let signed_area = (corners[1].x - corners[0].x) * (corners[2].y - corners[0].y)
                    - (corners[1].y - corners[0].y) * (corners[2].x - corners[0].x);
                let ordered = if signed_area < 0.0 {
                    [corners[3], corners[2], corners[1], corners[0]]
                } else {
                    corners
                };

                path_builder.move_to(ordered[0].x as f32, ordered[0].y as f32);

                for corner in &ordered[1..] {
                    path_builder.line_to(corner.x as f32, corner.y as f32);
                }

                path_builder.close();
            };

            emit([x3, x4, x6, x5], path_builder);
        }

        last_sample = Some(point);
    }
}

fn append_events_straight<'e>(events: EventGroup<'e>, path_builder: &mut PathBuilder) {
    let events = events.events();

    let first = events.first().unwrap();
    let last = events.last().unwrap();

    path_builder.move_to(first.point.x as f32, first.point.y as f32);
    path_builder.line_to(last.point.x as f32, last.point.y as f32);
}
