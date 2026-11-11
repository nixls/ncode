//! Chat pane: virtualized, tail-following timeline plus the composer. It observes only the
//! active thread's `Timeline` entity, so streaming in one thread repaints just this pane.

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::icons::AppIcon;
use crate::model::*;
use crate::state::{AppEvent, AppState, Timeline};

pub struct ChatPane {
    app: Entity<AppState>,
    composer: Entity<TextareaState>,
    timeline: Option<Entity<Timeline>>,
    thread_id: Option<ThreadId>,
    drafts: HashMap<ThreadId, String>,
    _timeline_sub: Option<Subscription>,
}

impl ChatPane {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .submit_on_enter(true)
                .placeholder("Ask Claude anything… (Enter to send, Shift+Enter for newline)")
        });
        cx.subscribe_in(&composer, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit(window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&app, window, |this, _, event: &AppEvent, window, cx| match event {
            AppEvent::ActiveThreadChanged => this.sync_thread(window, cx),
        })
        .detach();
        cx.observe(&app, |_, _, cx| cx.notify()).detach();

        let mut this = Self {
            app,
            composer,
            timeline: None,
            thread_id: None,
            drafts: HashMap::new(),
            _timeline_sub: None,
        };
        this.sync_thread(window, cx);
        this
    }

    fn sync_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.app.read(cx).active_thread.clone();
        if next == self.thread_id {
            return;
        }
        // Keep each thread's unsent draft, like upstream's per-thread composer drafts.
        let draft = self.composer.read(cx).value().to_string();
        if let Some(prev) = self.thread_id.take() {
            self.drafts.insert(prev, draft);
        }
        let restored = next.as_ref().and_then(|id| self.drafts.remove(id)).unwrap_or_default();
        self.composer.update(cx, |c, cx| {
            c.set_value(restored, window, cx);
            c.focus(window, cx);
        });
        self.timeline = next
            .as_ref()
            .map(|id| self.app.update(cx, |app, cx| app.timeline(id, cx)));
        self._timeline_sub = self.timeline.as_ref().map(|t| cx.observe(t, |_, _, cx| cx.notify()));
        self.thread_id = next;
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else { return };
        if self.timeline.as_ref().is_some_and(|t| t.read(cx).running) {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.composer.update(cx, |c, cx| c.set_value("", window, cx));
        self.app.update(cx, |app, cx| app.send_message(&thread_id, text, cx));
    }

    fn render_row(
        timeline: &Entity<Timeline>,
        app: &Entity<AppState>,
        index: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let t = timeline.read(cx);
        let Some(row) = t.rows.get(index) else { return div().into_any_element() };
        let theme = cx.theme();
        let key = row.key.clone();
        let body = match &row.item {
            Item::User { text } => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.8))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme.secondary)
                        .text_color(theme.secondary_foreground)
                        .child(TextView::markdown(SharedString::from(format!("u-{key}")), text.clone()).selectable(true)),
                )
                .into_any_element(),
            Item::Assistant { text } => div()
                .child(
                    TextView::markdown(SharedString::from(format!("a-{key}")), text.clone())
                        .selectable(true)
                        .stream_fade(t.running),
                )
                .into_any_element(),
            Item::Thinking { text } => {
                let expanded = t.expanded.contains(&key);
                let timeline = timeline.clone();
                v_flex()
                    .id(SharedString::from(format!("th-{key}")))
                    .cursor_pointer()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .on_click(move |_, _, cx| timeline.update(cx, |t, cx| t.toggle_expanded(&key, cx)))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Icon::new(AppIcon::Brain).xsmall())
                            .child(if expanded { "Thinking".to_owned() } else { truncate_line(text, 120) }),
                    )
                    .when(expanded, |el| el.child(div().pl_5().italic().child(text.clone())))
                    .into_any_element()
            }
            Item::Tool { name, summary, output, status, .. } => {
                let expanded = t.expanded.contains(&key);
                let (dot, color) = match status {
                    ToolStatus::Running => ("●", theme.muted_foreground),
                    ToolStatus::Done => ("✓", theme.green),
                    ToolStatus::Failed => ("✕", theme.red),
                };
                let timeline = timeline.clone();
                let output = output.clone();
                v_flex()
                    .id(SharedString::from(format!("tool-{key}")))
                    .text_sm()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| timeline.update(cx, |t, cx| t.toggle_expanded(&key, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(div().w(px(12.)).text_color(color).child(dot))
                            .child(Icon::new(AppIcon::Wrench).xsmall().text_color(theme.muted_foreground))
                            .child(div().font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .child(div().flex_1().min_w_0().truncate().font_family("monospace").text_xs().text_color(theme.muted_foreground).child(summary.clone())),
                    )
                    .when(expanded, |el| {
                        let text = output.unwrap_or_else(|| "(no output yet)".into());
                        let clipped: String = text.lines().take(60).collect::<Vec<_>>().join("\n");
                        el.child(
                            div()
                                .ml_5()
                                .mt_1()
                                .p_2()
                                .rounded_md()
                                .bg(theme.muted)
                                .font_family("monospace")
                                .text_xs()
                                .child(clipped),
                        )
                    })
                    .into_any_element()
            }
            Item::Approval { request_id, tool, summary, state } => {
                let pending = *state == ApprovalState::Pending;
                let thread_id = t.thread_id.clone();
                let mut card = v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(if pending { theme.warning } else { theme.border })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(AppIcon::ShieldCheck).small().text_color(theme.warning))
                            .child(div().font_weight(FontWeight::MEDIUM).child(format!("Claude wants to use {tool}"))),
                    )
                    .child(div().font_family("monospace").text_xs().text_color(theme.muted_foreground).child(summary.clone()));
                card = if pending {
                    let (a1, a2) = (app.clone(), app.clone());
                    let (t1, t2) = (thread_id.clone(), thread_id);
                    let (r1, r2) = (request_id.clone(), request_id.clone());
                    card.child(
                        h_flex()
                            .gap_2()
                            .child(Button::new(SharedString::from(format!("allow-{key}"))).primary().small().label("Allow").on_click(
                                move |_, _, cx| a1.update(cx, |a, cx| a.respond_approval(&t1, &r1, true, cx)),
                            ))
                            .child(Button::new(SharedString::from(format!("deny-{key}"))).outline().small().label("Deny").on_click(
                                move |_, _, cx| a2.update(cx, |a, cx| a.respond_approval(&t2, &r2, false, cx)),
                            )),
                    )
                } else {
                    card.child(div().text_xs().text_color(theme.muted_foreground).child(
                        if *state == ApprovalState::Allowed { "Allowed" } else { "Denied" },
                    ))
                };
                card.into_any_element()
            }
            Item::TurnEnd { duration_ms, cost_usd, is_error } => h_flex()
                .gap_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(div().flex_1().h(px(1.)).bg(theme.border))
                .child(format!(
                    "{} {:.1}s{}",
                    if *is_error { "Stopped after" } else { "Worked for" },
                    *duration_ms as f64 / 1000.0,
                    if *cost_usd > 0.0 { format!(" · ${cost_usd:.3} session total") } else { String::new() }
                ))
                .child(div().flex_1().h(px(1.)).bg(theme.border))
                .into_any_element(),
            Item::Notice { text, error } => div()
                .text_sm()
                .text_color(if *error { theme.danger } else { theme.muted_foreground })
                .child(text.clone())
                .into_any_element(),
        };
        div().w_full().max_w(px(820.)).mx_auto().px_6().py_1p5().child(body).into_any_element()
    }

    fn picker(
        id: &'static str,
        label: String,
        options: Vec<(String, String)>,
        on_pick: impl Fn(&str, &mut App) + 'static,
    ) -> impl IntoElement {
        let on_pick = std::rc::Rc::new(on_pick);
        Popover::new(id)
            .anchor(Anchor::BottomLeft)
            .trigger(Button::new(SharedString::from(format!("{id}-trigger"))).ghost().xsmall().label(label).dropdown_caret(true))
            .content(move |_, _, _| {
                let mut list = v_flex().gap_0p5().min_w(px(180.));
                for (value, text) in options.iter().cloned() {
                    let on_pick = on_pick.clone();
                    list = list.child(
                        Button::new(SharedString::from(format!("{id}-{value}")))
                            .ghost()
                            .small()
                            .w_full()
                            .justify_start()
                            .label(text)
                            .on_click(move |_, window, cx| {
                                on_pick(&value, cx);
                                window.refresh();
                            }),
                    );
                }
                list
            })
    }

    fn render_composer(&self, thread: &Thread, running: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tid = thread.id.clone();
        let app = self.app.clone();
        let model_picker = {
            let app = app.clone();
            let tid = tid.clone();
            Self::picker(
                "model",
                model_label(&thread.model).to_owned(),
                MODELS.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect(),
                move |v, cx| app.update(cx, |a, cx| a.set_thread_model(&tid, v, cx)),
            )
        };
        let mode_picker = {
            let app = app.clone();
            let tid = tid.clone();
            Self::picker(
                "mode",
                thread.mode.label().to_owned(),
                RuntimeMode::ALL.iter().map(|m| (m.as_str().to_owned(), m.label().to_owned())).collect(),
                move |v, cx| app.update(cx, |a, cx| a.set_thread_mode(&tid, RuntimeMode::parse(v), cx)),
            )
        };
        let action = if running {
            Button::new("stop").small().outline().icon(Icon::new(AppIcon::CircleStop)).label("Stop").on_click(
                cx.listener(move |this, _, _, cx| {
                    this.app.update(cx, |a, _| a.interrupt(&tid));
                }),
            )
        } else {
            Button::new("send")
                .small()
                .primary()
                .icon(Icon::new(IconName::ArrowUp))
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };
        div().w_full().max_w(px(820.)).mx_auto().px_6().pb_4().child(
            v_flex()
                .rounded_xl()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .shadow_sm()
                .child(div().px_1().pt_1().child(Textarea::new(&self.composer).appearance(false)))
                .child(
                    h_flex()
                        .px_2()
                        .pb_2()
                        .gap_1()
                        .child(model_picker)
                        .child(mode_picker)
                        .child(div().flex_1())
                        .when(running, |el| el.child(Spinner::new().small()))
                        .child(action),
                ),
        )
    }
}

impl Render for ChatPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let thread = self.app.read(cx).active_thread().cloned();
        let (Some(thread), Some(timeline)) = (thread, self.timeline.clone()) else {
            return self.render_empty(cx).into_any_element();
        };
        let running = timeline.read(cx).running;
        let empty = timeline.read(cx).rows.is_empty();
        let scroller_state = timeline.read(cx).scroller.clone();
        let app = self.app.clone();
        let row_timeline = timeline.clone();

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(if empty {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(AppIcon::MessageSquare).large())
                    .child(div().text_lg().child("Start a conversation"))
                    .child(div().text_sm().child(format!("Claude Code will run in {}", self.project_root(&thread, cx))))
                    .into_any_element()
            } else {
                MessageScroller::new("timeline", scroller_state, move |index, window, cx| {
                    Self::render_row(&row_timeline, &app, index, window, cx)
                })
                .with_bottom_fade(theme.background)
                .flex_1()
                .into_any_element()
            })
            .child(self.render_composer(&thread, running, cx))
            .into_any_element()
    }
}

impl ChatPane {
    fn project_root(&self, thread: &Thread, cx: &App) -> String {
        self.app
            .read(cx)
            .project(&thread.project_id)
            .map(|p| p.root.clone())
            .unwrap_or_default()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let app = self.app.read(cx);
        let has_projects = !app.projects.is_empty();
        let project = app.current_project().map(|p| (p.id.clone(), p.name.clone()));
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme.background)
            .text_color(theme.muted_foreground)
            .child(div().text_xl().text_color(theme.foreground).child("ncode"))
            .child(div().text_sm().child(if has_projects {
                "Pick a thread or start a new one."
            } else {
                "Add a project folder from the sidebar to get started."
            }))
            .when_some(project, |el, (pid, name)| {
                el.child(
                    Button::new("empty-new-thread")
                        .primary()
                        .icon(Icon::new(AppIcon::SquarePen))
                        .label(format!("New thread in {name}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.app.update(cx, |a, cx| {
                                a.new_thread(&pid, cx);
                            });
                        })),
                )
            })
    }
}
