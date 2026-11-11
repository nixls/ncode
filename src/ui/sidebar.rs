//! Project/thread sidebar: projects in user order, threads newest first under each,
//! inline rename, reorder, collapse, and two-step delete.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::icons::AppIcon;
use crate::state::{AppState, relative_time};

#[derive(Clone, PartialEq)]
enum Target {
    Project(String),
    Thread(String),
}

pub struct Sidebar {
    app: Entity<AppState>,
    rename_input: Entity<InputState>,
    renaming: Option<Target>,
    confirm_delete: Option<Target>,
}

impl Sidebar {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(&rename_input, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(cx),
            _ => {}
        })
        .detach();
        Self { app, rename_input, renaming: None, confirm_delete: None }
    }

    fn start_rename(&mut self, target: Target, current: String, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(target);
        self.rename_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.renaming.take() else { return };
        let value = self.rename_input.read(cx).value().to_string();
        self.app.update(cx, |app, cx| match target {
            Target::Project(id) => app.rename_project(&id, value, cx),
            Target::Thread(id) => app.rename_thread(&id, value, cx),
        });
        cx.notify();
    }

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        let app = self.app.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = app.update(cx, |app, cx| app.add_project(path, cx));
            }
        })
        .detach();
    }

    /// First click arms the delete, second click within the same target performs it.
    fn delete(&mut self, target: Target, cx: &mut Context<Self>) {
        if self.confirm_delete.as_ref() != Some(&target) {
            self.confirm_delete = Some(target);
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        self.app.update(cx, |app, cx| match target {
            Target::Project(id) => app.remove_project(&id, cx),
            Target::Thread(id) => app.delete_thread(&id, cx),
        });
    }

    fn icon_button(id: impl Into<ElementId>, icon: impl Into<Icon>, tooltip: &'static str) -> Button {
        Button::new(id).ghost().xsmall().icon(icon).tooltip(tooltip)
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let app = self.app.read(cx);
        let active_thread = app.active_thread.clone();
        let active_project = app.current_project().map(|p| p.id.clone());

        let mut list = v_flex().gap_0p5().px_2().pb_4();
        let project_count = app.projects.len();
        for (index, project) in app.projects.iter().enumerate() {
            let pid = project.id.clone();
            let group: SharedString = format!("proj-{pid}").into();
            let is_renaming = self.renaming == Some(Target::Project(pid.clone()));
            let confirm = self.confirm_delete == Some(Target::Project(pid.clone()));
            let is_current = active_project.as_deref() == Some(pid.as_str());

            let header = h_flex()
                .id(SharedString::from(format!("project-{pid}")))
                .group(group.clone())
                .h(px(28.))
                .px_1()
                .gap_1()
                .rounded_md()
                .hover(|s| s.bg(theme.sidebar_accent))
                .text_sm()
                .when(is_current, |el| el.text_color(theme.sidebar_foreground))
                .child(
                    Icon::new(if project.collapsed { IconName::ChevronRight } else { IconName::ChevronDown })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Icon::new(if project.collapsed { IconName::Folder } else { IconName::FolderOpen })
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(if is_renaming {
                    div().flex_1().child(Input::new(&self.rename_input).xsmall()).into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(project.name.clone())
                        .into_any_element()
                })
                .child(
                    h_flex()
                        .gap_0p5()
                        .when(!confirm, |el| el.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                        .child(Self::icon_button(format!("up-{pid}"), IconName::ChevronUp, "Move up")
                            .disabled(index == 0)
                            .on_click(cx.listener({ let pid = pid.clone(); move |this, _, _, cx| {
                                this.app.update(cx, |a, cx| a.move_project(&pid, -1, cx));
                            }})))
                        .child(Self::icon_button(format!("down-{pid}"), IconName::ChevronDown, "Move down")
                            .disabled(index + 1 == project_count)
                            .on_click(cx.listener({ let pid = pid.clone(); move |this, _, _, cx| {
                                this.app.update(cx, |a, cx| a.move_project(&pid, 1, cx));
                            }})))
                        .child(Self::icon_button(format!("ren-{pid}"), AppIcon::Pencil, "Rename project")
                            .on_click(cx.listener({ let pid = pid.clone(); let name = project.name.clone(); move |this, _, window, cx| {
                                this.start_rename(Target::Project(pid.clone()), name.clone(), window, cx);
                            }})))
                        .child(if confirm {
                            Button::new(SharedString::from(format!("del-{pid}")))
                                .danger()
                                .xsmall()
                                .label("Remove?")
                        } else {
                            Self::icon_button(format!("del-{pid}"), AppIcon::Trash, "Remove project")
                        }
                        .on_click(cx.listener({ let pid = pid.clone(); move |this, _, _, cx| {
                            this.delete(Target::Project(pid.clone()), cx);
                        }})))
                        .child(Self::icon_button(format!("new-{pid}"), AppIcon::SquarePen, "New thread")
                            .on_click(cx.listener({ let pid = pid.clone(); move |this, _, _, cx| {
                                this.app.update(cx, |a, cx| { a.new_thread(&pid, cx); });
                            }}))),
                )
                .on_click(cx.listener({
                    let pid = pid.clone();
                    move |this, _, _, cx| {
                        this.confirm_delete = None;
                        this.app.update(cx, |a, cx| {
                            a.toggle_project_collapsed(&pid, cx);
                            a.select_project(&pid, cx);
                        });
                    }
                }));
            list = list.child(header);

            if project.collapsed {
                continue;
            }
            let mut any = false;
            for thread in app.threads_for(&pid) {
                any = true;
                let tid = thread.id.clone();
                let group: SharedString = format!("thread-{tid}").into();
                let selected = active_thread.as_deref() == Some(tid.as_str());
                let running = app.running.contains(&tid);
                let is_renaming = self.renaming == Some(Target::Thread(tid.clone()));
                let confirm = self.confirm_delete == Some(Target::Thread(tid.clone()));
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("t-{tid}")))
                        .group(group.clone())
                        .h(px(28.))
                        .pl(px(26.))
                        .pr_1()
                        .gap_1p5()
                        .rounded_md()
                        .text_sm()
                        .text_color(if selected { theme.sidebar_accent_foreground } else { theme.muted_foreground })
                        .when(selected, |el| el.bg(theme.sidebar_accent))
                        .hover(|s| s.bg(theme.sidebar_accent))
                        .child(if is_renaming {
                            div().flex_1().child(Input::new(&self.rename_input).xsmall()).into_any_element()
                        } else {
                            div().flex_1().min_w_0().truncate().child(thread.title.clone()).into_any_element()
                        })
                        .child(
                            h_flex()
                                .gap_0p5()
                                .when(!confirm, |el| el.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                                .child(Self::icon_button(format!("tren-{tid}"), AppIcon::Pencil, "Rename thread")
                                    .on_click(cx.listener({ let tid = tid.clone(); let title = thread.title.clone(); move |this, _, window, cx| {
                                        this.start_rename(Target::Thread(tid.clone()), title.clone(), window, cx);
                                    }})))
                                .child(if confirm {
                                    Button::new(SharedString::from(format!("tdel-{tid}"))).danger().xsmall().label("Delete?")
                                } else {
                                    Self::icon_button(format!("tdel-{tid}"), AppIcon::Trash, "Delete thread")
                                }
                                .on_click(cx.listener({ let tid = tid.clone(); move |this, _, _, cx| {
                                    this.delete(Target::Thread(tid.clone()), cx);
                                }}))),
                        )
                        .child(if running {
                            Spinner::new().xsmall().into_any_element()
                        } else {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(relative_time(thread.updated_at))
                                .into_any_element()
                        })
                        .on_click(cx.listener({
                            let tid = tid.clone();
                            move |this, _, _, cx| {
                                this.confirm_delete = None;
                                this.app.update(cx, |a, cx| a.select_thread(Some(tid.clone()), cx));
                            }
                        })),
                );
            }
            if !any {
                list = list.child(
                    div().pl(px(26.)).h(px(24.)).text_xs().text_color(theme.muted_foreground).child("No threads yet"),
                );
            }
        }

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("PROJECTS")
                    .child(
                        Self::icon_button("add-project", AppIcon::FolderPlus, "Add project")
                            .on_click(cx.listener(|this, _, _, cx| this.add_project(cx))),
                    ),
            )
            .child(div().id("sidebar-scroll").flex_1().overflow_y_scroll().child(list))
    }
}
