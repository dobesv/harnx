use std::collections::HashMap;
use std::iter::zip;

use ratatui_core::layout::Rect;
use ratatui_core::terminal::Frame;
use ratatui_core::widgets::Widget;

pub type ElementHeight = usize;
pub type IndexInHeightLog = usize;
pub type PartialTopItem = Option<(IndexInHeightLog, Rect)>;
pub type FullItems = Option<(std::ops::Range<IndexInHeightLog>, Rect)>;
/// `(item_index, screen_area, lines_above_viewport)`
///
/// `lines_above_viewport` is the number of lines of this item that are
/// scrolled above the top of the viewport.  The renderer must skip that
/// many lines before copying content into the frame.
/// `(item_index, screen_area, lines_above_viewport)`
///
/// `lines_above_viewport` is the number of lines of this item that have been
/// scrolled past the *top* edge of the viewport (not the bottom).  The
/// renderer must skip exactly that many lines from the start of the item
/// buffer before copying content into the frame.
///
/// For a tall item that is anchored at the bottom of the viewport (the most
/// recent transcript entry), this is `max(0, item_height - scroll_offset -
/// viewport_height)` — the portion of the item that overflows above the
/// visible area.  When the item does not overflow above the viewport
/// (i.e. it fits between the bottom of the viewport and somewhere inside it),
/// this is `0`.
pub type PartialBottomItem = Option<(IndexInHeightLog, Rect, usize)>;

pub struct ScrollState {
    pub last_max_position: usize,
    pub position: usize,
    pub follow: bool,
    render_height_cache: HashMap<u16, Vec<usize>>,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollState {
    pub fn new() -> Self {
        Self {
            last_max_position: 0,
            position: 0,
            follow: true,
            render_height_cache: HashMap::new(),
        }
    }

    pub const fn scroll_down(&mut self) -> bool {
        let value_change = self.position < self.last_max_position;
        if value_change {
            self.position = self.position.saturating_add(1);
        }
        // When scrolled all the way to the bottom, auto follow the feed ("sticky behavior")
        self.follow = self.position == self.last_max_position;

        value_change
    }

    pub const fn scroll_up(&mut self) -> bool {
        self.follow = false;

        // Can't scroll up when already on top
        let value_change = self.position > 0;
        self.position = self.position.saturating_sub(1);

        value_change
    }

    fn get_height_log_from_cache_for_width(
        &mut self,
        width: u16,
        num_elements: usize,
    ) -> &Vec<usize> {
        let list = self.render_height_cache.entry(width).or_insert_with(|| {
            let tmp_height_list = vec![1; num_elements];
            tmp_height_list
        });
        for _ in 0..num_elements - list.len() {
            list.push(1);
        }
        list
    }

    /// Computes the `position` needed to scroll such that `item_index` is visible.
    /// Uses the height cache for `viewport_width`. If cache is empty or incomplete
    /// for this width, it returns a best-effort position.
    pub fn scroll_position_to_show_item(
        &mut self,
        item_index: usize,
        viewport_width: u16,
        viewport_height: usize,
        num_elements: usize,
    ) -> usize {
        let height_log = self.get_height_log_from_cache_for_width(viewport_width, num_elements);

        let top_offset: usize = height_log.iter().take(item_index).sum();
        let item_height = height_log.get(item_index).copied().unwrap_or(1);

        let max_scroll_offset = height_log
            .iter()
            .sum::<usize>()
            .saturating_sub(viewport_height);
        if max_scroll_offset == 0 {
            return 0; // Everything fits
        }

        // `position` in this widget = distance from top (0 = top, max_scroll_offset = bottom/follow).
        // The render loop converts: scroll_offset = max_scroll_offset - position.
        // So position = desired_scroll_offset (lines of content above the viewport top).
        //
        // We want the item centred, or top-aligned if it is taller than the viewport.
        if item_height >= viewport_height {
            top_offset // align top of item with top of viewport
        } else {
            top_offset.saturating_sub((viewport_height - item_height) / 2) // centre
        }
        .min(max_scroll_offset)
    }

    fn render_scrollbar(
        frame: &mut Frame,
        area: Rect,
        scroll_position: usize,
        max_scroll_offset: usize,
    ) {
        use ratatui_core::layout::Margin;
        use ratatui_widgets::scrollbar::{Scrollbar, ScrollbarState};

        let scrollbar = Scrollbar::default()
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll_offset).position(max_scroll_offset - scroll_position);
        frame.render_stateful_widget(
            scrollbar,
            area.outer(Margin {
                vertical: 0,
                horizontal: 1,
            }),
            &mut scrollbar_state,
        );
    }

    /// Renders the currently visible elements according to the scroll position.
    /// The `render_element` closure receives a reference to an element and has to
    /// return the expected height of the rendered element and a widget that renders it.
    /// The closure might get called zero or more times.
    pub fn render<'a, Element, ElementWidget, F>(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        elements: &'a [Element],
        render_element: F,
    ) where
        ElementWidget: Widget,
        F: Fn(&'a Element) -> (ElementHeight, ElementWidget),
    {
        //                                      |
        // ┌────────┐                  ┌────────▼────────┐
        // │ Height │ Get last heights │ Calculate which │
        // │ Cache  ├──────────────────► Elements will   │
        // └───▲────┘                  │ be visible      │
        //     │                       └────────┬────────┘
        //     │                                │
        //     │                       ┌────────▼────────┐
        //     │                       │ Try to render   │
        //     │                       │ the visible     │
        // ┌───┴─────────┐             │ elements        │
        // │ Update the  │             └────────┬────────┘
        // │ cache with  │                      │
        // │ new heights │             ┌────────▼────────┐
        // └───▲─────────┘             │ Does the height │
        //     │          ┌──┐         │ of rendered     │
        //     └──────────┤No◄─────────┤ elements match  │
        //                └──┘         │ the height in   │
        //                             │ the cache?      │
        //                             └────────┬────────┘
        //                                      │
        //                                    ┌─▼─┐
        //                                    │Yes│
        //                                    └─┬─┘
        //                                      │
        //                               ┌──────▼──────┐
        //                               │ Display the │
        //                               │ result!     │
        //                               └─────────────┘

        let viewport_height = area.height as usize;
        let viewport_width = area.width;

        let mut height_log = self
            .get_height_log_from_cache_for_width(viewport_width, elements.len())
            .clone();

        // Loop until we successfully rendert the current scroll position
        // Render attempts might fail if the cache is out of sync
        loop {
            let total_height: usize = height_log.iter().sum();

            // No need to scroll at all, when everything fits on the screen
            let max_scroll_offset = total_height.saturating_sub(viewport_height);

            if self.follow {
                self.position = max_scroll_offset;
            }

            // Inverted scrolling behaviour
            let scroll_offset = max_scroll_offset.saturating_sub(self.position);

            // Try rendering. This updates the height_log regardless of its success
            let result = try_render_scroll_state(
                frame,
                area,
                scroll_offset,
                &mut height_log,
                elements,
                &render_element,
            );

            // All good? Update the cache and end the endless loop
            if result.is_ok() {
                // Update the cache
                self.render_height_cache.insert(viewport_width, height_log);

                self.last_max_position = max_scroll_offset;

                // More content than fits on the screen? Show scrollbar
                if total_height > viewport_height {
                    Self::render_scrollbar(frame, area, scroll_offset, max_scroll_offset);
                }

                // End loop
                break;
            }
        }
    }
}

/// This functions tries to render the elements that are visible with the given scroll position.
///
/// To figure out, how many elements to scroll (partially) past, it looks up the elements
/// height (in lines) in the provided `height_log`.
/// Once it starts rendering, it compares the actual height of the rendering result with the height
/// from the log. If they differ, the incorrect height inside the log gets corrected / replaced.
/// Only if the height of all rendered elements matches the heights in the log will this function
/// return `Ok(())`, otherwise it returns `Err(())`.
///
/// Because this functions updates the height log (if necessary) on every call, you can call it
/// repeatedly with the same height log and eventually you will get a `Ok(())` result.
///
/// This design allows to have caching for the height log, that does not need to be updated directly
/// but can get out of sync over time, as the rendering will update the cache on-the-fly with this
/// function. The caching is a performance optimisation: To calculate which element(s) to show,
/// given a scroll position, you need the height of every element in the list. Because the height of
/// a element depends on the width of the viewable area (e.g. line wrapping), the height can often
/// only be known after a full rendering. This means that you will have to render *every* element
/// in the list at least once, to figure out which element to show.
/// The `height_log` caches the height after rendering and can then be used for the next rendering.
#[allow(clippy::result_unit_err)]
pub fn try_render_scroll_state<'a, Element, ElementWidget, F>(
    frame: &mut Frame,
    draw_area: Rect,
    scroll_positon: usize,
    height_log: &mut [ElementHeight],
    elements: &'a [Element],
    render_element: F,
) -> Result<(), ()>
where
    ElementWidget: Widget,
    F: Fn(&'a Element) -> (ElementHeight, ElementWidget),
{
    use ratatui_core::buffer::Buffer;

    fn copy_partial_top_widget_to_frame(
        frame: &mut Frame,
        area: Rect,
        buffer: Buffer,
        widget_height: usize,
    ) {
        // Only take what can be shown on screen
        let visible_content = buffer
            .content
            .into_iter()
            // Skip all content that is outside the visible area
            // (.skip() is cell-wise, so we multiply with the width to get from number of lines to cells)
            .skip(area.width as usize * (widget_height - area.height as usize))
            .take(area.area() as usize);
        for (i, cell) in visible_content.enumerate() {
            let x = i as u16 % area.width;
            let y = i as u16 / area.width;
            frame.buffer_mut()[(area.x + x, area.y + y)] = cell;
        }
    }

    fn copy_partial_bottom_widget_to_frame(
        frame: &mut Frame,
        area: Rect,
        buffer: Buffer,
        lines_above: usize,
    ) {
        // Skip the lines of this item that are scrolled above the viewport,
        // then take only as many lines as fit in the visible area.
        // This is analogous to copy_partial_top_widget_to_frame which also
        // skips a leading portion before copying.
        let visible_content = buffer
            .content
            .into_iter()
            .skip(area.width as usize * lines_above)
            .take(area.area() as usize);
        for (i, cell) in visible_content.enumerate() {
            let x = i as u16 % area.width;
            let y = i as u16 / area.width;
            frame.buffer_mut()[(area.x + x, area.y + y)] = cell;
        }
    }

    let mut update_needed = false;
    let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
        get_areas_to_render_from_scroll_position(draw_area, scroll_positon, height_log);

    if let Some((index, area)) = partial_draw_top {
        let element = &elements[index];
        let (widget_height, widget) = render_element(element);

        if widget_height == height_log[index] {
            let buffer_area = Rect::new(
                0,
                0,
                area.width,
                widget_height.try_into().unwrap_or(u16::MAX),
            );
            let mut buffer = Buffer::empty(buffer_area);

            widget.render(buffer.area, &mut buffer);
            copy_partial_top_widget_to_frame(frame, area, buffer, widget_height);
        } else {
            height_log[index] = widget_height;
            update_needed = true;
        }
    }

    if let Some((index, area, lines_above)) = partial_draw_bottom {
        let element = &elements[index];
        let (widget_height, widget) = render_element(element);

        if widget_height == height_log[index] {
            let buffer_area = Rect::new(
                0,
                0,
                area.width,
                widget_height.try_into().unwrap_or(u16::MAX),
            );
            let mut buffer = Buffer::empty(buffer_area);

            widget.render(buffer.area, &mut buffer);
            copy_partial_bottom_widget_to_frame(frame, area, buffer, lines_above);
        } else {
            height_log[index] = widget_height;
            update_needed = true;
        }
    }

    if let Some((range, area)) = full_draw_middle {
        use ratatui_core::layout::{Constraint, Layout};
        let mut widget_blocks = vec![];
        let mut constrains = vec![];
        for index in range {
            let element = &elements[index];
            let (height, widget) = render_element(element);

            if height != height_log[index] {
                height_log[index] = height;
                update_needed = true;
            }

            if !update_needed {
                widget_blocks.push(widget);
                constrains.push(Constraint::Length(height.try_into().unwrap()));
            }
        }

        if !update_needed {
            let areas: Vec<Rect> = Layout::vertical(constrains).split(area).to_vec();
            for (a, widget) in zip(areas, widget_blocks) {
                widget.render(a, frame.buffer_mut());
            }
        }
    }

    if update_needed { Err(()) } else { Ok(()) }
}

/// Scrolling for arbitrary sized items.
///
/// Given an array of your items height (`height_log`), a scroll offset (`scroll_offset`) and
/// the area (`area`) where the items will (later) be rendered into, returns which items will
/// be shown and where they need to be rendered.  
pub fn get_areas_to_render_from_scroll_position(
    area: Rect,
    mut scroll_offset: usize,
    height_log: &[ElementHeight],
) -> (PartialTopItem, FullItems, PartialBottomItem) {
    // These are going to be our return values
    let mut area_for_partial_draw_top = None;
    let mut area_for_fully_drawn = None;
    let mut area_for_partial_draw_bottom = None;

    // The entire viewspace
    let viewable_space = usize::from(area.height);
    // Our iterator index, we iterate backwards
    let mut current_index = height_log.len();

    // If we need to compute the area of a partially drawn Item on the top
    let mut has_partial_item_top: Option<IndexInHeightLog> = None;
    // .. and/or on the bottom
    let mut has_partial_item_bottom: Option<IndexInHeightLog> = None;

    let mut middle_space_available = viewable_space;
    let mut middle_space_used = 0;

    // Scroll backwards through the items until:
    // there are no items left
    // or we have no scroll offset left
    // or we still have scroll offset left but its too little for the next item
    while scroll_offset > 0 && current_index > 0 {
        current_index -= 1;
        let current_item_height = height_log[current_index];
        // Do we have enough scroll_offset left to scroll past the current item?
        if current_item_height > scroll_offset {
            // No, we don't.
            // So the current item will be drawn at the bottom
            // (At the bottom because it is the most recent item)
            has_partial_item_bottom = Some(current_index);
            break;
        }
        // Yes we can completly scroll past this item
        scroll_offset -= current_item_height;
    }

    // At which item do we need to stop drawing?
    // When we have a partial item at the bottom, that item is also the stop item.
    // If we don't have a partial item at the bottom,
    // the stop item is the lastest item, it's just outside our viewable space.
    // e.g. if we have a scroll_offset of 0 (no scrolling going on), the stop item
    // will be `current_index`'s initial value of height_log.len().
    let stop_item_full_drawn = current_index;

    if let Some(index) = has_partial_item_bottom {
        // Calculate the amount of space of the partial item that is still
        // inside the viewable area.
        let remaining_item_bottom_height_after_scrolling = height_log[index] - scroll_offset;
        let partial_item_bottom_height =
            // There is one edge case, where the item is so big, it overflows the
            // viewable area. In that case, there is no space left for anything else,
            // and the item is limited to the viewable space.
            if remaining_item_bottom_height_after_scrolling > viewable_space {
                middle_space_available = 0;
                viewable_space
            } else {
                // Calculate how much space remains for other items
                middle_space_available -= remaining_item_bottom_height_after_scrolling;
                remaining_item_bottom_height_after_scrolling
            };

        // Lines of this item that are above the *top* of the viewport — the
        // portion that overflows upward beyond what the viewport can show.
        // Equals `(height - scroll_offset) - viewport_height` when the item
        // overflows the top of the viewport, otherwise `0`.
        //
        // The renderer skips this many lines from the start of the item's
        // buffer before copying.  Passing `scroll_offset` here (which counts
        // lines below the viewport, not above) makes the visible window
        // through a tall item slide in the wrong direction as the user
        // scrolls — the bug fixed here.
        let lines_above_viewport =
            remaining_item_bottom_height_after_scrolling.saturating_sub(viewable_space);

        area_for_partial_draw_bottom = Some((
            index,
            Rect {
                // We add the remaining space as we start drawing from the bottom up
                y: area.y
                    + middle_space_available
                        .try_into()
                        .unwrap_or(u16::MAX - area.y),
                height: partial_item_bottom_height.try_into().unwrap_or(u16::MAX),
                ..area
            },
            lines_above_viewport,
        ));
    }

    // Calculate how many items fit inside the remaining available space in the middle.
    // (in the middle between Option<partially top> and Option<partially bottom>)
    // We do that until:
    // There are not items left
    // or there is no space left to fit the next item
    while current_index > 0 && middle_space_available > 0 {
        current_index -= 1;
        let current_item_height = height_log[current_index];
        // Does the item fit into the remaining space?
        if current_item_height > middle_space_available {
            // No it does not.
            // This means we need to have a partially drawn item at the top
            has_partial_item_top = Some(current_index);

            // This is going to be the start item for the fully drawn items.
            // Since the current item is already only partially drawn, we offset by 1.
            current_index += 1;
            break;
        }
        middle_space_available -= current_item_height;

        // Track how much space we covered with fully drawn items.
        middle_space_used += current_item_height;
    }

    // Just a rename for clarity. This tells us where with which item we should start
    // drawing the fully drawn items.
    let start_item_full_draw = current_index;

    // How much space is covered by the top item that is only partially drawn (if any)?
    let remaining_space_top = if let Some(index) = has_partial_item_top {
        let remaining_space_top = middle_space_available.try_into().unwrap_or(u16::MAX);
        area_for_partial_draw_top = Some((
            index,
            Rect {
                height: remaining_space_top,
                ..area
            },
        ));
        remaining_space_top
    } else {
        // If we don't have a partial top item, it covers no space at all
        0
    };

    // Only draw full items...if we actually managed to fit any.
    if middle_space_used > 0 {
        area_for_fully_drawn = Some((
            start_item_full_draw..stop_item_full_drawn,
            Rect {
                // Offset to not overlap with the partially drawn top item (if any)
                y: area.y + remaining_space_top,
                height: middle_space_used.try_into().unwrap_or(u16::MAX),
                ..area
            },
        ));
    }

    (
        area_for_partial_draw_top,
        area_for_fully_drawn,
        area_for_partial_draw_bottom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_item_no_fit_no_scroll() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [14];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_some());
        let (index, area) = partial_draw_top.unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 10,
                width: 5,
            }
        );
        assert!(full_draw_middle.is_none());
        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn single_item_no_fit_with_partial_scroll() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 2;
        let height_log = [14];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());
        assert!(full_draw_middle.is_none());
        assert!(partial_draw_bottom.is_some());
        let (index, area, _lines_above) = partial_draw_bottom.unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 10,
                width: 5,
            }
        );
    }

    #[test]
    fn single_item_no_fit_full_scroll() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 4;
        let height_log = [14];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());
        assert!(full_draw_middle.is_none());
        assert!(partial_draw_bottom.is_some());
        let (index, area, _lines_above) = partial_draw_bottom.unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 10,
                width: 5,
            }
        );
    }

    #[test]
    fn single_item_fits_with_remaining_space() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [4];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 0..1);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 4,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn single_item_fits_perfect() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [10];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 0..1);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 10,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn two_item_fits_with_remaining_space() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [4, 5];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 0..2);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 4 + 5,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn two_item_fits_perfect() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [4, 6];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 0..2);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 4 + 6,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn three_item_no_fit() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 0;
        let height_log = [4, 5, 3];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_some());
        let (index, area) = partial_draw_top.unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 2,
                width: 5,
            }
        );

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 1..3);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 2,
                height: 5 + 3,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn three_item_no_fit_scrolled() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 1;
        let height_log = [4, 5, 3];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_some());
        let (index, area) = partial_draw_top.unwrap();
        assert_eq!(index, 0);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 3,
                width: 5,
            }
        );

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 1..2);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 3,
                height: 5,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_some());
        let (index, area, _lines_above) = partial_draw_bottom.unwrap();
        assert_eq!(index, 2);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 8,
                height: 2,
                width: 5,
            }
        );
    }

    #[test]
    fn two_item_perfect_fit_scrolled() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 3;
        let height_log = [4, 6, 3];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 0..2);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 4 + 6,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn three_item_perfect_fit_scrolled() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 30;
        // ....................|.......|..<< 30...
        let height_log = [4, 6, 3, 6, 1, 20, 9, 1];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_none());

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 2..5);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 3 + 6 + 1,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_none());
    }

    #[test]
    fn three_item_no_fit_scrolled_far() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let scroll_offset = 33;
        // ..................|.....|.....<< 33...
        let height_log = [4, 6, 3, 6, 1, 20, 9, 1];

        let (partial_draw_top, full_draw_middle, partial_draw_bottom) =
            get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);

        assert!(partial_draw_top.is_some());
        let (index, area) = partial_draw_top.unwrap();
        assert_eq!(index, 1);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                height: 3,
                width: 5,
            }
        );

        let (range, area) = full_draw_middle.unwrap();
        assert_eq!(range, 2..3);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 3,
                height: 3,
                width: 5,
            }
        );

        assert!(partial_draw_bottom.is_some());
        let (index, area, _lines_above) = partial_draw_bottom.unwrap();
        assert_eq!(index, 3);
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 6,
                height: 4,
                width: 5,
            }
        );
    }

    /// `lines_above_viewport` (the third tuple field of `PartialBottomItem`)
    /// must equal the number of item lines scrolled past the *top* of the
    /// viewport, not the number scrolled past the bottom.  These two values
    /// only coincide at one specific scroll position; everywhere else,
    /// passing the wrong one makes the visible window through a tall item
    /// slide in the wrong direction.
    ///
    /// For a single item with H = 20 in a viewport with V = 10:
    ///
    /// | scroll_offset (S) | expected lines_above = max(0, H - S - V) |
    /// |-------------------|------------------------------------------|
    /// |                 1 |                                        9 |
    /// |                 5 |                                        5 |
    /// |                10 |                                        0 |
    /// |                15 |                                        0 (item no longer overflows top) |
    #[test]
    fn partial_bottom_lines_above_is_overflow_above_not_offset_below() {
        let area = Rect {
            x: 0,
            y: 0,
            height: 10,
            width: 5,
        };
        let height_log = [20];

        for (scroll_offset, expected_lines_above) in [(1, 9), (5, 5), (10, 0), (15, 0)] {
            let (_, _, partial_bottom) =
                get_areas_to_render_from_scroll_position(area, scroll_offset, &height_log);
            let (_, _, lines_above) = partial_bottom.unwrap_or_else(|| {
                panic!("expected partial_bottom for scroll_offset={scroll_offset}")
            });
            assert_eq!(
                lines_above, expected_lines_above,
                "scroll_offset={scroll_offset}: lines_above must be max(0, H-S-V), not S"
            );
        }
    }

    // ========================================
    // Tests for scroll_position_to_show_item
    // ========================================

    /// Helper to create a ScrollState with a pre-populated height cache.
    fn scroll_state_with_heights(width: u16, heights: Vec<usize>) -> ScrollState {
        let mut state = ScrollState::new();
        state.render_height_cache.insert(width, heights);
        state
    }

    /// Test 1: Everything fits in viewport — returns 0
    #[test]
    fn scroll_position_to_show_item_everything_fits() {
        let mut state = scroll_state_with_heights(80, vec![2, 3, 2]); // total = 7
        let viewport_height = 10;

        // Request any item — should return 0 since all fit
        let pos = state.scroll_position_to_show_item(1, 80, viewport_height, 3);
        assert_eq!(pos, 0, "all items fit, position should be 0");
    }

    /// Test 2: Item at top (first item) with content taller than viewport — returns 0
    #[test]
    fn scroll_position_to_show_item_first_item_returns_zero() {
        let mut state = scroll_state_with_heights(80, vec![5, 5, 5, 5]); // total = 20
        let viewport_height = 10;

        let pos = state.scroll_position_to_show_item(0, 80, viewport_height, 4);
        assert_eq!(pos, 0, "first item should be at position 0");
    }

    /// Test 3: Item in middle — returns offset that centers the item
    #[test]
    fn scroll_position_to_show_item_middle_item_centered() {
        // heights: [3, 3, 3, 3, 3] = 15 total
        // viewport_height = 10, so max_scroll_offset = 15 - 10 = 5
        // item_index = 2, top_offset = 3 + 3 = 6
        // centering: 6 - (10 - 3) / 2 = 6 - 3 = 3, clamped to max_scroll_offset = 5
        let mut state = scroll_state_with_heights(80, vec![3, 3, 3, 3, 3]);
        let viewport_height = 10;

        // Item at index 2: top_offset = items[0] + items[1] = 6
        // Centered: 6 - (10-3)/2 = 6 - 3 = 3
        let pos = state.scroll_position_to_show_item(2, 80, viewport_height, 5);
        assert_eq!(pos, 3, "middle item should be centered");
    }

    /// Test 4: Item at bottom (last item) — returns max_scroll_offset
    #[test]
    fn scroll_position_to_show_item_last_item_returns_max() {
        // heights: [2, 3, 4, 6] = 15 total
        // viewport_height = 10, max_scroll_offset = 5
        // item_index = 3, top_offset = 2 + 3 + 4 = 9
        // item_height = 6, centering: 9 - (10-6)/2 = 9 - 2 = 7, clamped to 5
        let mut state = scroll_state_with_heights(80, vec![2, 3, 4, 6]);
        let viewport_height = 10;

        let pos = state.scroll_position_to_show_item(3, 80, viewport_height, 4);
        assert_eq!(pos, 5, "last item should be at max_scroll_offset");
    }

    /// Test 5: Item taller than viewport — top-aligns (returns top_offset, clamped to max)
    #[test]
    fn scroll_position_to_show_item_taller_than_viewport_top_aligned() {
        // heights: [3, 15, 3] = 21 total
        // viewport_height = 10, max_scroll_offset = 11
        // item_index = 1, top_offset = 3, item_height = 15 >= viewport_height
        // Since item is taller, top-align: return 3
        let mut state = scroll_state_with_heights(80, vec![3, 15, 3]);
        let viewport_height = 10;

        let pos = state.scroll_position_to_show_item(1, 80, viewport_height, 3);
        assert_eq!(pos, 3, "tall item should be top-aligned at its top_offset");
    }

    /// Test 5b: Item taller than viewport at bottom — clamped to max_scroll_offset
    #[test]
    fn scroll_position_to_show_item_taller_than_viewport_clamped() {
        // heights: [5, 20] = 25 total
        // viewport_height = 10, max_scroll_offset = 15
        // item_index = 1, top_offset = 5, item_height = 20 >= viewport_height
        // Top-align would be 5, but it's already within bounds
        let mut state = scroll_state_with_heights(80, vec![5, 20]);
        let viewport_height = 10;

        let pos = state.scroll_position_to_show_item(1, 80, viewport_height, 2);
        assert_eq!(pos, 5, "tall item at bottom should be at its top_offset");
    }

    /// Test 6: Empty/no cache — returns best-effort (doesn't panic)
    #[test]
    fn scroll_position_to_show_item_empty_cache_no_panic() {
        let mut state = ScrollState::new(); // No cache populated
        let viewport_height = 10;

        // Without cache, get_height_log_from_cache_for_width creates vec![1; n]
        // For 5 elements: heights = [1, 1, 1, 1, 1], total = 5 < viewport_height
        // So max_scroll_offset = 0, returns 0
        let pos = state.scroll_position_to_show_item(2, 80, viewport_height, 5);
        assert_eq!(
            pos, 0,
            "empty cache creates default heights, everything fits"
        );
    }

    /// Test 6b: Empty cache with more elements than viewport height
    #[test]
    fn scroll_position_to_show_item_empty_cache_with_many_elements() {
        let mut state = ScrollState::new();
        let viewport_height = 5;

        // 10 elements with default height 1 each = 10 total
        // max_scroll_offset = 10 - 5 = 5
        // item_index = 5, top_offset = 5, item_height = 1
        // centering: 5 - (5-1)/2 = 5 - 2 = 3
        let pos = state.scroll_position_to_show_item(5, 80, viewport_height, 10);
        assert_eq!(
            pos, 3,
            "empty cache with many elements should center the item"
        );
    }
}
