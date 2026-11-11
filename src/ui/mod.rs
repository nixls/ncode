pub mod branch;
pub mod chat;
pub mod sidebar;

use gpui_kit::component::{ActiveTheme as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::state::AppState;

/// Window root: title bar (breadcrumb + branch applet), sidebar, chat.
pub struct Workspace {
    app: Entity<AppState>,
    sidebar: Entity<sidebar::Sidebar>,
    chat: Entity<chat::ChatPane>,
    branch: Entity<branch::BranchApplet>,
}

impl Workspace {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self {
            sidebar: cx.new(|cx| sidebar::Sidebar::new(app.clone(), window, cx)),
            chat: cx.new(|cx| chat::ChatPane::new(app.clone(), window, cx)),
            branch: cx.new(|cx| branch::BranchApplet::new(app.clone(), window, cx)),
            app,
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let app = self.app.read(cx);
        let project = app.current_project().map(|p| p.name.clone());
        let thread = app.active_thread().map(|t| t.title.clone());

        let breadcrumb = h_flex()
            .gap_2()
            .min_w_0()
            .text_sm()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("ncode"))
            .when_some(project, |el, p| {
                el.child(div().text_color(theme.muted_foreground).child("/"))
                    .child(div().text_color(theme.muted_foreground).child(p))
            })
            .when_some(thread, |el, t| {
                el.child(div().text_color(theme.muted_foreground).child("/"))
                    .child(div().min_w_0().truncate().child(t))
            });

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .pr_2()
                        .justify_between()
                        .child(breadcrumb)
                        .child(self.branch.clone()),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(div().w(px(280.)).h_full().flex_shrink_0().child(self.sidebar.clone()))
                    .child(div().flex_1().h_full().min_w_0().child(self.chat.clone())),
            )
    }
}
