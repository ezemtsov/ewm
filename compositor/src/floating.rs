//! Per-output floating Emacs frame space; durable positions stored as a working-area fraction.

use std::iter::zip;

use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::LayoutEntry;
use crate::strip::{DEFAULT_ANIM_DURATION, Frame, ViewAnimation};

/// By how many logical pixels command-driven floating frame moves nudge frames.
pub const DIRECTIONAL_MOVE_PX: f64 = 50.0;
pub const DEFAULT_FRAME_WIDTH: f64 = 640.0;
pub const DEFAULT_FRAME_HEIGHT: f64 = 480.0;
const MIN_FRAME_WIDTH: f64 = 160.0;
const MIN_FRAME_HEIGHT: f64 = 120.0;

/// Bitflag set of frame resize edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResizeEdge(u32);

impl ResizeEdge {
    pub const TOP: Self = Self(0b0001);
    pub const BOTTOM: Self = Self(0b0010);
    pub const LEFT: Self = Self(0b0100);
    pub const RIGHT: Self = Self(0b1000);

    pub const TOP_LEFT: Self = Self(Self::TOP.0 | Self::LEFT.0);
    pub const BOTTOM_LEFT: Self = Self(Self::BOTTOM.0 | Self::LEFT.0);
    pub const TOP_RIGHT: Self = Self(Self::TOP.0 | Self::RIGHT.0);
    pub const BOTTOM_RIGHT: Self = Self(Self::BOTTOM.0 | Self::RIGHT.0);
    pub const LEFT_RIGHT: Self = Self(Self::LEFT.0 | Self::RIGHT.0);
    pub const TOP_BOTTOM: Self = Self(Self::TOP.0 | Self::BOTTOM.0);

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn cursor_icon(self) -> CursorIcon {
        match self {
            Self::LEFT => CursorIcon::WResize,
            Self::RIGHT => CursorIcon::EResize,
            Self::TOP => CursorIcon::NResize,
            Self::BOTTOM => CursorIcon::SResize,
            Self::TOP_LEFT => CursorIcon::NwResize,
            Self::TOP_RIGHT => CursorIcon::NeResize,
            Self::BOTTOM_RIGHT => CursorIcon::SeResize,
            Self::BOTTOM_LEFT => CursorIcon::SwResize,
            _ => CursorIcon::Default,
        }
    }
}

impl std::ops::BitOrAssign for ResizeEdge {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy)]
struct InteractiveMove {
    frame_id: u64,
    original_pos: Point<f64, Logical>,
}

#[derive(Debug, Clone, Copy)]
struct InteractiveResize {
    frame_id: u64,
    original_pos: Point<f64, Logical>,
    original_size: Size<f64, Logical>,
    edges: ResizeEdge,
}

/// Position relative to the working area.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct SizeFracPoint {
    pub x: f64,
    pub y: f64,
}

/// Extra per-floating-frame data.
#[derive(Debug)]
pub struct Data {
    /// Position relative to the working area.
    pos: SizeFracPoint,

    /// Cached logical position, not rounded to physical pixels.
    logical_pos: Point<f64, Logical>,

    /// Cached logical size.
    size: Size<f64, Logical>,

    /// Working area used for conversions.
    working_area: Rectangle<f64, Logical>,

    move_anim: Option<ViewAnimation>,

    shadow: crate::floating_shadow::Shadow,
}

impl Data {
    pub fn new(
        working_area: Rectangle<f64, Logical>,
        frame: &Frame,
        logical_pos: Point<f64, Logical>,
    ) -> Self {
        let mut rv = Self {
            pos: SizeFracPoint::default(),
            logical_pos: Point::default(),
            size: Self::frame_size(frame, working_area),
            working_area,
            move_anim: None,
            shadow: crate::floating_shadow::Shadow::new(
                crate::shadow_style::Shadow::floating_default(),
            ),
        };
        rv.set_logical_pos(logical_pos);
        rv
    }

    pub fn frame_size(frame: &Frame, working_area: Rectangle<f64, Logical>) -> Size<f64, Logical> {
        Size::from((
            if frame.width > 0.0 {
                frame.width
            } else {
                DEFAULT_FRAME_WIDTH
            }
            .max(1.0),
            if frame.height > 0.0 {
                frame.height
            } else {
                DEFAULT_FRAME_HEIGHT.min(working_area.size.h.max(1.0))
            }
            .max(1.0),
        ))
    }

    pub fn scale_by_working_area(
        area: Rectangle<f64, Logical>,
        pos: SizeFracPoint,
    ) -> Point<f64, Logical> {
        Point::from((
            area.loc.x + pos.x * area.size.w,
            area.loc.y + pos.y * area.size.h,
        ))
    }

    pub fn logical_to_size_frac_in_working_area(
        area: Rectangle<f64, Logical>,
        logical_pos: Point<f64, Logical>,
    ) -> SizeFracPoint {
        SizeFracPoint {
            x: (logical_pos.x - area.loc.x) / area.size.w.max(1.0),
            y: (logical_pos.y - area.loc.y) / area.size.h.max(1.0),
        }
    }

    fn recompute_logical_pos(&mut self) {
        let mut logical_pos = Self::scale_by_working_area(self.working_area, self.pos);

        // Keep at least a Mutter-like sliver reachable on-screen.
        let min_on_screen_hor = f64::clamp(self.size.w / 4.0, 10.0, 75.0);
        let min_on_screen_ver = f64::clamp(self.size.h / 4.0, 10.0, 75.0);
        let max_off_screen_hor = f64::max(0.0, self.size.w - min_on_screen_hor);
        let max_off_screen_ver = f64::max(0.0, self.size.h - min_on_screen_ver);

        logical_pos -= self.working_area.loc;
        logical_pos.x = f64::max(logical_pos.x, -max_off_screen_hor);
        logical_pos.y = f64::max(logical_pos.y, -max_off_screen_ver);
        logical_pos.x = f64::min(
            logical_pos.x,
            self.working_area.size.w - self.size.w + max_off_screen_hor,
        );
        logical_pos.y = f64::min(
            logical_pos.y,
            self.working_area.size.h - self.size.h + max_off_screen_ver,
        );
        logical_pos += self.working_area.loc;

        self.logical_pos = logical_pos;
    }

    pub fn update_config(&mut self, working_area: Rectangle<f64, Logical>) {
        if self.working_area == working_area {
            return;
        }
        self.working_area = working_area;
        self.recompute_logical_pos();
    }

    pub fn update(&mut self, frame: &Frame) {
        self.set_size(Self::frame_size(frame, self.working_area));
    }

    pub fn set_size(&mut self, size: Size<f64, Logical>) {
        if self.size == size {
            return;
        }
        self.size = size;
        self.recompute_logical_pos();
    }

    pub fn logical_pos(&self) -> Point<f64, Logical> {
        self.logical_pos
    }

    pub fn size(&self) -> Size<f64, Logical> {
        self.size
    }

    pub fn render_pos(&self) -> Point<f64, Logical> {
        self.logical_pos + self.move_offset()
    }

    pub fn rect(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(self.logical_pos, self.size)
    }

    pub fn shadow_mut(&mut self) -> &mut crate::floating_shadow::Shadow {
        &mut self.shadow
    }

    pub fn set_logical_pos(&mut self, logical_pos: Point<f64, Logical>) {
        self.pos = Self::logical_to_size_frac_in_working_area(self.working_area, logical_pos);
        self.recompute_logical_pos();
    }

    pub fn move_offset(&self) -> Point<f64, Logical> {
        Point::from((
            self.move_anim.as_ref().map_or(0.0, ViewAnimation::value),
            0.0,
        ))
    }

    pub fn set_move_anim(&mut self, from_x: f64, clock: crate::strip::AnimationsClock) {
        self.move_anim = Some(ViewAnimation::new(
            clock,
            from_x,
            0.0,
            DEFAULT_ANIM_DURATION,
        ));
    }

    pub fn advance(&mut self) {
        if self.move_anim.as_ref().is_some_and(ViewAnimation::is_done) {
            self.move_anim = None;
        }
    }

    pub fn animations_ongoing(&self) -> bool {
        self.move_anim.is_some()
    }
}

/// Floating Emacs frames in top-to-bottom order.
#[derive(Debug, serde::Serialize)]
pub struct FloatingSpace {
    pub frames: Vec<Frame>,
    #[serde(skip)]
    data: Vec<Data>,
    active_frame_id: Option<u64>,
    #[serde(skip)]
    interactive_move: Option<InteractiveMove>,
    #[serde(skip)]
    interactive_resize: Option<InteractiveResize>,
    #[serde(skip)]
    working_area: Rectangle<f64, Logical>,
    #[serde(skip)]
    clock: crate::strip::AnimationsClock,
}

impl FloatingSpace {
    pub fn new(
        working_area: Rectangle<f64, Logical>,
        clock: crate::strip::AnimationsClock,
    ) -> Self {
        Self {
            frames: Vec::new(),
            data: Vec::new(),
            active_frame_id: None,
            interactive_move: None,
            interactive_resize: None,
            working_area,
            clock,
        }
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn update_config(&mut self, working_area: Rectangle<f64, Logical>) {
        self.working_area = working_area;
        for (frame, data) in zip(&self.frames, &mut self.data) {
            data.update(frame);
            data.update_config(working_area);
        }
    }

    pub fn add_frame(&mut self, frame: Frame) {
        let stored = self.stored_or_default_frame_pos(&frame);
        let size = Data::frame_size(&frame, self.working_area);
        let pos = stored.unwrap_or_else(|| self.centered_pos_for(size));
        let data = Data::new(self.working_area, &frame, pos);
        self.active_frame_id = Some(frame.surface_id);
        self.data.insert(0, data);
        self.frames.insert(0, frame);
    }

    fn centered_pos_for(&self, size: Size<f64, Logical>) -> Point<f64, Logical> {
        let x = self.working_area.loc.x + (self.working_area.size.w - size.w).max(0.0) / 2.0;
        let y = self.working_area.loc.y + (self.working_area.size.h - size.h).max(0.0) / 2.0;
        Point::from((x, y))
    }

    fn stored_or_default_frame_pos(&self, frame: &Frame) -> Option<Point<f64, Logical>> {
        frame
            .floating_pos
            .map(|pos| Data::scale_by_working_area(self.working_area, pos))
    }

    pub fn remove_frame(&mut self, id: u64) -> Option<Frame> {
        let idx = self.idx_of(id)?;
        self.interactive_move = self.interactive_move.filter(|move_| move_.frame_id != id);
        self.interactive_resize = self
            .interactive_resize
            .filter(|resize| resize.frame_id != id);
        self.data.remove(idx);
        let frame = self.frames.remove(idx);
        if self.active_frame_id == Some(id) {
            self.active_frame_id = self.frames.first().map(|frame| frame.surface_id);
        }
        Some(frame)
    }

    pub fn has_frame(&self, id: u64) -> bool {
        self.idx_of(id).is_some()
    }

    pub fn frame(&self, id: u64) -> Option<&Frame> {
        self.idx_of(id).and_then(|idx| self.frames.get(idx))
    }

    pub fn frame_mut(&mut self, id: u64) -> Option<&mut Frame> {
        let idx = self.idx_of(id)?;
        self.frames.get_mut(idx)
    }

    pub fn frame_with_data(&self, id: u64) -> Option<(&Frame, &Data)> {
        let idx = self.idx_of(id)?;
        Some((&self.frames[idx], &self.data[idx]))
    }

    pub fn frame_with_data_mut(&mut self, id: u64) -> Option<(&mut Frame, &mut Data)> {
        let idx = self.idx_of(id)?;
        Some((&mut self.frames[idx], &mut self.data[idx]))
    }

    pub fn idx_of(&self, id: u64) -> Option<usize> {
        self.frames.iter().position(|frame| frame.surface_id == id)
    }

    pub fn activate_frame(&mut self, id: u64) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        self.raise_frame(idx, 0);
        self.active_frame_id = Some(id);
        true
    }

    fn raise_frame(&mut self, from_idx: usize, to_idx: usize) {
        assert!(to_idx <= from_idx);
        let frame = self.frames.remove(from_idx);
        let data = self.data.remove(from_idx);
        self.frames.insert(to_idx, frame);
        self.data.insert(to_idx, data);
    }

    pub fn merge_layout_frames(&mut self, frames: Vec<Frame>) {
        let mut order = std::collections::HashSet::new();
        for mut new_frame in frames {
            order.insert(new_frame.surface_id);
            if let Some(idx) = self.idx_of(new_frame.surface_id) {
                let preserved_workspace_name = std::mem::take(&mut self.frames[idx].workspace_name);
                let preserved_fullscreen = self.frames[idx].fullscreen;
                let prev_size = self.data[idx].size();
                let was_auto = self.frames[idx].floating_pos.is_none();
                let preserved_pos = self.data[idx].pos;
                new_frame.floating_pos = (!was_auto).then_some(preserved_pos);
                new_frame.fullscreen = preserved_fullscreen.filter(|fullscreen| {
                    new_frame.entry_surface_id(fullscreen.entry_id) == Some(fullscreen.surface_id)
                });
                self.frames[idx] = new_frame;
                if self.frames[idx].workspace_name.is_none() {
                    self.frames[idx].workspace_name = preserved_workspace_name;
                }
                self.data[idx].update(&self.frames[idx]);
                // Auto-floated frames are added placeholder-sized; center once the real size lands.
                let recenter = was_auto && self.data[idx].size() != prev_size;
                if recenter {
                    let centered = self.centered_pos_for(self.data[idx].size());
                    self.data[idx].set_logical_pos(centered);
                }
                if let Some(resize) = self
                    .interactive_resize
                    .filter(|resize| resize.frame_id == self.frames[idx].surface_id)
                {
                    let size = self.data[idx].size();
                    let mut pos = self.data[idx].logical_pos();
                    if resize.edges.contains(ResizeEdge::LEFT) {
                        pos.x += prev_size.w - size.w;
                    }
                    if resize.edges.contains(ResizeEdge::TOP) {
                        pos.y += prev_size.h - size.h;
                    }
                    self.data[idx].set_logical_pos(pos);
                }
            } else {
                self.add_frame(new_frame);
            }
        }

        for (frame, data) in zip(&mut self.frames, &self.data) {
            if !order.contains(&frame.surface_id) {
                frame.entries.clear();
                frame.selected_entry_id = None;
                frame.fullscreen = None;
                // Persist only an already-explicit position; leave auto frames auto.
                if frame.floating_pos.is_some() {
                    frame.floating_pos = Some(data.pos);
                }
            }
        }
    }

    pub fn move_frame_by(&mut self, id: u64, delta: Point<f64, Logical>) -> bool {
        let clock = self.clock.clone();
        let Some((frame, data)) = self.frame_with_data_mut(id) else {
            return false;
        };
        let prev = data.logical_pos();
        data.set_logical_pos(prev + delta);
        frame.floating_pos = Some(data.pos);
        let diff = prev - data.logical_pos();
        if diff.x * diff.x + diff.y * diff.y > 10.0 * 10.0 {
            data.set_move_anim(diff.x, clock);
        }
        true
    }

    pub fn resize_frame_by(
        &mut self,
        id: u64,
        delta: Size<f64, Logical>,
    ) -> Option<Size<i32, Logical>> {
        let (frame, data) = self.frame_with_data_mut(id)?;
        let size = data.size();
        let new_size = Size::from((
            (size.w + delta.w).max(MIN_FRAME_WIDTH),
            (size.h + delta.h).max(MIN_FRAME_HEIGHT),
        ));
        frame.width = new_size.w;
        frame.height = new_size.h;
        data.set_size(new_size);
        Some(Size::from((
            new_size.w.round() as i32,
            new_size.h.round() as i32,
        )))
    }

    pub fn frame_under(&self, pos: Point<f64, Logical>) -> Option<(u64, Rectangle<f64, Logical>)> {
        self.frames_with_render_geo()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(frame, rect)| (frame.surface_id, rect))
    }

    pub fn resize_edges_under(&self, pos: Point<f64, Logical>) -> Option<(u64, ResizeEdge)> {
        self.frames_with_render_geo()
            .find(|(_, frame_rect)| frame_rect.contains(pos))
            .map(|(frame, _)| (frame.surface_id, ResizeEdge::BOTTOM_RIGHT))
    }

    pub fn interactive_move_begin(&mut self, frame_id: u64) -> bool {
        if self.interactive_move.is_some() {
            return false;
        }

        let Some((_, data)) = self.frame_with_data(frame_id) else {
            return false;
        };

        self.interactive_move = Some(InteractiveMove {
            frame_id,
            original_pos: data.logical_pos(),
        });

        true
    }

    pub fn interactive_move_update(&mut self, frame_id: u64, delta: Point<f64, Logical>) -> bool {
        let Some(move_) = self.interactive_move else {
            return false;
        };

        if frame_id != move_.frame_id {
            return false;
        }

        let Some((frame, data)) = self.frame_with_data_mut(frame_id) else {
            return false;
        };

        data.set_logical_pos(move_.original_pos + delta);
        frame.floating_pos = Some(data.pos);

        true
    }

    pub fn interactive_move_end(&mut self, frame_id: Option<u64>) {
        let Some(move_) = self.interactive_move else {
            return;
        };

        if let Some(frame_id) = frame_id
            && frame_id != move_.frame_id
        {
            return;
        }

        self.interactive_move = None;
    }

    pub fn interactive_resize_begin(&mut self, frame_id: u64, edges: ResizeEdge) -> bool {
        if edges.is_empty() || self.interactive_resize.is_some() {
            return false;
        }

        let Some((_, data)) = self.frame_with_data(frame_id) else {
            return false;
        };

        let resize = InteractiveResize {
            frame_id,
            original_pos: data.logical_pos(),
            original_size: data.size(),
            edges,
        };
        self.interactive_resize = Some(resize);

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        frame_id: u64,
        delta: Point<f64, Logical>,
    ) -> Option<Size<i32, Logical>> {
        let resize = self.interactive_resize?;

        if frame_id != resize.frame_id {
            return None;
        }

        let mut width_delta = 0.0;
        if resize.edges.intersects(ResizeEdge::LEFT_RIGHT) {
            width_delta = delta.x;
            if resize.edges.contains(ResizeEdge::LEFT) {
                width_delta = -width_delta;
            };
        }

        let mut height_delta = 0.0;
        if resize.edges.intersects(ResizeEdge::TOP_BOTTOM) {
            height_delta = delta.y;
            if resize.edges.contains(ResizeEdge::TOP) {
                height_delta = -height_delta;
            };
        }

        let new_size = Size::from((
            (resize.original_size.w + width_delta).max(MIN_FRAME_WIDTH),
            (resize.original_size.h + height_delta).max(MIN_FRAME_HEIGHT),
        ));
        let mut new_pos = resize.original_pos;
        if resize.edges.contains(ResizeEdge::LEFT) {
            new_pos.x += resize.original_size.w - new_size.w;
        }
        if resize.edges.contains(ResizeEdge::TOP) {
            new_pos.y += resize.original_size.h - new_size.h;
        }

        let (frame, data) = self.frame_with_data_mut(frame_id)?;
        frame.width = new_size.w;
        frame.height = new_size.h;
        data.set_size(new_size);
        data.set_logical_pos(new_pos);
        frame.floating_pos = Some(data.pos);

        Some(Size::from((
            new_size.w.round() as i32,
            new_size.h.round() as i32,
        )))
    }

    pub fn interactive_resize_end(&mut self, frame_id: Option<u64>) {
        let Some(resize) = self.interactive_resize else {
            return;
        };

        if let Some(frame_id) = frame_id
            && frame_id != resize.frame_id
        {
            return;
        }

        self.interactive_resize = None;
    }

    pub fn frames_with_render_geo(
        &self,
    ) -> impl Iterator<Item = (&Frame, Rectangle<f64, Logical>)> {
        self.frames
            .iter()
            .zip(self.data.iter())
            .map(|(frame, data)| (frame, Rectangle::new(data.render_pos(), data.size())))
    }

    pub fn frames_with_render_geo_mut(
        &mut self,
    ) -> impl Iterator<Item = (&Frame, &mut Data, Rectangle<f64, Logical>)> {
        self.frames
            .iter()
            .zip(self.data.iter_mut())
            .map(|(frame, data)| {
                let rect = Rectangle::new(data.render_pos(), data.size());
                (frame, data, rect)
            })
    }

    pub fn entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.frames.iter().flat_map(|f| f.entries.iter())
    }

    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.frames.iter_mut().flat_map(|f| f.entries.iter_mut())
    }

    pub fn surface_entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.entries().filter(|entry| entry.surface.is_some())
    }

    pub fn surface_entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.entries_mut().filter(|entry| entry.surface.is_some())
    }

    pub fn entries_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.frames
            .iter()
            .enumerate()
            .flat_map(|(i, f)| f.entries.iter().map(move |entry| (i, entry)))
    }

    pub fn surface_entries_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.entries_with_frame()
            .filter(|(_, entry)| entry.surface.is_some())
    }

    pub fn advance(&mut self) {
        for data in &mut self.data {
            data.advance();
        }
    }

    pub fn animations_ongoing(&self) -> bool {
        self.data.iter().any(Data::animations_ongoing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strip::AnimationsClock;

    fn test_space() -> FloatingSpace {
        let working_area = Rectangle::new(Point::from((0., 0.)), Size::from((1200., 800.)));
        FloatingSpace::new(working_area, AnimationsClock::default())
    }

    fn test_frame(id: u64) -> Frame {
        let mut frame = Frame::new(id);
        frame.width = 600.;
        frame.height = 300.;
        frame
    }

    #[test]
    fn resize_edges_under_uses_bottom_right_for_any_frame_hit() {
        let mut space = test_space();
        space.add_frame(test_frame(1));
        let (_, rect) = space.frame_under(Point::from((600., 300.))).unwrap();

        assert_eq!(
            space.resize_edges_under(rect.loc + Point::from((10., 10.))),
            Some((1, ResizeEdge::BOTTOM_RIGHT))
        );
        assert_eq!(
            space.resize_edges_under(rect.loc + Point::from((590., 290.))),
            Some((1, ResizeEdge::BOTTOM_RIGHT))
        );
        assert_eq!(
            space.resize_edges_under(rect.loc + Point::from((300., 150.))),
            Some((1, ResizeEdge::BOTTOM_RIGHT))
        );
        assert_eq!(
            space.resize_edges_under(rect.loc - Point::from((1., 1.))),
            None
        );
    }

    #[test]
    fn interactive_resize_left_top_keeps_opposite_edge_fixed() {
        let mut space = test_space();
        space.add_frame(test_frame(1));
        let (_, start_rect) = space.frame_under(Point::from((600., 300.))).unwrap();

        assert!(space.interactive_resize_begin(1, ResizeEdge::TOP_LEFT));
        let size = space
            .interactive_resize_update(1, Point::from((50., 40.)))
            .unwrap();
        let (_, end_rect) = space.frame_under(start_rect.loc).unwrap_or_else(|| {
            let (frame, rect) = space.frames_with_render_geo().next().unwrap();
            (frame.surface_id, rect)
        });

        assert_eq!(size, Size::from((550, 260)));
        assert_eq!(
            end_rect.loc.x + end_rect.size.w,
            start_rect.loc.x + start_rect.size.w
        );
        assert_eq!(
            end_rect.loc.y + end_rect.size.h,
            start_rect.loc.y + start_rect.size.h
        );
    }
}
