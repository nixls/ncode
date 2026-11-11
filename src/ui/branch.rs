//! Header applet showing the current project's branch, dirty stats, ahead/behind and PR, with
//! a popover to switch or create branches. Polls git off the UI thread and only re-renders
//! when the status actually changes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::git::{self, GitStatus, PullRequest};
use crate::icons::AppIcon;
use crate::state::{AppState, GIT_POLL};

const PR_POLL: Duration = Duration::from_secs(60);

pub struct BranchApplet {
    app: Entity<AppState>,
    root: Option<PathBuf>,
    status: GitStatus,
    pr: Option<PullRequest>,
    branches: Vec<String>,
    error: Option<String>,
    new_branch: Entity<InputState>,
    open: bool,
    last_pr_check: Option<Instant>,
    _poll: Task<()>,
}

impl BranchApplet {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_branch = cx.new(|cx| InputState::new(window, cx).placeholder("Create branch…"));
        cx.subscribe_in(&new_branch, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let name = input.read(cx).value().trim().to_owned();
                if !name.is_empty() {
                    input.update(cx, |i, cx| i.set_value("", window, cx));
                    this.run_git(move |root| git::create_branch(root, &name), cx);
                }
            }
        })
        .detach();
        cx.observe(&app, |this, _, cx| this.sync_root(cx)).detach();

        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(root) = this.update(cx, |this, _| this.root.clone()) else { break };
                if let Some(root) = root {
                    let check_pr = this
                        .update(cx, |this, _| {
                            this.last_pr_check.is_none_or(|t| t.elapsed() >= PR_POLL)
                        })
                        .unwrap_or(false);
                    let r = root.clone();
                    let (status, pr) = cx
                        .background_spawn(async move {
                            let status = git::status(&r);
                            let pr = (check_pr && status.branch.is_some())
                                .then(|| git::pull_request(&r));
                            (status, pr)
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        if this.root.as_ref() != Some(&root) {
                            return;
                        }
                        if let Some(pr) = pr {
                            this.last_pr_check = Some(Instant::now());
                            if this.pr != pr {
                                this.pr = pr;
                                cx.notify();
                            }
                        }
                        if this.status != status {
                            this.status = status;
                            cx.notify();
                        }
                    });
                }
                cx.background_executor().timer(GIT_POLL).await;
            }
        });

        let mut this = Self {
            app,
            root: None,
            status: GitStatus::default(),
            pr: None,
            branches: vec![],
            error: None,
            new_branch,
            open: false,
            last_pr_check: None,
            _poll: poll,
        };
        this.sync_root(cx);
        this
    }

    fn sync_root(&mut self, cx: &mut Context<Self>) {
        let root = self.app.read(cx).current_project().map(|p| PathBuf::from(&p.root));
        if root != self.root {
            self.root = root;
            self.status = GitStatus::default();
            self.pr = None;
            self.last_pr_check = None;
            self.branches.clear();
            self.refresh(cx);
            cx.notify();
        }
    }

    /// Re-reads status (and the branch list when the popover is open) right away.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let with_branches = self.open;
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let (status, branches) = cx
                .background_spawn(async move {
                    let branches = if with_branches { git::local_branches(&r).ok() } else { None };
                    (git::status(&r), branches)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.root.as_ref() == Some(&root) {
                    this.status = status;
                    if let Some(b) = branches {
                        this.branches = b;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn run_git(
        &mut self,
        op: impl FnOnce(&std::path::Path) -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.root.clone() else { return };
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op(&root) }).await;
            let _ = this.update(cx, |this, cx| {
                this.error = result.err().map(|e| e.to_string());
                if this.error.is_none() {
                    this.open = false;
                }
                this.last_pr_check = None;
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            self.error = None;
            self.refresh(cx);
        }
        cx.notify();
    }

    fn label(&self) -> SharedString {
        match (&self.status.branch, &self.status.detached_at) {
            (Some(b), _) => b.clone().into(),
            (None, Some(sha)) => format!("detached @ {sha}").into(),
            _ => "no branch".into(),
        }
    }
}

impl Render for BranchApplet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.root.is_none() || !self.status.is_repo {
            return div().into_any_element();
        }
        let theme = cx.theme();
        let s = &self.status;
        let muted = theme.muted_foreground;
        let (green, red) = (theme.green, theme.red);

        let mut trigger_children: Vec<AnyElement> = vec![
            Icon::new(AppIcon::GitBranch).small().text_color(muted).into_any_element(),
            div().max_w(px(220.)).truncate().child(self.label()).into_any_element(),
        ];
        if s.changed_files > 0 {
            trigger_children.push(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .when(s.insertions + s.deletions == 0, |el| {
                        // Only untracked files: line counts would read "+0 −0".
                        el.text_color(muted).child(format!(
                            "{} file{}",
                            s.changed_files,
                            if s.changed_files == 1 { "" } else { "s" }
                        ))
                    })
                    .when(s.insertions + s.deletions > 0, |el| {
                        el.child(div().text_color(green).child(format!("+{}", s.insertions)))
                            .child(div().text_color(red).child(format!("−{}", s.deletions)))
                    })
                    .into_any_element(),
            );
        }
        if s.ahead > 0 || s.behind > 0 {
            trigger_children.push(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("↑{} ↓{}", s.ahead, s.behind))
                    .into_any_element(),
            );
        }
        if let Some(pr) = &self.pr {
            trigger_children.push(
                h_flex()
                    .gap_0p5()
                    .text_xs()
                    .text_color(muted)
                    .child(Icon::new(AppIcon::GitPullRequest).xsmall())
                    .child(format!("#{}", pr.number))
                    .into_any_element(),
            );
        }

        let trigger = Button::new("branch-applet")
            .ghost()
            .small()
            .child(h_flex().gap_1p5().items_center().children(trigger_children))
            .tooltip(match &s.upstream {
                Some(u) => format!("Tracking {u}"),
                None => "No upstream".into(),
            });

        let applet = cx.entity();
        Popover::new("branch-popover")
            .anchor(Anchor::TopRight)
            .trigger(trigger)
            .open(self.open)
            .on_open_change({
                let applet = applet.clone();
                move |open, _, cx| applet.update(cx, |this, cx| this.set_open(*open, cx))
            })
            .content(move |_, _window, cx| {
                let this = applet.read(cx);
                let current = this.status.branch.clone();
                let theme = cx.theme();
                let mut list = v_flex().gap_0p5().max_h(px(320.)).overflow_y_hidden();
                for b in this.branches.iter().take(40) {
                    let is_current = current.as_deref() == Some(b.as_str());
                    let name = b.clone();
                    let applet = applet.clone();
                    list = list.child(
                        Button::new(SharedString::from(format!("br-{b}")))
                            .ghost()
                            .small()
                            .w_full()
                            .justify_start()
                            .when(is_current, |b| b.icon(Icon::new(AppIcon::Check)))
                            .label(b.clone())
                            .on_click(move |_, _, cx| {
                                let name = name.clone();
                                applet.update(cx, |this, cx| {
                                    this.run_git(move |root| git::switch_branch(root, &name), cx)
                                });
                            }),
                    );
                }
                v_flex()
                    .w(px(300.))
                    .gap_2()
                    .child(div().text_xs().text_color(theme.muted_foreground).child("Switch branch"))
                    .child(Input::new(&this.new_branch).small())
                    .child(list)
                    .when_some(this.pr.clone(), |el, pr| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("PR #{} · {} · {}", pr.number, pr.state.to_lowercase(), pr.title)),
                        )
                    })
                    .when_some(this.error.clone(), |el, err| {
                        el.child(div().text_xs().text_color(theme.danger).child(err))
                    })
            })
            .into_any_element()
    }
}
