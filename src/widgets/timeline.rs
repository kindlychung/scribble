use druid::kurbo::{BezPath, Line};
use druid::theme;
use druid::{
    BoxConstraints, Color, Data, Env, Event, EventCtx, LayoutCtx, LifeCycle, LifeCycleCtx,
    PaintCtx, Point, Rect, RenderContext, Size, UpdateCtx, Widget, WidgetPod,
};
use std::collections::HashMap;

use crate::audio::{AudioSnippetData, AudioSnippetId, AudioSnippetsData};
use crate::data::{SnippetData, SnippetsData};
use crate::snippet::SnippetId;
use crate::snippet_layout;
use crate::ScribbleState;

const SNIPPET_HEIGHT: f64 = 20.0;
const MIN_NUM_ROWS: usize = 5;
const MIN_WIDTH: f64 = 100.0;
const PIXELS_PER_USEC: f64 = 40.0 / 1000000.0;
const TIMELINE_BG_COLOR: Color = Color::rgb8(0x66, 0x66, 0x66);
const CURSOR_COLOR: Color = Color::rgb8(0x10, 0x10, 0xaa);
const CURSOR_THICKNESS: f64 = 3.0;

const DRAW_SNIPPET_COLOR: Color = Color::rgb8(0x99, 0x99, 0x22);
const DRAW_SNIPPET_SELECTED_COLOR: Color = Color::rgb8(0x77, 0x77, 0x11);
const AUDIO_SNIPPET_COLOR: Color = Color::rgb8(0x55, 0x55, 0xBB);
const SNIPPET_STROKE_COLOR: Color = Color::rgb8(0x22, 0x22, 0x22);
const SNIPPET_HOVER_STROKE_COLOR: Color = Color::rgb8(0, 0, 0);
const SNIPPET_STROKE_THICKNESS: f64 = 1.0;

const MARK_COLOR: Color = Color::rgb8(0x33, 0x33, 0x99);

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Id {
    Drawing(SnippetId),
    Audio(AudioSnippetId),
}

#[derive(Clone, Data)]
enum Snip {
    Drawing(SnippetData),
    Audio(AudioSnippetData),
}

impl Snip {
    fn start_time(&self) -> i64 {
        match self {
            Snip::Audio(s) => s.start_time(),
            Snip::Drawing(d) => d.start_time(),
        }
    }

    fn end_time(&self) -> Option<i64> {
        match self {
            Snip::Audio(s) => Some(s.end_time()),
            Snip::Drawing(d) => d.end_time(),
        }
    }

    fn last_draw_time(&self) -> Option<i64> {
        match self {
            Snip::Audio(_) => None,
            Snip::Drawing(d) => {
                if d.end_time() != Some(d.last_draw_time()) {
                    Some(d.last_draw_time())
                } else {
                    None
                }
            }
        }
    }

    fn inner_lerp_times(&self) -> Vec<i64> {
        match self {
            Snip::Audio(_) => Vec::new(),
            Snip::Drawing(d) => {
                let lerps = d.lerp.times();
                let first_idx = lerps
                    .iter()
                    .position(|&x| x != lerps[0])
                    .unwrap_or(lerps.len());
                let last_idx = lerps
                    .iter()
                    .rposition(|&x| x != lerps[lerps.len() - 1])
                    .unwrap_or(0);
                if first_idx <= last_idx {
                    lerps[first_idx..=last_idx]
                        .iter()
                        .map(|&x| x - lerps[0])
                        .collect()
                } else {
                    Vec::new()
                }
            }
        }
    }
}

pub struct Timeline {
    snippet_offsets: HashMap<Id, usize>,
    num_rows: usize,
    children: HashMap<Id, WidgetPod<ScribbleState, TimelineSnippet>>,
}

impl Default for Timeline {
    fn default() -> Timeline {
        Timeline {
            snippet_offsets: HashMap::new(),
            num_rows: MIN_NUM_ROWS,
            children: HashMap::new(),
        }
    }
}

impl Timeline {
    fn recalculate_snippet_offsets(&mut self, snippets: &SnippetsData, audio: &AudioSnippetsData) {
        let draw_offsets = snippet_layout::layout(snippets.snippets());
        let audio_offsets = snippet_layout::layout(audio.snippets());
        self.num_rows = (draw_offsets.num_rows + audio_offsets.num_rows).max(MIN_NUM_ROWS);

        self.snippet_offsets.clear();
        self.children.clear();
        for (&id, &offset) in &draw_offsets.positions {
            let id = Id::Drawing(id);
            self.snippet_offsets.insert(id, offset);
            self.children
                .insert(id, WidgetPod::new(TimelineSnippet { id }));
        }
        for (&id, &offset) in &audio_offsets.positions {
            let id = Id::Audio(id);
            self.snippet_offsets.insert(id, self.num_rows - offset - 1);
            self.children
                .insert(id, WidgetPod::new(TimelineSnippet { id }));
        }
    }
}

struct TimelineSnippet {
    id: Id,
}

impl TimelineSnippet {
    fn snip(&self, data: &ScribbleState) -> Snip {
        match self.id {
            Id::Drawing(id) => Snip::Drawing(data.snippets.snippet(id).clone()),
            Id::Audio(id) => Snip::Audio(data.audio_snippets.snippet(id).clone()),
        }
    }

    fn width(&self, data: &ScribbleState) -> f64 {
        let snip = self.snip(data);
        if let Some(end_time) = snip.end_time() {
            (end_time - snip.start_time()) as f64 * PIXELS_PER_USEC
        } else {
            std::f64::INFINITY
        }
    }

    fn fill_color(&self, data: &ScribbleState) -> Color {
        match self.id {
            Id::Drawing(id) => {
                if data.selected_snippet == Some(id) {
                    DRAW_SNIPPET_SELECTED_COLOR
                } else {
                    DRAW_SNIPPET_COLOR
                }
            }
            Id::Audio(_) => AUDIO_SNIPPET_COLOR,
        }
    }
}

#[allow(unused_variables)]
impl Widget<ScribbleState> for TimelineSnippet {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut ScribbleState, _env: &Env) {
        match event {
            Event::MouseDown(ev) if ev.button.is_left() => {
                ctx.set_active(true);
                ctx.set_handled();
            }
            Event::MouseUp(ev) if ev.button.is_left() => {
                if ctx.is_active() {
                    ctx.set_active(false);
                    if ctx.is_hot() {
                        if let Id::Drawing(id) = self.id {
                            data.selected_snippet = Some(id);
                            ctx.request_paint();
                            ctx.set_handled();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn update(
        &mut self,
        ctx: &mut UpdateCtx,
        old_data: &ScribbleState,
        data: &ScribbleState,
        _env: &Env,
    ) {
        let snip = self.snip(data);
        let old_snip = self.snip(old_data);
        if !snip.same(&old_snip) {
            ctx.request_paint();
        }
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        _data: &ScribbleState,
        _env: &Env,
    ) {
        match event {
            LifeCycle::HotChanged(_) => {
                ctx.request_paint();
            }
            _ => {}
        }
    }

    fn layout(
        &mut self,
        _ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &ScribbleState,
        _env: &Env,
    ) -> Size {
        let width = self.width(data);
        let height = SNIPPET_HEIGHT;
        bc.constrain((width, height))
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &ScribbleState, env: &Env) {
        let snippet = self.snip(data);
        let width = self.width(data).min(10000.0); // FIXME: there are bugs drawing infinite rects.
        let height = SNIPPET_HEIGHT;
        let rect = Rect::from_origin_size(Point::ZERO, (width, height))
            .to_rounded_rect(env.get(theme::BUTTON_BORDER_RADIUS));
        let stroke_color = if ctx.is_hot() {
            &SNIPPET_STROKE_COLOR
        } else {
            &SNIPPET_HOVER_STROKE_COLOR
        };
        let fill_color = self.fill_color(data);

        ctx.fill(&rect, &fill_color);
        ctx.stroke(&rect, stroke_color, SNIPPET_STROKE_THICKNESS);

        // Draw the span of the edited region.
        if let Some(last_draw_time) = snippet.last_draw_time() {
            let draw_width = (last_draw_time - snippet.start_time()) as f64 * PIXELS_PER_USEC;
            let color = Color::rgb8(0, 0, 0);
            ctx.stroke(
                Line::new((0.0, height / 2.0), (draw_width, height / 2.0)),
                &color,
                1.0,
            );
            ctx.stroke(
                Line::new((draw_width, height * 0.25), (draw_width, height * 0.75)),
                &color,
                1.0,
            );
        }

        // Draw the lerp lines.
        for t in snippet.inner_lerp_times() {
            let x = (t as f64) * PIXELS_PER_USEC;
            ctx.stroke(Line::new((x, 0.0), (x, height)), &SNIPPET_STROKE_COLOR, 1.0);
        }
    }
}

impl Widget<ScribbleState> for Timeline {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut ScribbleState, env: &Env) {
        match event {
            Event::WindowConnected => {
                ctx.request_paint();
            }
            Event::MouseDown(ev) => {
                data.time_us = (ev.pos.x / PIXELS_PER_USEC) as i64;
                ctx.set_active(true);
                ctx.request_paint();
            }
            Event::MouseMoved(ev) => {
                // On click-and-drag, we change the time with the drag.
                if ctx.is_active() {
                    data.time_us = (ev.pos.x / PIXELS_PER_USEC) as i64;
                    ctx.request_paint();
                }
            }
            Event::MouseUp(_) => {
                if ctx.is_active() {
                    ctx.set_active(false);
                }
            }
            _ => {}
        }

        for child in self.children.values_mut() {
            child.event(ctx, event, data, env);
        }
    }

    fn update(
        &mut self,
        ctx: &mut UpdateCtx,
        old_data: &ScribbleState,
        data: &ScribbleState,
        env: &Env,
    ) {
        if !data.snippets.same(&old_data.snippets)
            || !data.audio_snippets.same(&old_data.audio_snippets)
        {
            ctx.request_layout();
            self.recalculate_snippet_offsets(&data.snippets, &data.audio_snippets);
            ctx.children_changed();
        }
        if old_data.time_us != data.time_us {
            ctx.request_paint();
        }
        for child in self.children.values_mut() {
            child.update(ctx, data, env);
        }
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &ScribbleState,
        env: &Env,
    ) {
        for child in self.children.values_mut() {
            child.lifecycle(ctx, event, data, env);
        }
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &ScribbleState,
        env: &Env,
    ) -> Size {
        for (&id, &offset) in &self.snippet_offsets {
            let child = self.children.get_mut(&id).unwrap();
            let x = (child.widget().snip(data).start_time() as f64) * PIXELS_PER_USEC;
            let y = offset as f64 * SNIPPET_HEIGHT;

            // FIXME: shouldn't we modify bc before recursing?
            let size = child.layout(ctx, bc, data, env);
            child.set_layout_rect(dbg!(Rect::from_origin_size((x, y), size)));
        }

        let height = SNIPPET_HEIGHT * self.num_rows as f64;
        bc.constrain((std::f64::INFINITY, height))
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &ScribbleState, env: &Env) {
        let size = ctx.size();
        let rect = Rect::from_origin_size(Point::ZERO, size);
        ctx.fill(rect, &TIMELINE_BG_COLOR);

        for child in self.children.values_mut() {
            child.paint_with_offset(ctx, data, env);
        }

        // Draw the cursor.
        let cursor_x = PIXELS_PER_USEC * (data.time_us as f64);
        let line = Line::new((cursor_x, 0.0), (cursor_x, size.height));
        ctx.stroke(line, &CURSOR_COLOR, CURSOR_THICKNESS);

        // Draw the mark.
        if let Some(mark_time) = data.mark {
            let mark_x = PIXELS_PER_USEC * (mark_time as f64);
            let mut path = BezPath::new();
            path.move_to((mark_x - 8.0, 0.0));
            path.line_to((mark_x + 8.0, 0.0));
            path.line_to((mark_x, 8.0));
            path.close_path();
            ctx.fill(path, &MARK_COLOR);
        }
    }
}
