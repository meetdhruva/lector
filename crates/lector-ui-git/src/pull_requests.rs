//! Pull request review (v0.6, Agent FORGE): the Pull Requests sidebar panel
//! (review requested / mine / all), opening a pull request as a review — its head
//! and base are fetched into the **shadow repository**, never the user's — with
//! remote threads synced into the comment store, the "Submit review" dialog
//! (Approve / Request changes / Comment; confirmation, since it is public and
//! irreversible) and the sign-in help.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, prelude::*, App, Entity, FocusHandle, Focusable, FontWeight, SharedString, Task,
    WeakEntity, Window,
};
use lector_app::comments::{
    self as cm, Anchor, CommentStore, DiffSide, NewThread, Origin, ThreadFilter, ThreadState,
};
use lector_forge::{
    ChecksState, DraftComment, Forge, ForgeKind, ForgeRemote, PrFilter, PullRequest, RemoteComment,
    ReviewEvent, ReviewSubmission, ReviewThread, Side, SubmittedReview, Token,
};
use lector_git::{Changeset, Repo, ShadowRepo};
use lector_ui::components::text_area::TextArea;
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::confirm::{confirm, ConfirmSpec};
use crate::store::GitStore;
use crate::util::{now_secs, relative_time};

// ---------------------------------------------------------------------------------------------
// Session (blocking; no GPUI)
// ---------------------------------------------------------------------------------------------

/// The code host of a repository: the first remote (origin first) on GitHub / GitLab.
pub fn forge_remote(repo: &Repo, settings: &lector_config::Settings) -> Option<ForgeRemote> {
    let hosts = forge_hosts(settings);
    let mut remotes = repo.remotes().ok()?;
    remotes.sort_by_key(|r| match r.name.as_str() {
        "origin" => 0,
        "upstream" => 1,
        _ => 2,
    });
    remotes
        .iter()
        .find_map(|r| lector_forge::detect_with(&r.url, &hosts))
}

/// Forge hosts from settings `[forge]`.
pub fn forge_hosts(settings: &lector_config::Settings) -> lector_forge::ForgeHosts {
    lector_forge::ForgeHosts {
        github: settings.forge.github_hosts.clone(),
        gitlab: settings.forge.gitlab_hosts.clone(),
        api_urls: settings.forge.api_urls.clone(),
    }
}

/// One pull request opened for review.
pub struct PrSession {
    /// API client.
    pub forge: Box<dyn Forge>,
    /// Details (with files).
    pub pr: PullRequest,
    /// Review key of its comments (`pr:host/path#n`).
    pub key: String,
    /// Merge base of base and head (the old side of the review).
    pub merge_base: String,
    /// The shadow repository as a read-only `Repo` (diff contents come from here).
    pub shadow_repo: Repo,
    /// The user's work tree (comments are stored under it).
    pub workdir: PathBuf,
}

impl std::fmt::Debug for PrSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrSession")
            .field("key", &self.key)
            .field("merge_base", &self.merge_base)
            .finish_non_exhaustive()
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl PrSession {
    /// Loads pull request `number`, fetches its commits into the shadow repository
    /// (skipped when they are already reachable there) and computes the merge base.
    /// Blocking.
    pub fn open(
        data_dir: &std::path::Path,
        repo: &Repo,
        forge: Box<dyn Forge>,
        token: Option<&Token>,
        number: u64,
    ) -> Result<PrSession, String> {
        let pr = forge.pull_request(number).map_err(err)?;
        let shadow = ShadowRepo::open(data_dir, repo).map_err(err)?;
        let have = |sha: &str| !sha.is_empty() && matches!(shadow.resolve(sha), Ok(Some(_)));
        if !have(&pr.head_sha) || !have(&pr.base_sha) {
            let remote = forge.remote();
            let specs = forge.fetch_refspecs(&pr);
            let refs: Vec<&str> = specs.iter().map(String::as_str).collect();
            let opts = lector_git::shadow::FetchOptions {
                http_header: token
                    .filter(|_| remote.remote_url.starts_with("http"))
                    .map(|t| lector_forge::git_auth_header(remote.kind, t)),
            };
            shadow
                .fetch_with(&remote.remote_url, &refs, &opts)
                .map_err(|e| {
                    format!("cannot fetch {} #{number}: {e}", remote.kind.request_noun())
                })?;
        }
        let head = shadow
            .resolve(&pr.head_sha)
            .map_err(err)?
            .or_else(|| {
                shadow
                    .resolve(&format!("refs/lector/pr/{number}/head"))
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| format!("head commit {} is not available", short(&pr.head_sha)))?;
        let base = if have(&pr.base_sha) {
            pr.base_sha.clone()
        } else {
            format!("refs/lector/pr/{number}/base")
        };
        let merge_base = shadow
            .merge_base(&base, &head.to_string())
            .map_err(err)?
            .map(|o| o.to_string())
            .unwrap_or(base);
        let shadow_repo = Repo::discover(shadow.git_dir()).map_err(err)?;
        let key = lector_forge::review_key(forge.remote(), number);
        let mut pr = pr;
        pr.head_sha = head.to_string();
        Ok(PrSession {
            forge,
            pr,
            key,
            merge_base,
            shadow_repo,
            workdir: repo.workdir().to_path_buf(),
        })
    }

    /// The review's changeset (merge base → head), read from [`PrSession::shadow_repo`].
    pub fn changeset(&self) -> Changeset {
        Changeset::Range {
            from: self.merge_base.clone(),
            to: self.pr.head_sha.clone(),
        }
    }

    /// Tab title: "#12 Title".
    pub fn title(&self) -> String {
        format!("#{} {}", self.pr.number, self.pr.title)
    }

    fn origin(&self) -> Origin {
        match self.forge.kind() {
            ForgeKind::Github => Origin::Github,
            ForgeKind::Gitlab => Origin::Gitlab,
        }
    }

    fn text_at(&self, rev: &str, path: &str) -> Option<String> {
        let bytes = self.shadow_repo.file_at(rev, path).ok()??;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Replies on a remote thread.
    pub fn reply(&self, thread: &str, body: &str) -> Result<RemoteComment, String> {
        self.forge.reply(self.pr.number, thread, body).map_err(err)
    }

    /// Resolves / reopens a remote thread.
    pub fn resolve(&self, thread: &str, resolved: bool) -> Result<(), String> {
        self.forge
            .resolve_thread(self.pr.number, thread, resolved)
            .map_err(err)
    }

    /// Mirrors the pull request's review threads into `store` (new threads are anchored
    /// on the head / merge-base revision; known ones get new comments and states).
    /// Returns the number of threads.
    pub fn sync_threads(&self, store: &CommentStore) -> Result<usize, String> {
        let threads = self.forge.review_threads(self.pr.number).map_err(err)?;
        for rt in &threads {
            self.sync_thread(store, rt).map_err(err)?;
        }
        Ok(threads.len())
    }

    fn sync_thread(&self, store: &CommentStore, rt: &ReviewThread) -> Result<(), cm::CommentError> {
        let origin = self.origin();
        let state = if rt.resolved {
            ThreadState::Resolved
        } else if rt.outdated || rt.line.is_none() {
            ThreadState::Outdated
        } else {
            ThreadState::Open
        };
        if let Some(t) = store.find_remote(&self.workdir, origin, &rt.id)? {
            for c in &rt.comments {
                if !t
                    .comments
                    .iter()
                    .any(|x| x.remote_id.as_deref() == Some(c.id.as_str()))
                {
                    store.add_remote_comment(&self.workdir, t.id, remote_comment(c))?;
                }
            }
            if (t.state == ThreadState::Resolved) != rt.resolved {
                store.set_resolved(&self.workdir, t.id, rt.resolved)?;
            }
            return Ok(());
        }
        let Some(first) = rt.comments.first() else {
            return Ok(());
        };
        let side = match rt.side {
            Side::Old => DiffSide::Old,
            Side::New => DiffSide::New,
        };
        let rev = match side {
            DiffSide::New => self.pr.head_sha.clone(),
            DiffSide::Old => self.merge_base.clone(),
        };
        let line = rt.line.or(rt.original_line).unwrap_or(0);
        let text = self.text_at(&rev, &rt.path).unwrap_or_default();
        let start = rt.start_line.filter(|s| *s < line);
        let anchor = Anchor::capture(
            &rt.path,
            side,
            &text,
            start.unwrap_or(line),
            start.map(|_| line),
            &self.key,
            None,
        )
        .unwrap_or_else(|| Anchor {
            path: rt.path.clone(),
            side,
            line,
            changeset: self.key.clone(),
            ..Default::default()
        });
        store.remember_text(&text);
        let t = store.add(NewThread {
            repo: self.workdir.clone(),
            anchor,
            body: first.body.clone(),
            author: first.author.clone(),
            draft: false,
            origin,
            remote_id: Some(rt.id.clone()),
            remote_comment_id: Some(first.id.clone()),
            created: first.created,
            state,
        })?;
        for c in rt.comments.iter().skip(1) {
            store.add_remote_comment(&self.workdir, t.id, remote_comment(c))?;
        }
        Ok(())
    }

    /// Draft threads / replies of this review in `store`.
    pub fn drafts(&self, store: &CommentStore) -> Vec<cm::Thread> {
        store
            .threads(
                &self.workdir,
                &ThreadFilter {
                    changeset: Some(self.key.clone()),
                    drafts_only: true,
                    ..Default::default()
                },
            )
            .unwrap_or_default()
    }

    /// Publishes the review: draft replies to remote threads are posted as replies,
    /// draft threads become inline comments of one review with `event` and `body`.
    /// Published local drafts are replaced by the host's threads (re-synced).
    pub fn submit(
        &self,
        store: &CommentStore,
        event: ReviewEvent,
        body: &str,
    ) -> Result<SubmittedReview, String> {
        let drafts = self.drafts(store);
        let mut inline = Vec::new();
        let mut inline_threads = Vec::new();
        for t in &drafts {
            let remote = t.remote_id.clone().filter(|_| t.origin != Origin::Local);
            match remote {
                Some(rid) => {
                    for c in t.comments.iter().filter(|c| c.draft) {
                        let posted = self.reply(&rid, &c.body)?;
                        store
                            .mark_published(&self.workdir, t.id, None, &[(c.id, posted.id)])
                            .map_err(err)?;
                    }
                }
                None => {
                    let text = t
                        .comments
                        .iter()
                        .filter(|c| c.draft)
                        .map(|c| c.body.trim())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let end = t
                        .anchor
                        .end_line
                        .unwrap_or(t.anchor.line)
                        .max(t.anchor.line);
                    inline.push(DraftComment {
                        path: t.anchor.path.clone(),
                        old_path: None,
                        line: end,
                        start_line: t.anchor.end_line.map(|_| t.anchor.line),
                        side: match t.anchor.side {
                            DiffSide::Old => Side::Old,
                            DiffSide::New => Side::New,
                        },
                        body: text,
                    });
                    inline_threads.push(t.id);
                }
            }
        }
        let review = ReviewSubmission {
            event,
            body: body.to_string(),
            comments: inline,
            head_sha: self.pr.head_sha.clone(),
            base_sha: self.pr.base_sha.clone(),
            start_sha: self.merge_base.clone(),
        };
        let submitted = self
            .forge
            .submit_review(self.pr.number, &review)
            .map_err(err)?;
        for id in inline_threads {
            let _ = store.delete(&self.workdir, id);
        }
        if let Err(e) = self.sync_threads(store) {
            tracing::warn!("re-syncing review threads after submitting: {e}");
        }
        Ok(submitted)
    }
}

fn remote_comment(c: &RemoteComment) -> cm::Comment {
    cm::Comment {
        author: c.author.clone(),
        body: c.body.clone(),
        created: c.created,
        updated: c.created,
        remote_id: Some(c.id.clone()),
        ..Default::default()
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

// ---------------------------------------------------------------------------------------------
// Pull Requests panel
// ---------------------------------------------------------------------------------------------

enum PanelState {
    /// Repository has no GitHub / GitLab remote.
    NoRemote,
    /// No token found.
    NeedsToken(ForgeRemote),
    Loading,
    Error(SharedString),
    Loaded(Vec<PullRequest>),
}

/// The Pull Requests sidebar panel.
pub struct PullRequestsPanel {
    store: Entity<GitStore>,
    remote: Option<ForgeRemote>,
    filter: PrFilter,
    state: PanelState,
    opening: Option<(u64, Task<()>)>,
    open_error: Option<SharedString>,
    load_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl Focusable for PullRequestsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PullRequestsPanel {
    /// The panel of `store`'s repository.
    pub fn new(store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let settings = store.read(cx).settings(cx);
        let filter = PrFilter::parse(&settings.forge.default_filter).unwrap_or_default();
        let remote = store
            .read(cx)
            .repo()
            .and_then(|r| forge_remote(r, &settings));
        PullRequestsPanel {
            store,
            remote,
            filter,
            state: PanelState::Loading,
            opening: None,
            open_error: None,
            load_task: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Loads the list (first time the panel is shown, refresh button, filter change).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.clone() else {
            self.state = PanelState::NoRemote;
            cx.notify();
            return;
        };
        self.state = PanelState::Loading;
        let filter = self.filter;
        let task = cx.background_spawn(async move {
            let Some(token) = lector_forge::auth_token(&remote) else {
                return Err(None);
            };
            let forge = lector_forge::connect(&remote, token);
            forge
                .list_pull_requests(filter)
                .map_err(|e| Some(SharedString::from(e.to_string())))
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = match r {
                    Ok(list) => PanelState::Loaded(list),
                    Err(None) => match this.remote.clone() {
                        Some(remote) => PanelState::NeedsToken(remote),
                        None => PanelState::NoRemote,
                    },
                    Err(Some(e)) => PanelState::Error(e),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_filter(&mut self, f: PrFilter, cx: &mut Context<Self>) {
        if self.filter != f {
            self.filter = f;
            self.refresh(cx);
        }
    }

    fn open(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.clone() else {
            return;
        };
        let Some(repo) = self.store.read(cx).repo().cloned() else {
            return;
        };
        self.open_error = None;
        let store = self.store.clone();
        let comments = CommentStore::for_core(&lector_ui::commands_bridge::core(cx));
        let work = cx.background_spawn(async move {
            let token = lector_forge::auth_token(&remote)
                .ok_or_else(|| "no token: see Review > Sign In".to_string())?;
            let forge = lector_forge::connect(&remote, token.clone());
            let session = PrSession::open(
                &lector_config::paths::data_dir(),
                &repo,
                forge,
                Some(&token),
                number,
            )?;
            if let Err(e) = session.sync_threads(&comments) {
                tracing::warn!("cannot load review threads: {e}");
            }
            Ok::<_, String>(Arc::new(session))
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let r = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.opening = None;
                match r {
                    Ok(session) => {
                        crate::review_tab::open_pull_request(&store, session, window, cx)
                    }
                    Err(e) => this.open_error = Some(e.into()),
                }
                cx.notify();
            });
        });
        self.opening = Some((number, task));
        cx.notify();
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let tab = |id: &'static str, label: &'static str, f: PrFilter, cx: &mut Context<Self>| {
            let active = self.filter == f;
            div()
                .id(id)
                .px(px(8.))
                .py(px(3.))
                .rounded(metrics::radius())
                .text_size(metrics::text_xs())
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if active {
                    ui.text.hsla()
                } else {
                    ui.text_muted.hsla()
                })
                .when(active, |el| el.bg(ui.active.hsla()))
                .hover(|s| s.bg(ui.hover.hsla()))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
        };
        h_flex()
            .gap(px(2.))
            .px(px(8.))
            .py(px(6.))
            .child(tab(
                "pr-filter-review",
                "Review requested",
                PrFilter::ReviewRequested,
                cx,
            ))
            .child(tab("pr-filter-mine", "Mine", PrFilter::Mine, cx))
            .child(tab("pr-filter-all", "All", PrFilter::All, cx))
    }

    fn render_row(&self, pr: &PullRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let n = pr.number;
        let opening = self.opening.as_ref().is_some_and(|(o, _)| *o == n);
        let now = now_secs();
        let checks = pr.checks.map(|c| match c {
            ChecksState::Success => (IconName::CircleCheck, ui.success.hsla()),
            ChecksState::Failure => (IconName::CircleX, ui.error.hsla()),
            ChecksState::Pending => (IconName::Clock, ui.warning.hsla()),
        });
        v_flex()
            .id(SharedString::from(format!("pr-row-{n}")))
            .px(px(10.))
            .py(px(6.))
            .gap(px(2.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .cursor_pointer()
            .hover(|s| s.bg(ui.hover.hsla()))
            .when(opening, |el| el.bg(ui.active.hsla()))
            .on_click(cx.listener(move |this, _, window, cx| this.open(n, window, cx)))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::GitPullRequest)
                            .size(IconSize::Small)
                            .color(if pr.draft {
                                ui.text_faint.hsla()
                            } else {
                                ui.success.hsla()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(metrics::text_sm())
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ui.text.hsla())
                            .child(pr.title.clone()),
                    )
                    .when(opening, |el| {
                        el.child(
                            Icon::new(IconName::Loader)
                                .size(IconSize::Small)
                                .color(ui.text_muted.hsla()),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .pl(px(20.))
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_faint.hsla())
                    .child(format!("#{n}"))
                    .child(pr.author.clone())
                    .child(format!("updated {}", relative_time(pr.updated, now)))
                    .when(pr.draft, |el| {
                        el.child(div().text_color(ui.text_muted.hsla()).child("Draft"))
                    })
                    .when_some(checks, |el, (icon, color)| {
                        el.child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    }),
            )
            .when(!pr.labels.is_empty() || !pr.head_ref.is_empty(), |el| {
                el.child(
                    h_flex()
                        .pl(px(20.))
                        .gap(px(4.))
                        .flex_wrap()
                        .child(
                            div()
                                .px(px(5.))
                                .rounded(px(3.))
                                .bg(ui.surface.hsla())
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_muted.hsla())
                                .child(format!("{} → {}", pr.head_ref, pr.base_ref)),
                        )
                        .children(pr.labels.iter().map(|l| {
                            div()
                                .px(px(6.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(ui.border.hsla())
                                .text_size(metrics::text_xs())
                                .text_color(ui.text_accent.hsla())
                                .child(l.clone())
                        })),
                )
            })
    }

    fn render_message(
        &self,
        title: &str,
        body: SharedString,
        sign_in: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        v_flex()
            .p(px(16.))
            .gap(px(8.))
            .child(
                div()
                    .text_size(metrics::text_sm())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_muted.hsla())
                    .child(body),
            )
            .when(sign_in, |el| {
                el.child(
                    h_flex().gap(px(6.)).child(
                        Button::new("pr-sign-in", "How to sign in…")
                            .style(ButtonStyle::Outlined)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(lector_ui::actions::review::SignIn),
                                    cx,
                                )
                            }),
                    ),
                )
            })
    }
}

impl Render for PullRequestsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(self.state, PanelState::Loading) && self.load_task.is_none() {
            self.refresh(cx);
        }
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let label = self
            .remote
            .as_ref()
            .map(|r| r.label())
            .unwrap_or_else(|| "No code host".into());
        let header = h_flex()
            .px(px(10.))
            .h(px(30.))
            .gap(px(6.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                Icon::new(IconName::Globe)
                    .size(IconSize::Small)
                    .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(metrics::text_xs())
                    .text_color(ui.text_muted.hsla())
                    .child(label),
            )
            .child(
                IconButton::new("pr-refresh", IconName::Refresh)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let body = match &self.state {
            PanelState::NoRemote => self
                .render_message(
                    "No GitHub or GitLab remote",
                    "Pull requests appear when a remote points at github.com, gitlab.com or a host listed in settings [forge].".into(),
                    false,
                    cx,
                )
                .into_any_element(),
            PanelState::NeedsToken(remote) => {
                let msg = format!(
                    "Lector needs a {} token to list {}s.",
                    remote.kind.label(),
                    remote.kind.request_noun()
                );
                self.render_message("Sign in", msg.into(), true, cx)
                    .into_any_element()
            }
            PanelState::Loading => div()
                .p(px(16.))
                .text_size(metrics::text_xs())
                .text_color(ui.text_faint.hsla())
                .child("Loading…")
                .into_any_element(),
            PanelState::Error(e) => {
                let e = e.clone();
                self.render_message("Could not load pull requests", e, true, cx)
                    .into_any_element()
            }
            PanelState::Loaded(list) if list.is_empty() => {
                let what = match self.filter {
                    PrFilter::ReviewRequested => "Nobody is waiting for your review.",
                    PrFilter::Mine => "You have no open pull requests.",
                    PrFilter::All => "No open pull requests.",
                };
                self.render_message("All clear", what.into(), false, cx)
                    .into_any_element()
            }
            PanelState::Loaded(list) => {
                let list = list.clone();
                v_flex()
                    .id("pr-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(list.iter().map(|pr| self.render_row(pr, cx).into_any_element()))
                    .into_any_element()
            }
        };
        v_flex()
            .id("pull-requests-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(header)
            .child(self.render_filters(cx))
            .when_some(self.open_error.clone(), |el, e| {
                el.child(
                    div()
                        .mx(px(8.))
                        .mb(px(6.))
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(metrics::radius())
                        .bg(ui.error.hsla().opacity(0.12))
                        .text_size(metrics::text_xs())
                        .text_color(ui.error.hsla())
                        .child(e),
                )
            })
            .child(body)
    }
}

// ---------------------------------------------------------------------------------------------
// Submit review dialog
// ---------------------------------------------------------------------------------------------

/// "Submit review": verdict, summary, draft count; asks for confirmation before
/// publishing.
pub struct SubmitReviewDialog {
    session: Arc<PrSession>,
    workspace: WeakEntity<Workspace>,
    event: ReviewEvent,
    summary: Entity<TextArea>,
    drafts: usize,
    on_done: Option<DoneFn>,
    focus_handle: FocusHandle,
}

type DoneFn = Box<dyn FnOnce(&mut Window, &mut App)>;

impl gpui::EventEmitter<gpui::DismissEvent> for SubmitReviewDialog {}

impl Focusable for SubmitReviewDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.summary.focus_handle(cx)
    }
}

impl SubmitReviewDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else {
            return;
        };
        let session = self.session.clone();
        let event = self.event;
        let body = self.summary.read(cx).text().trim().to_string();
        let drafts = self.drafts;
        let on_done = self.on_done.take();
        cx.emit(gpui::DismissEvent);
        let target = format!("{} #{}", session.forge.remote().label(), session.pr.number);
        let spec = ConfirmSpec {
            title: format!("Publish review ({})?", event.label()).into(),
            message: vec![
                format!(
                    "{drafts} draft comment{} and your summary will be posted to {target} as {}. Everyone with access to the repository will see them; this cannot be undone from Lector.",
                    if drafts == 1 { "" } else { "s" },
                    event.label().to_lowercase()
                )
                .into(),
            ],
            items: Vec::new(),
            confirm_label: "Publish review".into(),
        };
        let weak_ws = ws.downgrade();
        confirm(&ws, spec, window, cx, move |window, cx| {
            let store = CommentStore::for_core(&lector_ui::commands_bridge::core(cx));
            let task = cx.background_spawn(async move { session.submit(&store, event, &body) });
            window
                .spawn(cx, async move |cx| {
                    let r = task.await;
                    let _ = weak_ws.update(cx, |ws, cx| match r {
                        Ok(s) => {
                            let mut msg = format!("Review submitted to {target}");
                            if let Some(n) = s.notes.first() {
                                msg.push_str(&format!(" — {n}"));
                            }
                            ws.show_toast(msg, cx)
                        }
                        Err(e) => ws.show_toast(format!("Review not submitted: {e}"), cx),
                    });
                    if let Some(f) = on_done {
                        let _ = cx.update(|window, cx| f(window, cx));
                    }
                })
                .detach();
        });
    }
}

impl Render for SubmitReviewDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let option =
            |id: &'static str, ev: ReviewEvent, desc: &'static str, cx: &mut Context<Self>| {
                let selected = self.event == ev;
                h_flex()
                    .id(id)
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(metrics::radius())
                    .border_1()
                    .border_color(if selected {
                        ui.accent.hsla()
                    } else {
                        ui.border_variant.hsla()
                    })
                    .when(selected, |el| el.bg(ui.accent.hsla().opacity(0.08)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.event = ev;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if selected {
                            IconName::CircleDot
                        } else {
                            IconName::Circle
                        })
                        .size(IconSize::Small)
                        .color(if selected {
                            ui.accent.hsla()
                        } else {
                            ui.text_faint.hsla()
                        }),
                    )
                    .child(
                        v_flex()
                            .child(
                                div()
                                    .text_size(metrics::text_sm())
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(ui.text.hsla())
                                    .child(ev.label()),
                            )
                            .child(
                                div()
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.text_muted.hsla())
                                    .child(desc),
                            ),
                    )
            };
        let noun = self.session.forge.kind().request_noun();
        v_flex()
            .id("submit-review")
            .key_context("SubmitReviewDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                let k = &e.keystroke;
                if k.key == "escape" {
                    cx.emit(gpui::DismissEvent);
                    cx.stop_propagation();
                } else if k.key == "enter" && (k.modifiers.control || k.modifiers.platform) {
                    this.submit(window, cx);
                    cx.stop_propagation();
                }
            }))
            .w(px(520.))
            .p(px(16.))
            .gap(px(10.))
            .rounded(metrics::radius_lg())
            .border_1()
            .border_color(ui.border.hsla())
            .bg(ui.elevated.hsla())
            .shadow_lg()
            .child(
                div()
                    .text_size(metrics::text())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(format!("Submit review — {}", self.session.title())),
            )
            .child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(metrics::radius())
                    .bg(ui.surface.hsla())
                    .border_1()
                    .border_color(ui.border_variant.hsla())
                    .min_h(px(72.))
                    .child(self.summary.clone()),
            )
            .child(option(
                "review-comment",
                ReviewEvent::Comment,
                "Submit general feedback without explicit approval.",
                cx,
            ))
            .child(option(
                "review-approve",
                ReviewEvent::Approve,
                "Approve merging these changes.",
                cx,
            ))
            .child(option(
                "review-request",
                ReviewEvent::RequestChanges,
                "Submit feedback that must be addressed before merging.",
                cx,
            ))
            .child(
                h_flex()
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(ui.text_faint.hsla()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child(format!(
                                "{} draft comment{} will be published on the {noun}.",
                                self.drafts,
                                if self.drafts == 1 { "" } else { "s" }
                            )),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .justify_end()
                    .child(
                        Button::new("submit-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(gpui::DismissEvent))),
                    )
                    .child(
                        Button::new("submit-ok", "Submit review…")
                            .style(ButtonStyle::Accent)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Opens the "Submit review" dialog for `session`; `on_done` runs after publishing.
pub fn show_submit_dialog(
    workspace: &Entity<Workspace>,
    session: Arc<PrSession>,
    window: &mut Window,
    cx: &mut App,
    on_done: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let store = CommentStore::for_core(&lector_ui::commands_bridge::core(cx));
    let drafts = session.drafts(&store).iter().map(|t| t.draft_count()).sum();
    let layer = workspace.read(cx).modal_layer().clone();
    let weak = workspace.downgrade();
    let dialog = cx.new(|cx| SubmitReviewDialog {
        session,
        workspace: weak,
        event: ReviewEvent::Comment,
        summary: cx.new(|cx| {
            let mut a = TextArea::new("Leave a summary (optional, Markdown)", cx);
            a.set_line_limits(3, 10, cx);
            a
        }),
        drafts,
        on_done: Some(Box::new(on_done)),
        focus_handle: cx.focus_handle(),
    });
    layer.update(cx, |layer, cx| layer.show_modal(dialog.clone(), window, cx));
    let f = dialog.read(cx).summary.read(cx).focus_handle(cx);
    window.focus(&f);
}

// ---------------------------------------------------------------------------------------------
// Sign-in help
// ---------------------------------------------------------------------------------------------

/// `review::SignIn`: explains where Lector looks for a token (as a dialog).
pub fn show_sign_in_help(store: &Entity<GitStore>, window: &mut Window, cx: &mut App) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let settings = store.read(cx).settings(cx);
    let remote = store
        .read(cx)
        .repo()
        .and_then(|r| forge_remote(r, &settings));
    let (title, body) = match &remote {
        Some(r) => (
            format!("Sign in to {}", r.host),
            lector_forge::token_help(r),
        ),
        None => (
            "Sign in to GitHub / GitLab".to_string(),
            "This repository has no GitHub or GitLab remote. Lector recognises github.com, gitlab.com and the hosts listed in settings [forge] github_hosts / gitlab_hosts. Tokens come from GITHUB_TOKEN / GH_TOKEN / GITLAB_TOKEN, then the gh / glab CLIs, then the OS keychain (service lector-forge).".to_string(),
        ),
    };
    let layer = ws.read(cx).modal_layer().clone();
    let view = cx.new(|cx| SignInHelp {
        title: title.into(),
        body: body.into(),
        focus_handle: cx.focus_handle(),
    });
    layer.update(cx, |l, cx| l.show_modal(view, window, cx));
}

/// The sign-in explanation dialog.
pub struct SignInHelp {
    title: SharedString,
    body: SharedString,
    focus_handle: FocusHandle,
}

impl gpui::EventEmitter<gpui::DismissEvent> for SignInHelp {}

impl Focusable for SignInHelp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SignInHelp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let blocks = lector_ui::markdown::parse(&self.body);
        let opts = lector_ui::markdown::RenderOptions {
            base_dir: None,
            on_link: None,
            compact: true,
            id: "sign-in-help".into(),
        };
        let body = lector_ui::markdown::render_blocks(&blocks, &opts, window, cx);
        v_flex()
            .id("sign-in-help")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if matches!(e.keystroke.key.as_str(), "escape" | "enter") {
                    cx.emit(gpui::DismissEvent);
                }
            }))
            .w(px(520.))
            .p(px(16.))
            .gap(px(10.))
            .rounded(metrics::radius_lg())
            .border_1()
            .border_color(ui.border.hsla())
            .bg(ui.elevated.hsla())
            .shadow_lg()
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::Small)
                            .color(ui.accent.hsla()),
                    )
                    .child(
                        div()
                            .text_size(metrics::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(self.title.clone()),
                    ),
            )
            .child(
                v_flex()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_muted.hsla())
                    .children(body),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("sign-in-ok", "OK")
                        .style(ButtonStyle::Accent)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(gpui::DismissEvent))),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use lector_app::Db;
    use lector_forge::mock::{MockServer, Route};

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "gc.auto=0",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn open_sync_and_submit_a_pull_request() {
        let up = tempfile::tempdir().expect("tmp");
        let up = up.path();
        git(up, &["init", "-q", "-b", "main"]);
        std::fs::write(up.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").expect("w");
        git(up, &["add", "."]);
        git(up, &["commit", "-q", "-m", "base"]);
        let base = git(up, &["rev-parse", "HEAD"]);
        git(up, &["checkout", "-q", "-b", "pr"]);
        std::fs::write(
            up.join("a.rs"),
            "fn a() {}\nfn b() { 2 }\nfn c() {}\nfn d() {}\n",
        )
        .expect("w");
        git(up, &["commit", "-q", "-am", "pr"]);
        let head = git(up, &["rev-parse", "HEAD"]);
        git(up, &["update-ref", "refs/pull/12/head", "HEAD"]);
        git(up, &["checkout", "-q", "main"]);

        let user = tempfile::tempdir().expect("tmp");
        let user_path = dunce::canonicalize(user.path()).expect("canon");
        git(&user_path, &["init", "-q", "-b", "main"]);
        git(&user_path, &["fetch", "-q", &up.to_string_lossy(), "main"]);
        git(&user_path, &["reset", "-q", "--hard", "FETCH_HEAD"]);
        let user_refs_before = git(&user_path, &["for-each-ref"]);

        let pr_json = format!(
            r#"{{"number": 12, "title": "B returns 2", "state": "open", "user": {{"login": "grace"}},
               "head": {{"sha": "{head}", "ref": "pr"}}, "base": {{"sha": "{base}", "ref": "main"}}}}"#
        );
        let threads = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[
            {"id":"T1","isResolved":false,"isOutdated":false,"viewerCanResolve":true,"path":"a.rs","diffSide":"RIGHT","line":2,
             "comments":{"nodes":[{"id":"C1","databaseId":1,"body":"why 2?","createdAt":"2026-11-01T00:00:00Z","author":{"login":"linus"}}]}}]}}}}}"#;
        let server = MockServer::start(
            0,
            vec![
                Route::json("GET", "/repos/acme/widgets/pulls/12", pr_json),
                Route::json("GET", "/repos/acme/widgets/pulls/12/files", "[]"),
                Route::json("POST", "/graphql", threads).when_body("reviewThreads"),
                Route::json(
                    "POST",
                    "/repos/acme/widgets/pulls/12/reviews",
                    r#"{"id": 9}"#,
                ),
            ],
        )
        .expect("server");
        let remote = ForgeRemote {
            kind: ForgeKind::Github,
            host: "github.com".into(),
            path: "acme/widgets".into(),
            owner: "acme".into(),
            name: "widgets".into(),
            api_base: server.url(),
            web_base: "https://github.com".into(),
            remote_url: up.to_string_lossy().into_owned(),
        };
        let data = tempfile::tempdir().expect("tmp");
        let repo = Repo::discover(&user_path).expect("repo");
        let forge = lector_forge::connect(&remote, Token::new("t"));
        let session = PrSession::open(data.path(), &repo, forge, None, 12).expect("open");
        assert_eq!(session.pr.head_sha, head);
        assert_eq!(session.merge_base, base);
        assert_eq!(session.key, "pr:github.com/acme/widgets#12");
        let changes = session
            .shadow_repo
            .changes(&session.changeset())
            .expect("changes");
        assert_eq!(changes.len(), 1);
        assert_eq!(
            git(&user_path, &["for-each-ref"]),
            user_refs_before,
            "user refs untouched"
        );

        let store = CommentStore::new(Db::in_memory());
        assert_eq!(session.sync_threads(&store).expect("sync"), 1);
        session.sync_threads(&store).expect("sync again");
        let all = store
            .threads(&user_path, &ThreadFilter::changeset(session.key.clone()))
            .expect("threads");
        assert_eq!(all.len(), 1, "no duplicates");
        assert_eq!(all[0].origin, Origin::Github);
        assert_eq!(all[0].anchor.line, 1);
        assert_eq!(all[0].anchor.text, "fn b() { 2 }");

        let text = session.text_at(&head, "a.rs").expect("text");
        let anchor = Anchor::capture("a.rs", DiffSide::New, &text, 3, None, &session.key, None)
            .expect("anchor");
        store
            .add(NewThread {
                repo: user_path.clone(),
                anchor,
                body: "new fn needs docs".into(),
                draft: true,
                ..Default::default()
            })
            .expect("draft");
        assert_eq!(session.drafts(&store).len(), 1);
        session
            .submit(&store, ReviewEvent::RequestChanges, "Please document")
            .expect("submit");
        let review = server
            .requests()
            .into_iter()
            .find(|r| r.target.ends_with("/reviews"))
            .expect("review posted");
        let body: serde_json::Value = serde_json::from_str(&review.body).expect("json");
        assert_eq!(body["event"], "REQUEST_CHANGES");
        assert_eq!(body["comments"][0]["line"], 4);
        assert_eq!(body["comments"][0]["path"], "a.rs");
        assert_eq!(body["commit_id"], head.as_str());
        assert!(
            session.drafts(&store).is_empty(),
            "published drafts are gone"
        );
    }
}
