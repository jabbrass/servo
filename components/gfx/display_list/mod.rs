/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Servo heavily uses display lists, which are retained-mode lists of painting commands to
//! perform. Using a list instead of painting elements in immediate mode allows transforms, hit
//! testing, and invalidation to be performed using the same primitives as painting. It also allows
//! Servo to aggressively cull invisible and out-of-bounds painting elements, to reduce overdraw.
//! Finally, display lists allow tiles to be farmed out onto multiple CPUs and painted in parallel
//! (although this benefit does not apply to GPU-based painting).
//!
//! Display items describe relatively high-level drawing operations (for example, entire borders
//! and shadows instead of lines and blur operations), to reduce the amount of allocation required.
//! They are therefore not exactly analogous to constructs like Skia pictures, which consist of
//! low-level drawing primitives.

#![deny(unsafe_code)]

use display_list::optimizer::DisplayListOptimizer;
use paint_context::{PaintContext, ToAzureRect};
use self::DisplayItem::*;
use self::DisplayItemIterator::*;
use text::glyph::CharIndex;
use text::TextRun;

use azure::azure::AzFloat;
use azure::azure_hl::{Color};

use collections::linked_list::{self, LinkedList};
use geom::{Point2D, Rect, SideOffsets2D, Size2D, Matrix2D};
use geom::approxeq::ApproxEq;
use geom::num::Zero;
use libc::uintptr_t;
use paint_task::PaintLayer;
use msg::compositor_msg::LayerId;
use net_traits::image::base::Image;
use util::opts;
use util::cursor::Cursor;
use util::linked_list::prepend_from;
use util::geometry::{self, Au, MAX_RECT, ZERO_RECT};
use util::mem::HeapSizeOf;
use util::range::Range;
use util::smallvec::{SmallVec, SmallVec8};
use std::fmt;
use std::slice::Iter;
use std::sync::Arc;
use style::computed_values::{border_style, cursor, filter, image_rendering, mix_blend_mode};
use style::computed_values::{pointer_events};
use style::properties::ComputedValues;

// It seems cleaner to have layout code not mention Azure directly, so let's just reexport this for
// layout to use.
pub use azure::azure_hl::GradientStop;

pub mod optimizer;

/// The factor that we multiply the blur radius by in order to inflate the boundaries of display
/// items that involve a blur. This ensures that the display item boundaries include all the ink.
pub static BLUR_INFLATION_FACTOR: i32 = 3;

/// An opaque handle to a node. The only safe operation that can be performed on this node is to
/// compare it to another opaque handle or to another node.
///
/// Because the script task's GC does not trace layout, node data cannot be safely stored in layout
/// data structures. Also, layout code tends to be faster when the DOM is not being accessed, for
/// locality reasons. Using `OpaqueNode` enforces this invariant.
#[derive(Clone, PartialEq, Copy, Debug)]
pub struct OpaqueNode(pub uintptr_t);

impl OpaqueNode {
    /// Returns the address of this node, for debugging purposes.
    pub fn id(&self) -> uintptr_t {
        let OpaqueNode(pointer) = *self;
        pointer
    }
}

/// Display items that make up a stacking context. "Steps" here refer to the steps in CSS 2.1
/// Appendix E.
///
/// TODO(pcwalton): We could reduce the size of this structure with a more "skip list"-like
/// structure, omitting several pointers and lengths.
pub struct DisplayList {
    /// The border and backgrounds for the root of this stacking context: steps 1 and 2.
    pub background_and_borders: LinkedList<DisplayItem>,
    /// Borders and backgrounds for block-level descendants: step 4.
    pub block_backgrounds_and_borders: LinkedList<DisplayItem>,
    /// Floats: step 5. These are treated as pseudo-stacking contexts.
    pub floats: LinkedList<DisplayItem>,
    /// All other content.
    pub content: LinkedList<DisplayItem>,
    /// Outlines: step 10.
    pub outlines: LinkedList<DisplayItem>,
    /// Child stacking contexts.
    pub children: LinkedList<Arc<StackingContext>>,
}

impl DisplayList {
    /// Creates a new, empty display list.
    #[inline]
    pub fn new() -> DisplayList {
        DisplayList {
            background_and_borders: LinkedList::new(),
            block_backgrounds_and_borders: LinkedList::new(),
            floats: LinkedList::new(),
            content: LinkedList::new(),
            outlines: LinkedList::new(),
            children: LinkedList::new(),
        }
    }

    /// Appends all display items from `other` into `self`, preserving stacking order and emptying
    /// `other` in the process.
    #[inline]
    pub fn append_from(&mut self, other: &mut DisplayList) {
        self.background_and_borders.append(&mut other.background_and_borders);
        self.block_backgrounds_and_borders.append(&mut other.block_backgrounds_and_borders);
        self.floats.append(&mut other.floats);
        self.content.append(&mut other.content);
        self.outlines.append(&mut other.outlines);
        self.children.append(&mut other.children);
    }

    /// Merges all display items from all non-float stacking levels to the `float` stacking level.
    #[inline]
    pub fn form_float_pseudo_stacking_context(&mut self) {
        prepend_from(&mut self.floats, &mut self.outlines);
        prepend_from(&mut self.floats, &mut self.content);
        prepend_from(&mut self.floats, &mut self.block_backgrounds_and_borders);
        prepend_from(&mut self.floats, &mut self.background_and_borders);
    }

    /// Returns a list of all items in this display list concatenated together. This is extremely
    /// inefficient and should only be used for debugging.
    pub fn all_display_items(&self) -> Vec<DisplayItem> {
        let mut result = Vec::new();
        for display_item in self.background_and_borders.iter() {
            result.push((*display_item).clone())
        }
        for display_item in self.block_backgrounds_and_borders.iter() {
            result.push((*display_item).clone())
        }
        for display_item in self.floats.iter() {
            result.push((*display_item).clone())
        }
        for display_item in self.content.iter() {
            result.push((*display_item).clone())
        }
        for display_item in self.outlines.iter() {
            result.push((*display_item).clone())
        }
        result
    }

    // Print the display list. Only makes sense to call it after performing reflow.
    pub fn print_items(&self, mut indentation: String) {
        let min_length = 4;
        // We cover the case of an empty string.
        if indentation.len() == 0 {
            indentation = String::from_str("####");
        }

        // We grow the indentation by 4 characters if needed.
        // I wish to push it all as a slice, but it won't work if the string is a single char.
        while indentation.len() < min_length {
            let c = indentation.char_at(0);
            indentation.push(c);
        }

        // Closures are so nice!
        let doit = |items: &Vec<DisplayItem>| {
            for item in items.iter() {
                match *item {
                    DisplayItem::SolidColorClass(ref solid_color) => {
                        println!("{:?} SolidColor. {:?}", indentation, solid_color.base.bounds)
                    }
                    DisplayItem::TextClass(ref text) => {
                        println!("{:?} Text. {:?}", indentation, text.base.bounds)
                    }
                    DisplayItem::ImageClass(ref image) => {
                        println!("{:?} Image. {:?}", indentation, image.base.bounds)
                    }
                    DisplayItem::BorderClass(ref border) => {
                        println!("{:?} Border. {:?}", indentation, border.base.bounds)
                    }
                    DisplayItem::GradientClass(ref gradient) => {
                        println!("{:?} Gradient. {:?}", indentation, gradient.base.bounds)
                    }
                    DisplayItem::LineClass(ref line) => {
                        println!("{:?} Line. {:?}", indentation, line.base.bounds)
                    }
                    DisplayItem::BoxShadowClass(ref box_shadow) => {
                        println!("{:?} Box_shadow. {:?}", indentation, box_shadow.base.bounds)
                    }
                }
            }
            println!("\n");
        };

        doit(&(self.all_display_items()));
        if self.children.len() != 0 {
            println!("{} Children stacking contexts list length: {}",
                     indentation,
                     self.children.len());
            for sublist in self.children.iter() {
                sublist.display_list.print_items(indentation.clone()+&indentation[0..min_length]);
            }
        }
    }
}

impl HeapSizeOf for DisplayList {
    fn heap_size_of_children(&self) -> usize {
        self.background_and_borders.heap_size_of_children() +
            self.block_backgrounds_and_borders.heap_size_of_children() +
            self.floats.heap_size_of_children() +
            self.content.heap_size_of_children() +
            self.outlines.heap_size_of_children() +
            self.children.heap_size_of_children()
    }
}

/// Represents one CSS stacking context, which may or may not have a hardware layer.
pub struct StackingContext {
    /// The display items that make up this stacking context.
    pub display_list: Box<DisplayList>,

    /// The layer for this stacking context, if there is one.
    pub layer: Option<Arc<PaintLayer>>,

    /// The position and size of this stacking context.
    pub bounds: Rect<Au>,
    /// The overflow rect for this stacking context in its coordinate system.
    pub overflow: Rect<Au>,

    /// The `z-index` for this stacking context.
    pub z_index: i32,

    /// CSS filters to be applied to this stacking context (including opacity).
    pub filters: filter::T,

    /// The blend mode with which this stacking context blends with its backdrop.
    pub blend_mode: mix_blend_mode::T,

    /// A transform to be applied to this stacking context.
    ///
    /// TODO(pcwalton): 3D transforms.
    pub transform: Matrix2D<AzFloat>,
}

impl StackingContext {
    /// Creates a new stacking context.
    #[inline]
    pub fn new(display_list: Box<DisplayList>,
               bounds: &Rect<Au>,
               overflow: &Rect<Au>,
               z_index: i32,
               transform: &Matrix2D<AzFloat>,
               filters: filter::T,
               blend_mode: mix_blend_mode::T,
               layer: Option<Arc<PaintLayer>>)
               -> StackingContext {
        StackingContext {
            display_list: display_list,
            layer: layer,
            bounds: *bounds,
            overflow: *overflow,
            z_index: z_index,
            transform: *transform,
            filters: filters,
            blend_mode: blend_mode,
        }
    }

    /// Draws the stacking context in the proper order according to the steps in CSS 2.1 § E.2.
    pub fn optimize_and_draw_into_context(&self,
                                          paint_context: &mut PaintContext,
                                          tile_bounds: &Rect<AzFloat>,
                                          transform: &Matrix2D<AzFloat>,
                                          clip_rect: Option<&Rect<Au>>) {
        let transform = transform.mul(&self.transform);
        let temporary_draw_target =
            paint_context.get_or_create_temporary_draw_target(&self.filters, self.blend_mode);
        {
            let mut paint_subcontext = PaintContext {
                draw_target: temporary_draw_target.clone(),
                font_context: &mut *paint_context.font_context,
                page_rect: *tile_bounds,
                screen_rect: paint_context.screen_rect,
                clip_rect: clip_rect.map(|clip_rect| *clip_rect),
                transient_clip: None,
            };

            // Optimize the display list to throw out out-of-bounds display items and so forth.
            let display_list =
                DisplayListOptimizer::new(tile_bounds).optimize(&*self.display_list);

            if opts::get().dump_display_list_optimized {
                println!("**** optimized display list. Tile bounds: {:?}", tile_bounds);
                display_list.print_items(String::from_str("*"));
            }

            // Sort positioned children according to z-index.
            let mut positioned_children = SmallVec8::new();
            for kid in display_list.children.iter() {
                positioned_children.push((*kid).clone());
            }
            positioned_children.as_slice_mut()
                               .sort_by(|this, other| this.z_index.cmp(&other.z_index));

            // Set up our clip rect and transform.
            let old_transform = paint_subcontext.draw_target.get_transform();
            paint_subcontext.draw_target.set_transform(&transform);
            paint_subcontext.push_clip_if_applicable();

            // Steps 1 and 2: Borders and background for the root.
            for display_item in display_list.background_and_borders.iter() {
                display_item.draw_into_context(&mut paint_subcontext)
            }

            // Step 3: Positioned descendants with negative z-indices.
            for positioned_kid in positioned_children.iter() {
                if positioned_kid.z_index >= 0 {
                    break
                }
                if positioned_kid.layer.is_none() {
                    let new_transform =
                        transform.translate(positioned_kid.bounds
                                                          .origin
                                                          .x
                                                          .to_nearest_px() as AzFloat,
                                            positioned_kid.bounds
                                                          .origin
                                                          .y
                                                          .to_nearest_px() as AzFloat);
                    let new_tile_rect =
                        self.compute_tile_rect_for_child_stacking_context(tile_bounds,
                                                                          &**positioned_kid);
                    positioned_kid.optimize_and_draw_into_context(&mut paint_subcontext,
                                                                  &new_tile_rect,
                                                                  &new_transform,
                                                                  Some(&positioned_kid.overflow))
                }
            }

            // Step 4: Block backgrounds and borders.
            for display_item in display_list.block_backgrounds_and_borders.iter() {
                display_item.draw_into_context(&mut paint_subcontext)
            }

            // Step 5: Floats.
            for display_item in display_list.floats.iter() {
                display_item.draw_into_context(&mut paint_subcontext)
            }

            // TODO(pcwalton): Step 6: Inlines that generate stacking contexts.

            // Step 7: Content.
            for display_item in display_list.content.iter() {
                display_item.draw_into_context(&mut paint_subcontext)
            }

            // Steps 8 and 9: Positioned descendants with nonnegative z-indices.
            for positioned_kid in positioned_children.iter() {
                if positioned_kid.z_index < 0 {
                    continue
                }

                if positioned_kid.layer.is_none() {
                    let new_transform =
                        transform.translate(positioned_kid.bounds
                                                          .origin
                                                          .x
                                                          .to_nearest_px() as AzFloat,
                                            positioned_kid.bounds
                                                          .origin
                                                          .y
                                                          .to_nearest_px() as AzFloat);
                    let new_tile_rect =
                        self.compute_tile_rect_for_child_stacking_context(tile_bounds,
                                                                          &**positioned_kid);
                    positioned_kid.optimize_and_draw_into_context(&mut paint_subcontext,
                                                                  &new_tile_rect,
                                                                  &new_transform,
                                                                  Some(&positioned_kid.overflow))
                }
            }

            // Step 10: Outlines.
            for display_item in display_list.outlines.iter() {
                display_item.draw_into_context(&mut paint_subcontext)
            }

            // Undo our clipping and transform.
            paint_subcontext.remove_transient_clip_if_applicable();
            paint_subcontext.pop_clip_if_applicable();
            paint_subcontext.draw_target.set_transform(&old_transform)
        }

        paint_context.draw_temporary_draw_target_if_necessary(&temporary_draw_target,
                                                              &self.filters,
                                                              self.blend_mode)
    }

    /// Translate the given tile rect into the coordinate system of a child stacking context.
    fn compute_tile_rect_for_child_stacking_context(&self,
                                                    tile_bounds: &Rect<AzFloat>,
                                                    child_stacking_context: &StackingContext)
                                                    -> Rect<AzFloat> {
        static ZERO_AZURE_RECT: Rect<f32> = Rect {
            origin: Point2D {
                x: 0.0,
                y: 0.0,
            },
            size: Size2D {
                width: 0.0,
                height: 0.0
            }
        };

        // Translate the child's overflow region into our coordinate system.
        let child_stacking_context_overflow =
            child_stacking_context.overflow.translate(&child_stacking_context.bounds.origin)
                                           .to_azure_rect();

        // Intersect that with the current tile boundaries to find the tile boundaries that the
        // child covers.
        let tile_subrect = tile_bounds.intersection(&child_stacking_context_overflow)
                                      .unwrap_or(ZERO_AZURE_RECT);

        // Translate the resulting rect into the child's coordinate system.
        tile_subrect.translate(&-child_stacking_context.bounds.to_azure_rect().origin)
    }

    /// Places all nodes containing the point of interest into `result`, topmost first. Respects
    /// the `pointer-events` CSS property If `topmost_only` is true, stops after placing one node
    /// into the list. `result` must be empty upon entry to this function.
    pub fn hit_test(&self,
                    mut point: Point2D<Au>,
                    result: &mut Vec<DisplayItemMetadata>,
                    topmost_only: bool) {
        fn hit_test_in_list<'a,I>(point: Point2D<Au>,
                                  result: &mut Vec<DisplayItemMetadata>,
                                  topmost_only: bool,
                                  iterator: I)
                                  where I: Iterator<Item=&'a DisplayItem> {
            for item in iterator {
                // TODO(pcwalton): Use a precise algorithm here. This will allow us to properly hit
                // test elements with `border-radius`, for example.
                if !item.base().clip.might_intersect_point(&point) {
                    // Clipped out.
                    continue
                }
                if !geometry::rect_contains_point(item.bounds(), point) {
                    // Can't possibly hit.
                    continue
                }
                if item.base().metadata.pointing.is_none() {
                    // `pointer-events` is `none`. Ignore this item.
                    continue
                }
                match *item {
                    DisplayItem::BorderClass(ref border) => {
                        // If the point is inside the border, it didn't hit the border!
                        let interior_rect =
                            Rect(Point2D(border.base.bounds.origin.x + border.border_widths.left,
                                         border.base.bounds.origin.y + border.border_widths.top),
                                 Size2D(border.base.bounds.size.width -
                                            (border.border_widths.left +
                                             border.border_widths.right),
                                        border.base.bounds.size.height -
                                            (border.border_widths.top +
                                             border.border_widths.bottom)));
                        if geometry::rect_contains_point(interior_rect, point) {
                            continue
                        }
                    }
                    _ => {}
                }

                // We found a hit!
                result.push(item.base().metadata);
                if topmost_only {
                    return
                }
            }
        }

        // Convert the point into stacking context local space
        point = point - self.bounds.origin;

        debug_assert!(!topmost_only || result.is_empty());
        let frac_point = self.transform.transform_point(&Point2D(point.x.to_frac32_px(),
                                                                 point.y.to_frac32_px()));
        point = Point2D(Au::from_frac32_px(frac_point.x), Au::from_frac32_px(frac_point.y));

        // Iterate through display items in reverse stacking order. Steps here refer to the
        // painting steps in CSS 2.1 Appendix E.
        //
        // Step 10: Outlines.
        hit_test_in_list(point, result, topmost_only, self.display_list.outlines.iter().rev());
        if topmost_only && !result.is_empty() {
            return
        }

        // Steps 9 and 8: Positioned descendants with nonnegative z-indices.
        for kid in self.display_list.children.iter().rev() {
            if kid.z_index < 0 {
                continue
            }
            kid.hit_test(point, result, topmost_only);
            if topmost_only && !result.is_empty() {
                return
            }
        }

        // Steps 7, 5, and 4: Content, floats, and block backgrounds and borders.
        //
        // TODO(pcwalton): Step 6: Inlines that generate stacking contexts.
        for display_list in [
            &self.display_list.content,
            &self.display_list.floats,
            &self.display_list.block_backgrounds_and_borders,
        ].iter() {
            hit_test_in_list(point, result, topmost_only, display_list.iter().rev());
            if topmost_only && !result.is_empty() {
                return
            }
        }

        // Step 3: Positioned descendants with negative z-indices.
        for kid in self.display_list.children.iter().rev() {
            if kid.z_index >= 0 {
                continue
            }
            kid.hit_test(point, result, topmost_only);
            if topmost_only && !result.is_empty() {
                return
            }
        }

        // Steps 2 and 1: Borders and background for the root.
        hit_test_in_list(point,
                         result,
                         topmost_only,
                         self.display_list.background_and_borders.iter().rev())
    }
}

impl HeapSizeOf for StackingContext {
    fn heap_size_of_children(&self) -> usize {
        self.display_list.heap_size_of_children()

        // FIXME(njn): other fields may be measured later, esp. `layer`
    }
}

/// Returns the stacking context in the given tree of stacking contexts with a specific layer ID.
pub fn find_stacking_context_with_layer_id(this: &Arc<StackingContext>, layer_id: LayerId)
                                           -> Option<Arc<StackingContext>> {
    match this.layer {
        Some(ref layer) if layer.id == layer_id => return Some((*this).clone()),
        Some(_) | None => {}
    }

    for kid in this.display_list.children.iter() {
        match find_stacking_context_with_layer_id(kid, layer_id) {
            Some(stacking_context) => return Some(stacking_context),
            None => {}
        }
    }

    None
}

/// One drawing command in the list.
#[derive(Clone)]
pub enum DisplayItem {
    SolidColorClass(Box<SolidColorDisplayItem>),
    TextClass(Box<TextDisplayItem>),
    ImageClass(Box<ImageDisplayItem>),
    BorderClass(Box<BorderDisplayItem>),
    GradientClass(Box<GradientDisplayItem>),
    LineClass(Box<LineDisplayItem>),
    BoxShadowClass(Box<BoxShadowDisplayItem>),
}

/// Information common to all display items.
#[derive(Clone)]
pub struct BaseDisplayItem {
    /// The boundaries of the display item, in layer coordinates.
    pub bounds: Rect<Au>,

    /// Metadata attached to this display item.
    pub metadata: DisplayItemMetadata,

    /// The region to clip to.
    pub clip: ClippingRegion,
}

impl BaseDisplayItem {
    #[inline(always)]
    pub fn new(bounds: Rect<Au>, metadata: DisplayItemMetadata, clip: ClippingRegion)
               -> BaseDisplayItem {
        BaseDisplayItem {
            bounds: bounds,
            metadata: metadata,
            clip: clip,
        }
    }
}

impl HeapSizeOf for BaseDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.metadata.heap_size_of_children() +
            self.clip.heap_size_of_children()
    }
}

/// A clipping region for a display item. Currently, this can describe rectangles, rounded
/// rectangles (for `border-radius`), or arbitrary intersections of the two. Arbitrary transforms
/// are not supported because those are handled by the higher-level `StackingContext` abstraction.
#[derive(Clone, PartialEq, Debug)]
pub struct ClippingRegion {
    /// The main rectangular region. This does not include any corners.
    pub main: Rect<Au>,
    /// Any complex regions.
    ///
    /// TODO(pcwalton): Atomically reference count these? Not sure if it's worth the trouble.
    /// Measure and follow up.
    pub complex: Vec<ComplexClippingRegion>,
}

/// A complex clipping region. These don't as easily admit arbitrary intersection operations, so
/// they're stored in a list over to the side. Currently a complex clipping region is just a
/// rounded rectangle, but the CSS WGs will probably make us throw more stuff in here eventually.
#[derive(Clone, PartialEq, Debug)]
pub struct ComplexClippingRegion {
    /// The boundaries of the rectangle.
    pub rect: Rect<Au>,
    /// Border radii of this rectangle.
    pub radii: BorderRadii<Au>,
}

impl ClippingRegion {
    /// Returns an empty clipping region that, if set, will result in no pixels being visible.
    #[inline]
    pub fn empty() -> ClippingRegion {
        ClippingRegion {
            main: ZERO_RECT,
            complex: Vec::new(),
        }
    }

    /// Returns an all-encompassing clipping region that clips no pixels out.
    #[inline]
    pub fn max() -> ClippingRegion {
        ClippingRegion {
            main: MAX_RECT,
            complex: Vec::new(),
        }
    }

    /// Returns a clipping region that represents the given rectangle.
    #[inline]
    pub fn from_rect(rect: &Rect<Au>) -> ClippingRegion {
        ClippingRegion {
            main: *rect,
            complex: Vec::new(),
        }
    }

    /// Returns the intersection of this clipping region and the given rectangle.
    ///
    /// TODO(pcwalton): This could more eagerly eliminate complex clipping regions, at the cost of
    /// complexity.
    #[inline]
    pub fn intersect_rect(self, rect: &Rect<Au>) -> ClippingRegion {
        ClippingRegion {
            main: self.main.intersection(rect).unwrap_or(ZERO_RECT),
            complex: self.complex,
        }
    }

    /// Returns true if this clipping region might be nonempty. This can return false positives,
    /// but never false negatives.
    #[inline]
    pub fn might_be_nonempty(&self) -> bool {
        !self.main.is_empty()
    }

    /// Returns true if this clipping region might contain the given point and false otherwise.
    /// This is a quick, not a precise, test; it can yield false positives.
    #[inline]
    pub fn might_intersect_point(&self, point: &Point2D<Au>) -> bool {
        geometry::rect_contains_point(self.main, *point) &&
            self.complex.iter().all(|complex| geometry::rect_contains_point(complex.rect, *point))
    }

    /// Returns true if this clipping region might intersect the given rectangle and false
    /// otherwise. This is a quick, not a precise, test; it can yield false positives.
    #[inline]
    pub fn might_intersect_rect(&self, rect: &Rect<Au>) -> bool {
        self.main.intersects(rect) &&
            self.complex.iter().all(|complex| complex.rect.intersects(rect))
    }


    /// Returns a bounding rect that surrounds this entire clipping region.
    #[inline]
    pub fn bounding_rect(&self) -> Rect<Au> {
        let mut rect = self.main;
        for complex in self.complex.iter() {
            rect = rect.union(&complex.rect)
        }
        rect
    }

    /// Intersects this clipping region with the given rounded rectangle.
    #[inline]
    pub fn intersect_with_rounded_rect(mut self, rect: &Rect<Au>, radii: &BorderRadii<Au>)
                                       -> ClippingRegion {
        self.complex.push(ComplexClippingRegion {
            rect: *rect,
            radii: *radii,
        });
        self
    }

    /// Translates this clipping region by the given vector.
    #[inline]
    pub fn translate(&self, delta: &Point2D<Au>) -> ClippingRegion {
        ClippingRegion {
            main: self.main.translate(delta),
            complex: self.complex.iter().map(|complex| {
                ComplexClippingRegion {
                    rect: complex.rect.translate(delta),
                    radii: complex.radii,
                }
            }).collect(),
        }
    }
}

impl HeapSizeOf for ClippingRegion {
    fn heap_size_of_children(&self) -> usize {
        self.complex.heap_size_of_children()
    }
}

impl HeapSizeOf for ComplexClippingRegion {
    fn heap_size_of_children(&self) -> usize {
        0
    }
}

/// Metadata attached to each display item. This is useful for performing auxiliary tasks with
/// the display list involving hit testing: finding the originating DOM node and determining the
/// cursor to use when the element is hovered over.
#[derive(Clone, Copy)]
pub struct DisplayItemMetadata {
    /// The DOM node from which this display item originated.
    pub node: OpaqueNode,
    /// The value of the `cursor` property when the mouse hovers over this display item. If `None`,
    /// this display item is ineligible for pointer events (`pointer-events: none`).
    pub pointing: Option<Cursor>,
}

impl DisplayItemMetadata {
    /// Creates a new set of display metadata for a display item constributed by a DOM node.
    /// `default_cursor` specifies the cursor to use if `cursor` is `auto`. Typically, this will
    /// be `PointerCursor`, but for text display items it may be `TextCursor` or
    /// `VerticalTextCursor`.
    #[inline]
    pub fn new(node: OpaqueNode, style: &ComputedValues, default_cursor: Cursor)
               -> DisplayItemMetadata {
        DisplayItemMetadata {
            node: node,
            pointing: match (style.get_pointing().pointer_events, style.get_pointing().cursor) {
                (pointer_events::T::none, _) => None,
                (pointer_events::T::auto, cursor::T::AutoCursor) => Some(default_cursor),
                (pointer_events::T::auto, cursor::T::SpecifiedCursor(cursor)) => Some(cursor),
            },
        }
    }
}

impl HeapSizeOf for DisplayItemMetadata {
    fn heap_size_of_children(&self) -> usize {
        0
    }
}

/// Paints a solid color.
#[derive(Clone)]
pub struct SolidColorDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The color.
    pub color: Color,
}

impl HeapSizeOf for SolidColorDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
    }
}

/// Paints text.
#[derive(Clone)]
pub struct TextDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The text run.
    pub text_run: Arc<Box<TextRun>>,

    /// The range of text within the text run.
    pub range: Range<CharIndex>,

    /// The color of the text.
    pub text_color: Color,

    /// The position of the start of the baseline of this text.
    pub baseline_origin: Point2D<Au>,

    /// The orientation of the text: upright or sideways left/right.
    pub orientation: TextOrientation,

    /// The blur radius for this text. If zero, this text is not blurred.
    pub blur_radius: Au,
}

impl HeapSizeOf for TextDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
        // We exclude `text_run` because it is non-owning.
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum TextOrientation {
    Upright,
    SidewaysLeft,
    SidewaysRight,
}

/// Paints an image.
#[derive(Clone)]
pub struct ImageDisplayItem {
    pub base: BaseDisplayItem,
    pub image: Arc<Image>,

    /// The dimensions to which the image display item should be stretched. If this is smaller than
    /// the bounds of this display item, then the image will be repeated in the appropriate
    /// direction to tile the entire bounds.
    pub stretch_size: Size2D<Au>,

    /// The algorithm we should use to stretch the image. See `image_rendering` in CSS-IMAGES-3 §
    /// 5.3.
    pub image_rendering: image_rendering::T,
}

impl HeapSizeOf for ImageDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
        // We exclude `image` here because it is non-owning.
    }
}

/// Paints a gradient.
#[derive(Clone)]
pub struct GradientDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The start point of the gradient (computed during display list construction).
    pub start_point: Point2D<Au>,

    /// The end point of the gradient (computed during display list construction).
    pub end_point: Point2D<Au>,

    /// A list of color stops.
    pub stops: Vec<GradientStop>,
}

impl HeapSizeOf for GradientDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        use libc::c_void;
        use util::mem::heap_size_of;

        // We can't measure `stops` via Vec's HeapSizeOf implementation because GradientStop isn't
        // defined in this module, and we don't want to import GradientStop into util::mem where
        // the HeapSizeOf trait is defined. So we measure the elements directly.
        self.base.heap_size_of_children() +
            heap_size_of(self.stops.as_ptr() as *const c_void)
    }
}


/// Paints a border.
#[derive(Clone)]
pub struct BorderDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// Border widths.
    pub border_widths: SideOffsets2D<Au>,

    /// Border colors.
    pub color: SideOffsets2D<Color>,

    /// Border styles.
    pub style: SideOffsets2D<border_style::T>,

    /// Border radii.
    ///
    /// TODO(pcwalton): Elliptical radii.
    pub radius: BorderRadii<Au>,
}

impl HeapSizeOf for BorderDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
    }
}

/// Information about the border radii.
///
/// TODO(pcwalton): Elliptical radii.
#[derive(Clone, Default, PartialEq, Debug, Copy)]
pub struct BorderRadii<T> {
    pub top_left: T,
    pub top_right: T,
    pub bottom_right: T,
    pub bottom_left: T,
}

impl<T> BorderRadii<T> where T: PartialEq + Zero {
    /// Returns true if all the radii are zero.
    pub fn is_square(&self) -> bool {
        let zero = Zero::zero();
        self.top_left == zero && self.top_right == zero && self.bottom_right == zero &&
            self.bottom_left == zero
    }
}

impl<T> BorderRadii<T> where T: PartialEq + Zero + Clone {
    /// Returns a set of border radii that all have the given value.
    pub fn all_same(value: T) -> BorderRadii<T> {
        BorderRadii {
            top_left: value.clone(),
            top_right: value.clone(),
            bottom_right: value.clone(),
            bottom_left: value.clone(),
        }
    }
}

/// Paints a line segment.
#[derive(Clone)]
pub struct LineDisplayItem {
    pub base: BaseDisplayItem,

    /// The line segment color.
    pub color: Color,

    /// The line segment style.
    pub style: border_style::T
}

impl HeapSizeOf for LineDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
    }
}

/// Paints a box shadow per CSS-BACKGROUNDS.
#[derive(Clone)]
pub struct BoxShadowDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The dimensions of the box that we're placing a shadow around.
    pub box_bounds: Rect<Au>,

    /// The offset of this shadow from the box.
    pub offset: Point2D<Au>,

    /// The color of this shadow.
    pub color: Color,

    /// The blur radius for this shadow.
    pub blur_radius: Au,

    /// The spread radius of this shadow.
    pub spread_radius: Au,

    /// How we should clip the result.
    pub clip_mode: BoxShadowClipMode,
}

impl HeapSizeOf for BoxShadowDisplayItem {
    fn heap_size_of_children(&self) -> usize {
        self.base.heap_size_of_children()
    }
}

/// How a box shadow should be clipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoxShadowClipMode {
    /// No special clipping should occur. This is used for (shadowed) text decorations.
    None,
    /// The area inside `box_bounds` should be clipped out. Corresponds to the normal CSS
    /// `box-shadow`.
    Outset,
    /// The area outside `box_bounds` should be clipped out. Corresponds to the `inset` flag on CSS
    /// `box-shadow`.
    Inset,
}

pub enum DisplayItemIterator<'a> {
    Empty,
    Parent(linked_list::Iter<'a,DisplayItem>),
}

impl<'a> Iterator for DisplayItemIterator<'a> {
    type Item = &'a DisplayItem;
    #[inline]
    fn next(&mut self) -> Option<&'a DisplayItem> {
        match *self {
            DisplayItemIterator::Empty => None,
            DisplayItemIterator::Parent(ref mut subiterator) => subiterator.next(),
        }
    }
}

impl DisplayItem {
    /// Paints this display item into the given painting context.
    fn draw_into_context(&self, paint_context: &mut PaintContext) {
        {
            let this_clip = &self.base().clip;
            match paint_context.transient_clip {
                Some(ref transient_clip) if transient_clip == this_clip => {}
                Some(_) | None => paint_context.push_transient_clip((*this_clip).clone()),
            }
        }

        match *self {
            DisplayItem::SolidColorClass(ref solid_color) => {
                if !solid_color.color.a.approx_eq(&0.0) {
                    paint_context.draw_solid_color(&solid_color.base.bounds, solid_color.color)
                }
            }

            DisplayItem::TextClass(ref text) => {
                debug!("Drawing text at {:?}.", text.base.bounds);
                paint_context.draw_text(&**text);
            }

            DisplayItem::ImageClass(ref image_item) => {
                // FIXME(pcwalton): This is a really inefficient way to draw a tiled image; use a
                // brush instead.
                debug!("Drawing image at {:?}.", image_item.base.bounds);

                let mut y_offset = Au(0);
                while y_offset < image_item.base.bounds.size.height {
                    let mut x_offset = Au(0);
                    while x_offset < image_item.base.bounds.size.width {
                        let mut bounds = image_item.base.bounds;
                        bounds.origin.x = bounds.origin.x + x_offset;
                        bounds.origin.y = bounds.origin.y + y_offset;
                        bounds.size = image_item.stretch_size;

                        paint_context.draw_image(&bounds,
                                                 image_item.image.clone(),
                                                 image_item.image_rendering.clone());

                        x_offset = x_offset + image_item.stretch_size.width;
                    }

                    y_offset = y_offset + image_item.stretch_size.height;
                }
            }

            DisplayItem::BorderClass(ref border) => {
                paint_context.draw_border(&border.base.bounds,
                                          &border.border_widths,
                                          &border.radius,
                                          &border.color,
                                          &border.style)
            }

            DisplayItem::GradientClass(ref gradient) => {
                paint_context.draw_linear_gradient(&gradient.base.bounds,
                                                   &gradient.start_point,
                                                   &gradient.end_point,
                                                   &gradient.stops);
            }

            DisplayItem::LineClass(ref line) => {
                paint_context.draw_line(&line.base.bounds, line.color, line.style)
            }

            DisplayItem::BoxShadowClass(ref box_shadow) => {
                paint_context.draw_box_shadow(&box_shadow.box_bounds,
                                              &box_shadow.offset,
                                              box_shadow.color,
                                              box_shadow.blur_radius,
                                              box_shadow.spread_radius,
                                              box_shadow.clip_mode)
            }
        }
    }

    pub fn base<'a>(&'a self) -> &'a BaseDisplayItem {
        match *self {
            DisplayItem::SolidColorClass(ref solid_color) => &solid_color.base,
            DisplayItem::TextClass(ref text) => &text.base,
            DisplayItem::ImageClass(ref image_item) => &image_item.base,
            DisplayItem::BorderClass(ref border) => &border.base,
            DisplayItem::GradientClass(ref gradient) => &gradient.base,
            DisplayItem::LineClass(ref line) => &line.base,
            DisplayItem::BoxShadowClass(ref box_shadow) => &box_shadow.base,
        }
    }

    pub fn mut_base<'a>(&'a mut self) -> &'a mut BaseDisplayItem {
        match *self {
            DisplayItem::SolidColorClass(ref mut solid_color) => &mut solid_color.base,
            DisplayItem::TextClass(ref mut text) => &mut text.base,
            DisplayItem::ImageClass(ref mut image_item) => &mut image_item.base,
            DisplayItem::BorderClass(ref mut border) => &mut border.base,
            DisplayItem::GradientClass(ref mut gradient) => &mut gradient.base,
            DisplayItem::LineClass(ref mut line) => &mut line.base,
            DisplayItem::BoxShadowClass(ref mut box_shadow) => &mut box_shadow.base,
        }
    }

    pub fn bounds(&self) -> Rect<Au> {
        self.base().bounds
    }

    pub fn debug_with_level(&self, level: u32) {
        let mut indent = String::new();
        for _ in 0..level {
            indent.push_str("| ")
        }
        println!("{}+ {:?}", indent, self);
    }
}

impl fmt::Debug for DisplayItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} @ {:?} ({:x})",
            match *self {
                DisplayItem::SolidColorClass(_) => "SolidColor",
                DisplayItem::TextClass(_) => "Text",
                DisplayItem::ImageClass(_) => "Image",
                DisplayItem::BorderClass(_) => "Border",
                DisplayItem::GradientClass(_) => "Gradient",
                DisplayItem::LineClass(_) => "Line",
                DisplayItem::BoxShadowClass(_) => "BoxShadow",
            },
            self.base().bounds,
            self.base().metadata.node.id()
        )
    }
}

impl HeapSizeOf for DisplayItem {
    fn heap_size_of_children(&self) -> usize {
        match *self {
            SolidColorClass(ref item) => item.heap_size_of_children(),
            TextClass(ref item)       => item.heap_size_of_children(),
            ImageClass(ref item)      => item.heap_size_of_children(),
            BorderClass(ref item)     => item.heap_size_of_children(),
            GradientClass(ref item)   => item.heap_size_of_children(),
            LineClass(ref item)       => item.heap_size_of_children(),
            BoxShadowClass(ref item)  => item.heap_size_of_children(),
        }
    }
}

