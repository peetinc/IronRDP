//! Pure rectangle maths for the WebGL2 backend, kept apart so it is host-testable.
//!
//! Surfaces are stored top row first (texel row 0 is the surface's top), so blits between
//! surfaces use surface coordinates directly. Only presenting to the canvas, whose
//! framebuffer origin is bottom-left, flips.

use ironrdp::pdu::geometry::ExclusiveRectangle;

/// GL blit coordinates: `(x0, y0, x1, y1)`.
pub(crate) type BlitRect = (i32, i32, i32, i32);

/// Source and destination of the blit that presents a whole `surface` at `origin` on the
/// canvas, scaled to `target`. The destination's y runs downwards from the canvas top.
pub(crate) fn present_blit(
    surface: (u16, u16),
    origin: (u32, u32),
    target: (u32, u32),
    output_height: u32,
) -> (BlitRect, BlitRect) {
    let to_i32 = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    let (left, top) = (to_i32(origin.0), to_i32(origin.1));
    let height = to_i32(output_height);
    (
        (0, 0, i32::from(surface.0), i32::from(surface.1)),
        (
            left,
            height - top,
            left.saturating_add(to_i32(target.0)),
            height - top.saturating_add(to_i32(target.1)),
        ),
    )
}

/// The part of `rect` inside a `width` by `height` surface, or `None` if nothing is left.
pub(crate) fn clip(rect: &ExclusiveRectangle, width: u16, height: u16) -> Option<ExclusiveRectangle> {
    let clipped = ExclusiveRectangle {
        left: rect.left,
        top: rect.top,
        right: rect.right.min(width),
        bottom: rect.bottom.min(height),
    };
    (clipped.left < clipped.right && clipped.top < clipped.bottom).then_some(clipped)
}

/// Clips a copy of `source` to `point` on a surface of `destination_size`, shrinking the
/// source by what falls off. Returns `(source, destination)`, of equal size.
pub(crate) fn clip_copy(
    source: &ExclusiveRectangle,
    point: (u16, u16),
    destination_size: (u16, u16),
) -> Option<(ExclusiveRectangle, ExclusiveRectangle)> {
    let width = source.right.saturating_sub(source.left);
    let height = source.bottom.saturating_sub(source.top);
    let destination = clip(
        &ExclusiveRectangle {
            left: point.0,
            top: point.1,
            right: point.0.saturating_add(width),
            bottom: point.1.saturating_add(height),
        },
        destination_size.0,
        destination_size.1,
    )?;
    let source = ExclusiveRectangle {
        left: source.left,
        top: source.top,
        right: source.left + (destination.right - destination.left),
        bottom: source.top + (destination.bottom - destination.top),
    };
    Some((source, destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn present_blit_flips_a_mapped_surface_into_canvas_coordinates() {
        let (source, destination) = present_blit((100, 50), (10, 20), (100, 50), 200);

        assert_eq!(source, (0, 0, 100, 50));
        // Surface row 0 lands at the canvas's top edge, y = 200 - 20.
        assert_eq!(destination, (10, 180, 110, 130));
    }

    #[test]
    fn present_blit_scales_to_the_target_size() {
        let (_, destination) = present_blit((100, 50), (0, 0), (200, 100), 100);

        assert_eq!(destination, (0, 100, 200, 0));
    }

    #[test]
    fn clip_keeps_the_part_inside_the_surface() {
        assert_eq!(clip(&rect(90, 10, 120, 20), 100, 100), Some(rect(90, 10, 100, 20)));
        assert_eq!(clip(&rect(100, 10, 120, 20), 100, 100), None);
        assert_eq!(clip(&rect(5, 5, 5, 9), 100, 100), None);
    }

    #[test]
    fn clip_copy_shrinks_the_source_with_the_destination() {
        let clipped = clip_copy(&rect(0, 0, 10, 10), (95, 95), (100, 100));

        assert_eq!(clipped, Some((rect(0, 0, 5, 5), rect(95, 95, 100, 100))));
        assert_eq!(clip_copy(&rect(0, 0, 10, 10), (100, 0), (100, 100)), None);
    }
}
