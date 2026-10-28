use auto_update::{AutoUpdater, release_notes_url};
use db::kvp::{Dismissable, KeyValueStore};
use editor::{Editor, MultiBuffer};
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global,
    TaskExt, Window, actions, prelude::*,
};
use markdown_preview::markdown_preview_view::{MarkdownPreviewMode, MarkdownPreviewView};
use project::DisableAiSettings;
use release_channel::{AppVersion, ReleaseChannel, RpReleaseMetadata, rp_release_metadata};
use semver::Version;
use serde::Deserialize;
use settings::Settings as _;
use smol::io::AsyncReadExt;
use ui::{AnnouncementToast, DeltaIllustration, ListBulletItem, prelude::*};
use util::{ResultExt as _, maybe};
use workspace::{
    Workspace,
    notifications::{
        Notification, NotificationId, SuppressEvent, show_app_notification,
        simple_message_notification::MessageNotification,
    },
    workspace_error::{ErrorAction, ErrorSeverity, WorkspaceError},
};
use zed_actions::ShowUpdateNotification;

actions!(
    auto_update,
    [
        /// Opens the release notes for the current version in a new tab.
        ViewReleaseNotesLocally
    ]
);

const RP_RELEASE_NOTES_KVP_KEY: &str = "rp_release_notes_last_displayed_version";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseNotesSource {
    Rp(RpReleaseMetadata),
    UpstreamBrowser,
    UpstreamLocal,
}

fn release_notes_source(
    rp_release: Option<RpReleaseMetadata>,
    release_channel: ReleaseChannel,
) -> ReleaseNotesSource {
    if let Some(rp_release) = rp_release {
        ReleaseNotesSource::Rp(rp_release)
    } else if matches!(
        release_channel,
        ReleaseChannel::Nightly | ReleaseChannel::Dev
    ) {
        ReleaseNotesSource::UpstreamBrowser
    } else {
        ReleaseNotesSource::UpstreamLocal
    }
}

#[derive(Default)]
struct RpReleaseNotesOpenState {
    in_flight_version: Option<String>,
}

impl Global for RpReleaseNotesOpenState {}

impl RpReleaseNotesOpenState {
    fn reserve(
        &mut self,
        current_version: &str,
        last_shown_version: Option<&str>,
        force: bool,
    ) -> bool {
        if rp_release_notes_open_decision(
            current_version,
            last_shown_version,
            self.in_flight_version.as_deref(),
            force,
        ) != RpReleaseNotesOpenDecision::Open
        {
            return false;
        }

        self.in_flight_version = Some(current_version.to_owned());
        true
    }

    fn release(&mut self, version: &str) {
        if self.in_flight_version.as_deref() == Some(version) {
            self.in_flight_version = None;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RpReleaseNotesOpenDecision {
    Open,
    AlreadyShown,
    AlreadyOpening,
}

fn rp_release_notes_open_decision(
    current_version: &str,
    last_shown_version: Option<&str>,
    in_flight_version: Option<&str>,
    force: bool,
) -> RpReleaseNotesOpenDecision {
    if in_flight_version.is_some() {
        RpReleaseNotesOpenDecision::AlreadyOpening
    } else if !force && last_shown_version == Some(current_version) {
        RpReleaseNotesOpenDecision::AlreadyShown
    } else {
        RpReleaseNotesOpenDecision::Open
    }
}

pub fn init(cx: &mut App) {
    notify_if_app_was_updated(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ViewReleaseNotesLocally, window, cx| {
            view_release_notes_locally(workspace, window, cx);
        });

        if matches!(
            ReleaseChannel::global(cx),
            ReleaseChannel::Nightly | ReleaseChannel::Dev
        ) {
            workspace.register_action(|_workspace, _: &ShowUpdateNotification, _window, cx| {
                show_update_notification(cx);
            });
        }

        if let Some(window) = window {
            maybe_open_rp_release_notes(workspace, window, cx);
        }
    })
    .detach();
}

#[derive(Deserialize)]
struct ReleaseNotesBody {
    title: String,
    release_notes: String,
}

fn notify_release_notes_failed_to_show(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let url = release_notes_url(cx);

    struct ReleaseNotesError {
        url: Option<String>,
    }

    impl WorkspaceError for ReleaseNotesError {
        fn primary_message(&self) -> SharedString {
            "Couldn't load release notes".into()
        }
        fn severity(&self) -> ErrorSeverity {
            ErrorSeverity::Error
        }
        fn primary_action(&self) -> ErrorAction {
            self.url
                .clone()
                .map(|url| ErrorAction::link("View in Browser", url))
                .unwrap_or_else(ErrorAction::dismiss)
        }
    }

    workspace.show_error(ReleaseNotesError { url }, cx);
}

fn view_release_notes_locally(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let release_channel = ReleaseChannel::global(cx);

    match release_notes_source(rp_release_metadata(), release_channel) {
        ReleaseNotesSource::Rp(rp_release) => {
            if reserve_rp_release_notes_open(rp_release.calendar_version, None, true, cx) {
                open_rp_release_notes(workspace, window, rp_release, cx);
            }
            return;
        }
        ReleaseNotesSource::UpstreamBrowser => {
            if let Some(url) = release_notes_url(cx) {
                cx.open_url(&url);
            }
            return;
        }
        ReleaseNotesSource::UpstreamLocal => {}
    }

    let version = AppVersion::global(cx).to_string();

    let client = client::Client::global(cx).http_client();
    let url = client.build_url(&format!(
        "/api/release_notes/v2/{}/{}",
        release_channel.dev_name(),
        version
    ));

    let markdown = workspace
        .app_state()
        .languages
        .language_for_name("Markdown");

    cx.spawn_in(window, async move |workspace, cx| {
        let markdown = markdown.await.log_err();
        let response = client.get(&url, Default::default(), true).await;
        let Some(mut response) = response.log_err() else {
            workspace
                .update_in(cx, notify_release_notes_failed_to_show)
                .log_err();
            return;
        };

        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await.ok();

        let body: serde_json::Result<ReleaseNotesBody> = serde_json::from_slice(body.as_slice());

        let res: Option<()> = maybe!(async {
            let body = body.ok()?;
            let project = workspace
                .read_with(cx, |workspace, _| workspace.project().clone())
                .ok()?;
            let (language_registry, buffer) = project.update(cx, |project, cx| {
                (
                    project.languages().clone(),
                    project.create_buffer(markdown, false, cx),
                )
            });
            let buffer = buffer.await.ok()?;
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, body.release_notes)], None, cx)
            });

            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(body.title));

            let ws_handle = workspace.clone();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let editor =
                        cx.new(|cx| Editor::for_multibuffer(buffer, Some(project), window, cx));
                    let markdown_preview: Entity<MarkdownPreviewView> = MarkdownPreviewView::new(
                        MarkdownPreviewMode::Default,
                        editor,
                        ws_handle,
                        language_registry,
                        window,
                        cx,
                    );
                    workspace.add_item_to_active_pane(
                        Box::new(markdown_preview),
                        None,
                        true,
                        window,
                        cx,
                    );
                    cx.notify();
                })
                .ok()
        })
        .await;
        if res.is_none() {
            workspace
                .update_in(cx, notify_release_notes_failed_to_show)
                .log_err();
        }
    })
    .detach();
}

fn reserve_rp_release_notes_open(
    current_version: &str,
    last_shown_version: Option<&str>,
    force: bool,
    cx: &mut App,
) -> bool {
    cx.default_global::<RpReleaseNotesOpenState>().reserve(
        current_version,
        last_shown_version,
        force,
    )
}

fn maybe_open_rp_release_notes(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(rp_release) = rp_release_metadata() else {
        return;
    };
    let Some(last_shown_version) = KeyValueStore::global(cx)
        .read_kvp(RP_RELEASE_NOTES_KVP_KEY)
        .log_err()
    else {
        return;
    };
    if reserve_rp_release_notes_open(
        rp_release.calendar_version,
        last_shown_version.as_deref(),
        false,
        cx,
    ) {
        open_rp_release_notes(workspace, window, rp_release, cx);
    }
}

fn open_rp_release_notes(
    workspace: &mut Workspace,
    window: &mut Window,
    rp_release: RpReleaseMetadata,
    cx: &mut Context<Workspace>,
) {
    let window_handle = window.window_handle();
    let markdown = workspace
        .app_state()
        .languages
        .language_for_name("Markdown");
    let title = release_channel::rp_release_notes_title(rp_release, &AppVersion::global(cx));

    cx.spawn(async move |workspace, cx| {
        let cleanup_cx = cx.clone();
        let _release_reservation = util::defer(move || {
            cleanup_cx.update(|cx| {
                cx.default_global::<RpReleaseNotesOpenState>()
                    .release(rp_release.calendar_version);
            });
        });
        let markdown = markdown.await.log_err();
        let res: Option<()> = maybe!(async {
            let project = workspace
                .read_with(cx, |workspace, _| workspace.project().clone())
                .ok()?;
            let (language_registry, buffer) = project.update(cx, |project, cx| {
                (
                    project.languages().clone(),
                    project.create_buffer(markdown, false, cx),
                )
            });
            let buffer = buffer.await.ok()?;
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, rp_release.release_notes)], None, cx)
            });

            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));

            let ws_handle = workspace.clone();
            cx.update_window(window_handle, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let editor =
                        cx.new(|cx| Editor::for_multibuffer(buffer, Some(project), window, cx));
                    let markdown_preview: Entity<MarkdownPreviewView> = MarkdownPreviewView::new(
                        MarkdownPreviewMode::Default,
                        editor,
                        ws_handle,
                        language_registry,
                        window,
                        cx,
                    );
                    workspace.add_item_to_active_pane(
                        Box::new(markdown_preview),
                        None,
                        true,
                        window,
                        cx,
                    );
                    cx.notify();
                })
            })
            .ok()?
            .ok()
        })
        .await;

        if res.is_some() {
            let kvp = cx.update(|cx| KeyValueStore::global(cx));
            kvp.write_kvp(
                RP_RELEASE_NOTES_KVP_KEY.to_owned(),
                rp_release.calendar_version.to_owned(),
            )
            .await
            .log_err();
        }

        if res.is_none() {
            if let Ok(Err(error)) = cx.update_window(window_handle, |_, _window, cx| {
                workspace.update(cx, |workspace, cx| {
                    struct RpReleaseNotesError;

                    impl WorkspaceError for RpReleaseNotesError {
                        fn primary_message(&self) -> SharedString {
                            "Couldn't open embedded RP fork release notes".into()
                        }

                        fn severity(&self) -> ErrorSeverity {
                            ErrorSeverity::Error
                        }

                        fn primary_action(&self) -> ErrorAction {
                            ErrorAction::dismiss()
                        }
                    }

                    workspace.show_error(RpReleaseNotesError, cx);
                })
            }) {
                anyhow::Result::<()>::Err(error).log_err();
            }
        }
    })
    .detach();
}

#[derive(Clone)]
struct AnnouncementContent {
    heading: SharedString,
    description: SharedString,
    bullet_items: Vec<SharedString>,
    primary_action_label: SharedString,
    secondary_action_label: SharedString,
    primary_action_url: SharedString,
    secondary_action_url: SharedString,
}

struct DeltaAnnouncement;

impl Dismissable for DeltaAnnouncement {
    const KEY: &'static str = "delta_announcement_dismissed";
}

fn announcement_for_version(version: &Version, cx: &App) -> Option<AnnouncementContent> {
    let version_with_delta = Version::new(1, 22, 0);
    if *version < version_with_delta
        || DisableAiSettings::get_global(cx).disable_ai
        || DeltaAnnouncement::dismissed(cx)
    {
        return None;
    }

    Some(AnnouncementContent {
        heading: "Introducing Delta".into(),
        description:
            "Built on DeltaDB, so your threads and code stay in sync across machines and teammates."
                .into(),
        bullet_items: vec![
            "Made by the Zed team, with the same quality and performance".into(),
            "Work with teammates and agents in the same thread, live or later".into(),
            "Pick up your thread on the web or your phone, without committing or pushing".into(),
        ],
        primary_action_label: "Try Delta".into(),
        secondary_action_label: "Learn More".into(),
        primary_action_url: "https://delta.dev/".into(),
        secondary_action_url: "https://delta.dev/docs/getting-started".into(),
    })
}

struct AnnouncementToastNotification {
    focus_handle: FocusHandle,
    content: AnnouncementContent,
}

impl AnnouncementToastNotification {
    fn new(content: AnnouncementContent, cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content,
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        DeltaAnnouncement::set_dismissed(true, cx);
    }
}

impl Focusable for AnnouncementToastNotification {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for AnnouncementToastNotification {}
impl EventEmitter<SuppressEvent> for AnnouncementToastNotification {}
impl Notification for AnnouncementToastNotification {}

impl Render for AnnouncementToastNotification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toast = AnnouncementToast::new()
            .illustration(DeltaIllustration::new())
            .heading(self.content.heading.clone())
            .description(self.content.description.clone())
            .bullet_items(
                self.content
                    .bullet_items
                    .iter()
                    .map(|item| ListBulletItem::new(item.clone())),
            )
            .primary_action_label(self.content.primary_action_label.clone())
            .secondary_action_label(self.content.secondary_action_label.clone())
            .primary_on_click(cx.listener({
                let url = self.content.primary_action_url.clone();
                move |this, _, _window, cx| {
                    telemetry::event!("Delta Announcement Main Click");
                    cx.open_url(&url);
                    this.dismiss(cx);
                }
            }))
            .secondary_on_click(cx.listener({
                let url = self.content.secondary_action_url.clone();
                move |_, _, _window, cx| {
                    telemetry::event!("Delta Announcement Secondary Click");
                    cx.open_url(&url);
                }
            }))
            .dismiss_on_click(cx.listener(|this, _, _window, cx| {
                telemetry::event!("Delta Announcement Dismiss");
                this.dismiss(cx);
            }));

        div()
            .self_end()
            .flex_none()
            .w(rems_from_px(400_f32))
            .max_w((window.viewport_size().width - window.rem_size() * 1.5).max(px(0.)))
            .child(toast)
    }
}

struct UpdateNotification;

fn show_update_notification(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    let mut version = updater.read(cx).current_version();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let update_identity = release_channel::release_display_identity(
        rp_release_metadata(),
        ReleaseChannel::global(cx),
        &version,
    );

    if let Some(content) = announcement_for_version(&version, cx) {
        show_app_notification(
            NotificationId::unique::<UpdateNotification>(),
            cx,
            move |cx| cx.new(|cx| AnnouncementToastNotification::new(content.clone(), cx)),
        );
    } else {
        show_app_notification(
            NotificationId::unique::<UpdateNotification>(),
            cx,
            move |cx| {
                let workspace_handle = cx.entity().downgrade();
                cx.new(|cx| {
                    MessageNotification::new(format!("Updated to {update_identity}"), cx)
                        .primary_message("View Release Notes")
                        .primary_on_click(move |window, cx| {
                            if let Some(workspace) = workspace_handle.upgrade() {
                                workspace.update(cx, |workspace, cx| {
                                    crate::view_release_notes_locally(workspace, window, cx);
                                })
                            }
                            cx.emit(DismissEvent);
                        })
                        .show_suppress_button(false)
                })
            },
        );
    }
}

/// Shows a notification across all workspaces if an update was previously automatically installed
/// and this notification had not yet been shown.
pub fn notify_if_app_was_updated(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    if let ReleaseChannel::Nightly = ReleaseChannel::global(cx) {
        return;
    }

    let should_show_notification = updater.read(cx).should_show_update_notification(cx);

    cx.spawn(async move |cx| {
        let should_show_notification = should_show_notification.await?;

        if should_show_notification {
            cx.update(|cx| {
                show_update_notification(cx);
                updater.update(cx, |updater, cx| {
                    updater
                        .set_should_show_update_notification(false, cx)
                        .detach_and_log_err(cx);
                });
            });
        }
        anyhow::Ok(())
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const RP_RELEASE: RpReleaseMetadata = RpReleaseMetadata {
        calendar_version: "20260902.1",
        release_tag: "rp-stable-20260902.1",
        upstream_tag: "v1.17.2",
        upstream_tag_commit: "0123456789abcdef0123456789abcdef01234567",
        release_notes: "# RP Fork Release Notes 20260902.1",
        notes_identity: "sha256:notes",
        manifest: "{}",
    };

    #[test]
    fn release_notes_source_is_fork_specific() {
        assert_eq!(
            release_notes_source(Some(RP_RELEASE), ReleaseChannel::Stable),
            ReleaseNotesSource::Rp(RP_RELEASE)
        );
        assert_eq!(
            release_notes_source(None, ReleaseChannel::Stable),
            ReleaseNotesSource::UpstreamLocal
        );
        assert_eq!(
            release_notes_source(None, ReleaseChannel::Preview),
            ReleaseNotesSource::UpstreamLocal
        );
        assert_eq!(
            release_notes_source(None, ReleaseChannel::Nightly),
            ReleaseNotesSource::UpstreamBrowser
        );
        assert_eq!(
            release_notes_source(None, ReleaseChannel::Dev),
            ReleaseNotesSource::UpstreamBrowser
        );
    }

    #[test]
    fn rp_release_notes_open_once_per_exact_calendar_version() {
        assert_eq!(
            rp_release_notes_open_decision("20260902.1", None, None, false),
            RpReleaseNotesOpenDecision::Open
        );
        assert_eq!(
            rp_release_notes_open_decision("20260902.1", Some("20260902.1"), None, false),
            RpReleaseNotesOpenDecision::AlreadyShown
        );
        assert_eq!(
            rp_release_notes_open_decision("20260902.2", Some("20260902.1"), None, false),
            RpReleaseNotesOpenDecision::Open
        );
        assert_eq!(
            rp_release_notes_open_decision("20260902.1", None, Some("20260902.1"), false),
            RpReleaseNotesOpenDecision::AlreadyOpening
        );
        assert_eq!(
            rp_release_notes_open_decision("20260902.1", Some("20260902.1"), None, true),
            RpReleaseNotesOpenDecision::Open
        );
    }

    #[test]
    fn failed_open_releases_reservation_for_manual_recovery() {
        let mut state = RpReleaseNotesOpenState::default();

        assert!(state.reserve("20260902.1", None, false));
        assert!(!state.reserve("20260902.1", None, true));

        state.release("20260902.1");

        assert!(state.reserve("20260902.1", Some("20260902.1"), true));
    }
}
