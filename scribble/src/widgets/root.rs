use druid::widget::{Align, Flex};
use druid::{
    BoxConstraints, Color, Command, Env, Event, EventCtx, KeyCode, KeyEvent, LayoutCtx, LifeCycle,
    LifeCycleCtx, PaintCtx, Size, TimerToken, UpdateCtx, Widget, WidgetExt, WidgetId,
};
use std::sync::mpsc::{channel, Receiver};

use scribble_curves::{SnippetData, SnippetId, Time};

use crate::audio::{AudioSnippetData, AudioSnippetId};
use crate::cmd;
use crate::data::{AppState, CurrentAction, MaybeSnippetId, RecordingSpeed, SegmentInProgress};
use crate::encode::EncodingStatus;
use crate::widgets::{
    icons, make_status_bar, make_timeline, DrawingPane, LabelledContainer, Palette, ToggleButton,
};
use crate::FRAME_TIME;

pub struct Root {
    timer_id: TimerToken,

    // While we're encoding a file, this receives status updates from the encoder. Each update
    // is a number between 0.0 and 1.0 (where 1.0 means finished).
    encoder_progress: Option<Receiver<EncodingStatus>>,

    inner: Box<dyn Widget<AppState>>,
}

fn make_draw_button_group() -> impl Widget<AppState> {
    let rec_button: ToggleButton<AppState> = ToggleButton::new(
        &icons::VIDEO,
        20.0,
        |state: &AppState| state.action.rec_toggle(),
        |ctx, _, _| ctx.submit_command(cmd::DRAW, None),
        |ctx, _, _| ctx.submit_command(cmd::STOP, None),
    );
    let rec_speed_group = crate::widgets::radio_icon::make_radio_icon_group(
        20.0,
        vec![
            (&icons::PAUSE, RecordingSpeed::Paused),
            (&icons::SNAIL, RecordingSpeed::Slower),
            (&icons::TURTLE, RecordingSpeed::Slow),
            (&icons::RABBIT, RecordingSpeed::Normal),
        ],
    );
    let rec_fade_button = ToggleButton::new(
        &icons::FADE_OUT,
        20.0,
        |&b: &bool| b.into(),
        |_, data, _| *data = true,
        |_, data, _| *data = false,
    )
    .lens(AppState::fade_enabled);

    let draw_button_group = Flex::row()
        .with_child(rec_button)
        .with_spacer(10.0)
        .with_child(rec_speed_group.lens(AppState::recording_speed))
        .with_spacer(10.0)
        .with_child(rec_fade_button)
        .padding(5.0);
    let draw_button_group = LabelledContainer::new(draw_button_group, "Draw")
        .border_color(Color::WHITE)
        .corner_radius(druid::theme::BUTTON_BORDER_RADIUS)
        .padding(5.0);

    draw_button_group
}

impl Root {
    pub fn new() -> Root {
        let drawing = DrawingPane::default();
        let rec_audio_button: ToggleButton<AppState> = ToggleButton::new(
            &icons::MICROPHONE,
            20.0,
            |state: &AppState| state.action.rec_audio_toggle(),
            |ctx, _, _| ctx.submit_command(cmd::TALK, None),
            |ctx, _, _| ctx.submit_command(cmd::STOP, None),
        );
        let play_button = ToggleButton::new(
            &icons::PLAY,
            20.0,
            |state: &AppState| state.action.play_toggle(),
            |ctx, _, _| ctx.submit_command(cmd::PLAY, None),
            |ctx, _, _| ctx.submit_command(cmd::STOP, None),
        );

        let palette = Palette::default();
        let draw_button_group = make_draw_button_group();

        let audio_button_group = Flex::row().with_child(rec_audio_button).padding(5.0);
        let audio_button_group = LabelledContainer::new(audio_button_group, "Talk")
            .border_color(Color::WHITE)
            .corner_radius(druid::theme::BUTTON_BORDER_RADIUS)
            .padding(5.0);

        let watch_button_group = Flex::row().with_child(play_button).padding(5.0);
        let watch_button_group = LabelledContainer::new(watch_button_group, "Watch")
            .border_color(Color::WHITE)
            .corner_radius(druid::theme::BUTTON_BORDER_RADIUS)
            .padding(5.0);

        let button_row = Flex::row()
            .with_child(draw_button_group)
            .with_child(audio_button_group)
            .with_child(watch_button_group)
            .with_flex_spacer(1.0)
            .with_child(palette.lens(AppState::palette));
        let timeline_id = WidgetId::next();
        let timeline = make_timeline().with_id(timeline_id);
        /*
        TODO: Issues with split:
         - can't get timeline to use up the vertical space it has available
         - can't set a reasonable default initial size
        let drawing_and_timeline = Split::horizontal(drawing.padding(10.0), timeline)
            .draggable(true).debug_paint_layout();
        */
        let column = Flex::column()
            .with_child(button_row)
            .with_flex_child(drawing.padding(10.0), 1.0)
            .with_child(timeline)
            .with_child(make_status_bar());

        Root {
            inner: Box::new(Align::centered(column)),
            encoder_progress: None,
            timer_id: TimerToken::INVALID,
        }
    }
}

impl Root {
    fn handle_key_down(
        &mut self,
        ctx: &mut EventCtx,
        ev: &KeyEvent,
        data: &mut AppState,
        _env: &Env,
    ) {
        // If they push another key while holding down the arrow, cancel the scanning.
        if let CurrentAction::Scanning(speed) = data.action {
            let direction = if speed > 0.0 {
                KeyCode::ArrowRight
            } else {
                KeyCode::ArrowLeft
            };
            if ev.key_code != direction {
                data.stop_scanning();
            }
            ctx.set_handled();
            if ev.key_code == KeyCode::ArrowRight || ev.key_code == KeyCode::ArrowLeft {
                return;
            }
        }

        match ev.key_code {
            KeyCode::ArrowRight | KeyCode::ArrowLeft => {
                let speed = if ev.mods.shift { 2.0 } else { 1.0 };
                let dir = if ev.key_code == KeyCode::ArrowRight {
                    1.0
                } else {
                    -1.0
                };
                let velocity = speed * dir;
                if data.action.is_idle() || data.action.is_scanning() {
                    data.scan(velocity);
                }
                ctx.set_handled();
            }
            _ => {}
        }
    }

    fn handle_key_up(
        &mut self,
        ctx: &mut EventCtx,
        ev: &KeyEvent,
        data: &mut AppState,
        _env: &Env,
    ) {
        match ev.key_code {
            KeyCode::ArrowRight | KeyCode::ArrowLeft => {
                if data.action.is_scanning() {
                    data.stop_scanning();
                }
                ctx.set_handled();
            }
            _ => {}
        }
    }

    fn handle_command(
        &mut self,
        ctx: &mut EventCtx,
        cmd: &Command,
        data: &mut AppState,
        _env: &Env,
    ) -> bool {
        let ret = match cmd.selector {
            cmd::ADD_SNIPPET => {
                let snip = cmd.get_object::<SnippetData>().expect("no snippet");
                let (new_snippets, new_id) = data.scribble.snippets.with_new_snippet(snip.clone());
                data.scribble.snippets = new_snippets;
                data.scribble.selected_snippet = new_id.into();
                data.undo.borrow_mut().push(&data.scribble);
                true
            }
            cmd::DELETE_SNIPPET => {
                if let Some(id) = cmd
                    .get_object::<SnippetId>()
                    .ok()
                    .cloned()
                    .or(data.scribble.selected_snippet.as_draw())
                {
                    let new_snippets = data.scribble.snippets.without_snippet(id);
                    data.scribble.snippets = new_snippets;
                    if data.scribble.selected_snippet == id.into() {
                        data.scribble.selected_snippet = MaybeSnippetId::None;
                    }
                    data.undo.borrow_mut().push(&data.scribble);
                } else if let Some(id) = cmd
                    .get_object::<AudioSnippetId>()
                    .ok()
                    .cloned()
                    .or(data.scribble.selected_snippet.as_audio())
                {
                    let new_snippets = data.scribble.audio_snippets.without_snippet(id);
                    data.scribble.audio_snippets = new_snippets;
                    if data.scribble.selected_snippet == id.into() {
                        data.scribble.selected_snippet = MaybeSnippetId::None;
                    }
                    data.undo.borrow_mut().push(&data.scribble);
                } else {
                    log::error!("No snippet id to delete");
                }
                true
            }
            cmd::ADD_AUDIO_SNIPPET => {
                let snip = cmd
                    .get_object::<AudioSnippetData>()
                    .expect("no audio snippet");
                data.scribble.audio_snippets =
                    data.scribble.audio_snippets.with_new_snippet(snip.clone());
                data.undo.borrow_mut().push(&data.scribble);
                true
            }
            cmd::APPEND_NEW_SEGMENT => {
                let seg = cmd.get_object::<SegmentInProgress>().expect("no segment");
                data.add_segment_to_snippet(seg.clone());
                data.undo.borrow_mut().push_transient(&data.scribble);
                true
            }
            cmd::CHOOSE_COLOR => {
                let color = cmd.get_object::<Color>().expect("API violation");
                data.palette.select(color);
                true
            }
            cmd::EXPORT => {
                let export = cmd.get_object::<cmd::ExportCmd>().expect("API violation");

                if self.encoder_progress.is_some() {
                    log::warn!("already encoding, not doing another one");
                } else {
                    let (tx, rx) = channel();
                    let export = export.clone();
                    // Encoder progress will be read whenever the timer ticks, and when encoding
                    // is done this will be set back to `None`.
                    self.encoder_progress = Some(rx);
                    data.encoding_status = None;
                    std::thread::spawn(move || crate::encode::encode_blocking(export, tx));
                }

                true
            }
            cmd::SET_MARK => {
                let time = *cmd.get_object::<Time>().unwrap_or(&data.time());
                data.scribble.mark = Some(time);
                data.undo.borrow_mut().push(&data.scribble);
                true
            }
            cmd::TRUNCATE_SNIPPET => {
                if let Some(id) = data.scribble.selected_snippet.as_draw() {
                    data.scribble.snippets = data
                        .scribble
                        .snippets
                        .with_truncated_snippet(id, data.time());
                    data.undo.borrow_mut().push(&data.scribble);
                } else {
                    log::error!("cannot truncate, nothing selected");
                }
                true
            }
            cmd::LERP_SNIPPET => {
                if let (Some(mark_time), Some(id)) =
                    (data.scribble.mark, data.scribble.selected_snippet.as_draw())
                {
                    data.scribble.snippets =
                        data.scribble
                            .snippets
                            .with_new_lerp(id, data.time(), mark_time);
                    data.undo.borrow_mut().push(&data.scribble);
                    ctx.submit_command(Command::new(cmd::WARP_TIME_TO, mark_time), None);
                } else {
                    log::error!(
                        "cannot lerp, mark time {:?}, selected {:?}",
                        data.scribble.mark,
                        data.scribble.selected_snippet
                    );
                }
                true
            }
            druid::commands::UNDO => {
                let undone_state = data.undo.borrow_mut().undo();
                if let Some(undone_state) = undone_state {
                    data.scribble = undone_state;
                    ctx.request_paint();

                    // This is a bit of a special-case hack. If there get to be
                    // more of these, it might be worth storing some
                    // metadata in the undo state.
                    //
                    // In case the undo resets us to a mid-recording state, we
                    // ensure that the state is waiting-to-record (i.e.,
                    // recording but paused).
                    if let Some(ref new_curve) = data.scribble.new_curve {
                        let time = *new_curve.times.last().unwrap();
                        data.warp_time_to(time);
                        data.ensure_recording();
                    }
                }
                true
            }
            druid::commands::REDO => {
                if let Some(redone_state) = data.undo.borrow_mut().redo() {
                    data.scribble = redone_state;
                    ctx.request_paint();
                }
                true
            }
            cmd::PLAY => {
                if data.action.is_idle() {
                    data.start_playing();
                } else {
                    log::error!("can't play, current action is {:?}", data.action);
                }
                true
            }
            cmd::DRAW => {
                if data.action.is_idle() {
                    data.start_recording(data.recording_speed.factor());
                } else {
                    log::error!("can't draw, current action is {:?}", data.action);
                }
                true
            }
            cmd::TALK => {
                if data.action.is_idle() {
                    data.start_recording_audio();
                } else {
                    log::error!("can't talk, current action is {:?}", data.action);
                }
                true
            }
            cmd::STOP => {
                match data.action {
                    CurrentAction::Idle => {}
                    CurrentAction::Scanning(_) => {}
                    CurrentAction::Playing => data.stop_playing(),
                    CurrentAction::WaitingToRecord(_) | CurrentAction::Recording(_) => {
                        if let Some(new_snippet) = data.stop_recording() {
                            ctx.submit_command(Command::new(cmd::ADD_SNIPPET, new_snippet), None);
                        }
                    }
                    CurrentAction::RecordingAudio(_) => {
                        let snip = data.stop_recording_audio();
                        ctx.submit_command(Command::new(cmd::ADD_AUDIO_SNIPPET, snip), None);
                    }
                }
                true
            }
            cmd::WARP_TIME_TO => {
                if data.action.is_idle() {
                    data.warp_time_to(*cmd.get_object::<Time>().expect("API violation"));
                } else {
                    log::warn!("not warping: state is {:?}", data.action)
                }
                true
            }
            _ => false,
        };
        // This might be a little conservative, but there are lots of state
        // changes that cause the menus to change, so the easiest thing is just
        // to rebuild the menus on every command.
        ctx.submit_command(cmd::REBUILD_MENUS, None);
        ret
    }
}

impl Widget<AppState> for Root {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut AppState, env: &Env) {
        match event {
            Event::WindowConnected => {
                ctx.request_focus();
                ctx.request_paint();
                self.timer_id = ctx.request_timer(FRAME_TIME);
            }
            Event::Command(cmd) => {
                let handled = self.handle_command(ctx, cmd, data, env);
                if handled {
                    ctx.set_handled();
                }
            }
            Event::KeyDown(ev) => self.handle_key_down(ctx, ev, data, env),
            Event::KeyUp(ev) => self.handle_key_up(ctx, ev, data, env),
            Event::Timer(tok) => {
                if tok == &self.timer_id {
                    // Handle any status reports from the encoder.
                    if let Some(ref rx) = self.encoder_progress {
                        if let Some(status) = rx.try_iter().last() {
                            data.encoding_status = Some(status);
                        }
                        match data.encoding_status {
                            Some(EncodingStatus::Finished) | Some(EncodingStatus::Error(_)) => {
                                self.encoder_progress = None;
                            }
                            _ => {}
                        }
                    }

                    // TODO: we should handing ticking using animation instead of timers?
                    // The issue with that is that `lifecycle` doesn't get to mutate the data.

                    // Update the current time, if necessary.
                    data.update_time();
                    self.timer_id = ctx.request_timer(FRAME_TIME);
                    ctx.set_handled();
                }
            }
            _ => {
                self.inner.event(ctx, event, data, env);
            }
        }
    }

    fn update(&mut self, ctx: &mut UpdateCtx, old_data: &AppState, data: &AppState, env: &Env) {
        self.inner.update(ctx, old_data, data, env);
    }

    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &AppState, env: &Env) {
        self.inner.lifecycle(ctx, event, data, env);
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &AppState,
        env: &Env,
    ) -> Size {
        self.inner.layout(ctx, bc, data, env)
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &AppState, env: &Env) {
        self.inner.paint(ctx, data, env);
    }
}
