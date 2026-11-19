//! Spaces sidebar: the space-filter dropdown (searchable, with "All projects"),
//! the filtered Sessions list, and the add-space palette (device
//! tabs + filtered folder browser).
//!
//! A space = a synced (device, folder) pair. Spaces stopped being a
//! navigation spine when tabs went device-local: the dropdown only FILTERS
//! the sidebar's session list (never the tab strip) and hosts space
//! management (add via the palette; rename/delete via row context menus).
//! Child module of `shell` so it renders straight off `Shell`'s private state.

use super::*;
use crate::pickers::{
    breadcrumbs, browser_rows, completion_prefix_len, escape_typed_path, folder_path_key,
    is_windows_path, parent_path, resolve_typed_path, same_folder_path, split_typed_leaf,
    unescape_typed_path,
};
use gpui::{FocusHandle, Window};
use std::collections::HashSet;
use zeron_proto::{
    Device, DriveEntry, DriveListing, FolderListing, HomeFolderMatch,
    HomeFolderSearchResult, SearchHomeFoldersRequest, Space,
};

/// Promote the user's ordered pins above the untouched activity projection.
/// Every unpinned id keeps exactly the relative order supplied by recency.
pub(super) fn project_pinned_first(recency_ids: &[String], pinned_ids: &[String]) -> Vec<String> {
    let active: HashSet<&str> = recency_ids.iter().map(String::as_str).collect();
    let pinned: HashSet<&str> = pinned_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    pinned_ids
        .iter()
        .filter(|id| active.contains(id.as_str()))
        .chain(
            recency_ids
                .iter()
                .filter(|id| !pinned.contains(id.as_str())),
        )
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// Remove only ids absent from the workspace. Archived sessions remain known
/// so unarchiving restores their local pin and position.
pub(super) fn retain_known_pins(
    pinned_ids: &mut Vec<String>,
    known_chat_ids: &HashSet<String>,
) -> bool {
    let before = pinned_ids.len();
    let mut seen = HashSet::new();
    pinned_ids.retain(|id| known_chat_ids.contains(id) && seen.insert(id.clone()));
    pinned_ids.len() != before
}

pub(super) fn pinned_drag_snapshot_is_valid(
    dragged_id: &str,
    snapshot_ids: &[String],
    current_ids: &[String],
) -> bool {
    snapshot_ids.iter().any(|id| id == dragged_id) && snapshot_ids == current_ids
}

pub(super) fn compare_sidebar_chats(
    sort: SidebarSort,
    left: &zeron_proto::Chat,
    right: &zeron_proto::Chat,
) -> std::cmp::Ordering {
    let primary = match sort {
        SidebarSort::Created => right.created_at.cmp(&left.created_at),
        SidebarSort::LastUpdated => right
            .last_message_at
            .unwrap_or(right.created_at)
            .cmp(&left.last_message_at.unwrap_or(left.created_at)),
    };
    primary.then_with(|| left.id.cmp(&right.id))
}

/// The space-filter dropdown, `Some` while open. The same searchable-menu
/// recipe as the composer's ref picker: filter input on top
/// (`PaletteSearch` context so ↑↓/⏎ bubble to the card), ranked substring
/// rows, keyboard highlight.
pub(super) struct SpacesMenu {
    search: Entity<ComposerInput>,
    /// Keyboard highlight — an index into [`Shell::spaces_menu_rows`], or
    /// that list's length when the pinned "New project" footer holds it.
    active: usize,
    /// Tracked on the card — puts it on the keyboard dispatch path while the
    /// search input holds focus (the structure every working picker uses).
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

pub(super) struct SidebarViewMenu {
    submenu: Option<usize>,
    submenu_active: Option<usize>,
    submenu_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    submenu_on_left: bool,
    hover_intent: popover::HoverIntent<usize>,
    /// Keyboard cursor. Mouse-opened menus start without one so the persisted
    /// radio/check state is the only selection signal until an arrow key is
    /// pressed.
    active: Option<usize>,
    focus: FocusHandle,
}

struct SidebarViewOptionsTooltip;

impl Render for SidebarViewOptionsTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .child("Sidebar view options")
    }
}

/// The Archived shelf shows this many rows, then "Show more" pages by
/// [`ARCHIVED_PAGE_ROWS`].
const ARCHIVED_INITIAL_ROWS: usize = 10;
const ARCHIVED_PAGE_ROWS: usize = 25;

/// Put this machine's device group first without disturbing the recency-based
/// order of any remote groups. A targeted promotion is more truthful than a
/// full name sort: local context leads, then the user's chosen chat sort wins.
fn promote_local_device_group<T>(
    groups: &mut Vec<(Option<(String, String)>, Vec<T>)>,
    local_device_id: Option<&str>,
) {
    let Some(local_device_id) = local_device_id else {
        return;
    };
    let Some(index) = groups.iter().position(|(group, _)| {
        group
            .as_ref()
            .is_some_and(|(device_id, _)| device_id == local_device_id)
    }) else {
        return;
    };
    if index > 0 {
        let local = groups.remove(index);
        groups.insert(0, local);
    }
}

/// Project-mode group of a chat as (key, label). Projects sharing a
/// repository identity fold into one group named for their representative
/// space; project-less sessions group per device as "No project".
fn sidebar_project_group(state: &AppState, chat: &zeron_proto::Chat) -> (String, String) {
    match state.space_for_chat(chat) {
        Some(space) => (
            zeron_proto::view::project_key(space),
            state.representative_space(space).display_name().to_string(),
        ),
        None => match chat.space_id.clone() {
            Some(space_id) => (space_id, "?".to_string()),
            None => (format!("home:{}", chat.device_id), "No project".to_string()),
        },
    }
}

/// Shared quiet rule for sidebar groups and palette sections.
pub(super) fn sidebar_separator(theme: &Theme) -> gpui::Div {
    div().h(px(1.0)).bg(theme.border.opacity(0.6))
}

/// New project navigates devices, locations, then folders on a command-palette surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectStep {
    Devices,
    Locations,
    Folders,
    /// Cloud picked: the GitHub repositories Cloud reaches. A Cloud project
    /// is a repository, not a folder on a device.
    Repositories,
}

pub(super) struct AddSpaceFlow {
    step: ProjectStep,
    location: Option<(String, Option<String>)>,
    /// The selected device.
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder. Carries the
    /// tab-completion ghost (the faint suffix ⇥ accepts), and a trailing `/`
    /// on a folder-naming query descends immediately.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// The selected device's mounted drives/volumes.
    /// Best-effort: an error just leaves the section at Home only.
    drives: Loadable<Vec<DriveEntry>>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the Home crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the current step’s filtered rows.
    active: usize,
    submit_busy: bool,
    /// A clone into the open folder is running (`owner/name`).
    cloning: Option<String>,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    /// Horizontal breadcrumb strip; `crumb_key` is the path it last revealed.
    crumb_scroll: gpui::ScrollHandle,
    crumb_key: String,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    drives_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    /// Folders matching the query anywhere under the device's home (Home
    /// location only).
    home_search: HomeFolderSearch,
    /// The crumb row as a typed path; `None` shows the crumbs.
    path_edit: Option<PathEdit>,
    _search_events: Subscription,
}

impl AddSpaceFlow {
    /// The folder the trail shows as open — while one loads, or after it
    /// failed, the path it asked for; home on the Locations step. A typed
    /// path starts from it, relative paths and ⇥ resolve against it.
    fn open_folder(&self) -> Option<String> {
        match self.step {
            ProjectStep::Folders => self
                .browser
                .ready()
                .map(|listing| listing.path.clone())
                .or_else(|| self.browser_path.clone())
                .or_else(|| self.location.as_ref().and_then(|(_, path)| path.clone()))
                .or_else(|| self.home.clone()),
            _ => self.home.clone(),
        }
    }
}

/// The crumb trail as an editable path, address-bar style: ⌘L, the pen at
/// the row's end, or a click beside the crumbs. The open folder stays
/// listed until the typed path answers, so a typo never loses it.
struct PathEdit {
    input: Entity<ComposerInput>,
    /// ListFolders for the typed path is in flight.
    opening: bool,
    /// ⌘↵ asked for the open in flight to add the project too; a later ↵
    /// doesn't take that back.
    adding: bool,
    /// Why the typed path didn't open; the next edit clears it.
    error: Option<SharedString>,
    /// ⇥'s source: the folders of the typed path's parent, keyed by that
    /// parent's `folder_path_key`.
    siblings: Option<(String, Loadable<Vec<zeron_proto::FolderEntry>>)>,
    focus_pending: bool,
    open_task: Option<Task<()>>,
    siblings_task: Option<Task<()>>,
    /// Resolves the device's home for `~` when nothing has listed it yet.
    _home_task: Option<Task<()>>,
    _events: Subscription,
}

/// ListFolders on `device_id`, relayed unless it is `local`; no `path`
/// lists the device's home.
async fn list_folders_on(
    engine: crate::state::EngineHandle,
    path: Option<String>,
    device_id: Option<String>,
    local: Option<String>,
) -> Result<FolderListing, String> {
    let mut params = serde_json::Map::new();
    if let Some(path) = path {
        params.insert("path".into(), serde_json::Value::String(path));
    }
    add_space_target(&mut params, device_id.as_deref(), local.as_deref());
    let value = engine
        .client()
        .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// A typed path that didn't list, in the path field's words: a missing
/// folder (POSIX 2, Windows 2/3) or a path through a file (POSIX 20,
/// Windows 267 — the file may be the path or any folder on it) as the
/// device's io error reports it; anything else as the device said it.
fn path_open_error(path: &str, error: &str) -> String {
    let reported = |codes: &[&str]| codes.iter().any(|code| error.contains(code));
    if reported(&["(os error 2)", "(os error 3)"]) {
        format!("No folder at {path}")
    } else if reported(&["(os error 20)", "(os error 267)"]) {
        format!("{path} isn't a folder")
    } else {
        error.to_owned()
    }
}

/// The location a typed path opens under: Home when it is in home, else
/// the deepest drive holding it, else its filesystem root — so the trail
/// reads `Home › code`, not every folder down from `/`.
fn typed_path_location(
    path: &str,
    home: Option<&str>,
    drives: &[DriveEntry],
) -> (String, Option<String>) {
    if home.is_some_and(|home| path_under(path, home)) {
        return ("Home".to_string(), None);
    }
    if let Some(drive) = drives
        .iter()
        .filter(|drive| path_under(path, &drive.path))
        .max_by_key(|drive| folder_path_key(&drive.path).len())
    {
        return (drive.name.clone(), Some(drive.path.clone()));
    }
    let root = breadcrumbs(path)
        .into_iter()
        .next()
        .map_or_else(|| "/".to_string(), |(_, root)| root);
    (root.clone(), Some(root))
}

/// Shorter queries match nearly every folder under home; they are not sent.
const HOME_FOLDER_MIN_QUERY_CHARS: usize = 2;
const HOME_FOLDER_RESULTS: u16 = 20;
const HOME_FOLDER_INDEX_RETRY: Duration = Duration::from_millis(500);
const HOME_FOLDER_INDEX_RETRY_MAX: Duration = Duration::from_secs(2);

/// The new-project picker's search over the device's home, answered by the
/// host's home search index (the one projectless chats use). Entering Home
/// starts the index; leaving Home or closing the picker releases it unless a
/// projectless chat warmed it.
#[derive(Default)]
struct HomeFolderSearch {
    /// Bumped per request; an answer to an older one is dropped.
    generation: u64,
    loading: bool,
    /// The host's index was still scanning at the last answer.
    indexing: bool,
    results: Vec<HomeFolderMatch>,
    /// A request reached the host, so it may hold an index to release.
    started: bool,
    /// The host predates `SearchHomeFolders`; the section stays hidden.
    unsupported: bool,
    task: Option<Task<()>>,
    /// The request that started the host's index.
    warm_task: Option<Task<()>>,
}

/// Refresh `SearchHomeFolders` until indexing finishes, backing off between
/// replies. `generation` identifies the query; `None` is a one-shot warm-up
/// whose answer only tells whether the host supports the method.
fn request_home_folders(
    engine: crate::state::EngineHandle,
    device_id: Option<String>,
    local: Option<String>,
    query: String,
    generation: Option<u64>,
    cx: &mut Context<Shell>,
) -> Task<()> {
    let request = SearchHomeFoldersRequest {
        query,
        limit: Some(HOME_FOLDER_RESULTS),
    };
    cx.spawn(async move |this, cx| {
        let mut params = match serde_json::to_value(&request) {
            Ok(serde_json::Value::Object(params)) => params,
            _ => return,
        };
        add_space_target(&mut params, device_id.as_deref(), local.as_deref());
        let params = serde_json::Value::Object(params);
        let mut delay = HOME_FOLDER_INDEX_RETRY;
        loop {
            let result = engine
                .client()
                .call(methods::SEARCH_HOME_FOLDERS, params.clone())
                .await;
            let retry = this
                .update(cx, |shell, cx| {
                    let selected = shell
                        .add_space
                        .as_ref()
                        .and_then(|flow| shell.add_space_folder_rows(cx).get(flow.active).cloned());
                    let Some(search) = shell.add_space.as_mut().map(|flow| &mut flow.home_search)
                    else {
                        return false;
                    };
                    if generation.is_some() && generation != Some(search.generation) {
                        return false;
                    }
                    if let Err(zeron_rpc::RpcError::UnknownMethod(_)) = &result {
                        search.unsupported = true;
                        search.started = false;
                        search.loading = false;
                        search.indexing = false;
                        cx.notify();
                        return false;
                    }
                    if generation.is_none() || search.unsupported {
                        return false;
                    }
                    search.loading = false;
                    search.indexing = false;
                    match result.map(serde_json::from_value::<HomeFolderSearchResult>) {
                        Ok(Ok(found)) => {
                            search.indexing = found.indexing;
                            search.results = found.matches;
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%error, "home folder search answer did not decode");
                        }
                        Err(error) => tracing::debug!(%error, "home folder search failed"),
                    }
                    let retry = search.indexing;
                    // New partial results may reorder the rows under the cursor.
                    if let Some(selected) = selected
                        && let Some(active) = shell
                            .add_space_folder_rows(cx)
                            .iter()
                            .position(|row| row == &selected)
                        && let Some(flow) = shell.add_space.as_mut()
                    {
                        flow.active = active;
                    }
                    cx.notify();
                    retry
                })
                .unwrap_or(false);
            if !retry {
                break;
            }
            // The stored task is dropped on edits, navigation or close. Wait
            // after the reply so slow hosts never accumulate parallel calls.
            cx.background_executor().timer(delay).await;
            delay = (delay * 2).min(HOME_FOLDER_INDEX_RETRY_MAX);
            let current = this
                .update(cx, |shell, _| {
                    shell.add_space_in_home()
                        && shell.add_space.as_ref().is_some_and(|flow| {
                            generation == Some(flow.home_search.generation)
                                && !flow.home_search.unsupported
                        })
                })
                .unwrap_or(false);
            if !current {
                break;
            }
        }
    })
}

/// One row of the Folders step, in navigation order.
#[derive(Clone, Debug, PartialEq)]
enum FolderRow {
    /// A subfolder of the open folder, matched by name.
    Local(zeron_proto::FolderEntry),
    /// A folder anywhere under home.
    Home(HomeFolderMatch),
    /// Clone a repository into the open folder: a pasted git URL, or one of
    /// the user's GitHub repositories matching the query.
    Clone(CloneRow),
}

#[derive(Clone, Debug, PartialEq)]
struct CloneRow {
    url: String,
    /// `owner/name`.
    label: String,
    /// From the user's GitHub repositories (vs a pasted URL).
    github: bool,
    private: bool,
}

/// Most GitHub repositories offered for one query.
const CLONE_MATCHES: usize = 5;

/// A query that is a git remote: an `https://`/`ssh://`/`git://` URL with an
/// owner and a name, or scp-style `git@host:owner/name`.
fn git_url(query: &str) -> Option<String> {
    let query = query.trim();
    if query.contains(char::is_whitespace) {
        return None;
    }
    let path = if let Some(rest) = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|scheme| query.strip_prefix(scheme))
    {
        rest.split_once('/')?.1
    } else if let Some((user_host, path)) = query.split_once(':')
        && user_host.contains('@')
        && !user_host.contains('/')
    {
        path
    } else {
        return None;
    };
    let mut parts = path.trim_matches('/').split('/').filter(|part| !part.is_empty());
    (parts.next().is_some() && parts.next().is_some()).then(|| query.to_owned())
}

/// `owner/name` of a git remote, for labels.
fn git_url_label(url: &str) -> String {
    let path = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit_once(['/', ':'])
        .map(|(rest, name)| {
            let owner = rest.rsplit(['/', ':']).next().unwrap_or_default();
            if owner.is_empty() {
                name.to_owned()
            } else {
                format!("{owner}/{name}")
            }
        });
    path.unwrap_or_else(|| url.to_owned())
}

/// `targetDeviceId` for a remote device; local calls skip the relay.
fn add_space_target(
    params: &mut serde_json::Map<String, serde_json::Value>,
    device_id: Option<&str>,
    local: Option<&str>,
) {
    if let Some(target) = device_id
        && local != Some(target)
    {
        params.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.to_owned()),
        );
    }
}

/// The title over the picker's matches from under home, with the host's
/// search status trailing.
fn home_folders_header(theme: &Theme, status: Option<&'static str>) -> gpui::Div {
    home_folders_header_titled(theme, "In Home").when_some(status, |header, status| {
        header.child(div().opacity(0.7).child(status))
    })
}

/// A titled section of the Folders step's results ("In Home", "From GitHub").
fn home_folders_header_titled(theme: &Theme, title: &'static str) -> gpui::Div {
    div()
        .px(px(8.0))
        .pt(px(2.0))
        .pb(px(4.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(theme.text_muted)
        .child(title)
}

/// A folder from under home: its name (query highlighted), then where it
/// lives (`~/…`), marked when it is a repository.
fn home_folder_row(
    row: gpui::Stateful<gpui::Div>,
    found: &HomeFolderMatch,
    query: &str,
    theme: &Theme,
    cx: &Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let (name, parent) = match found.relative.rsplit_once('/') {
        Some((parent, name)) => (name, format!("~/{parent}")),
        None => (found.relative.as_str(), "~".to_owned()),
    };
    let (path, is_repo) = (found.path.clone(), found.is_repo);
    row.aria_label(SharedString::from(format!("{name} in {parent}")))
        .child(
            icon(icons::FOLDER)
                .size(px(16.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .child(div().flex_none().child(popover::search_highlight(
                    SharedString::from(name.to_owned()),
                    Some(query),
                    theme,
                )))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(parent)),
                ),
        )
        .when(is_repo, |el| {
            el.child(
                icon(icons::GIT_BRANCH)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
        })
        .on_click(
            cx.listener(move |this, _, _, cx| this.add_space_descend(path.clone(), is_repo, cx)),
        )
}

/// A clone-here row: the owner's avatar (a pasted URL gets the git glyph),
/// `owner/name`, a Private badge, and what Enter does.
fn clone_row(
    row: gpui::Stateful<gpui::Div>,
    clone: &CloneRow,
    query: &str,
    theme: &Theme,
    cx: &Context<Shell>,
) -> gpui::Stateful<gpui::Div> {
    let owner = clone.label.split('/').next().unwrap_or_default().to_owned();
    // GitHub avatars went with the pull-request views: every row takes the
    // branch glyph.
    let _ = &owner;
    let leading = {
        icon(icons::GIT_BRANCH)
            .size(px(16.0))
            .flex_none()
            .text_color(theme.text_muted)
            .into_any_element()
    };
    let target = clone.clone();
    row.aria_label(SharedString::from(format!("Clone {} here", clone.label)))
        .child(leading)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(popover::search_highlight(
                    SharedString::from(clone.label.clone()),
                    Some(query),
                    theme,
                )),
        )
        .when(clone.private, |el| {
            el.child(crate::settings::widgets::badge(theme, "Private"))
        })
        .child(
            div()
                .flex_none()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("Clone here")),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.add_space_clone(target.clone(), cx)))
}

/// Folder crumbs shown before the middle folds into `…`, and how many of the
/// deepest stay visible once it does.
const CRUMB_FOLDERS_MAX: usize = 3;
const CRUMB_FOLDERS_TAIL: usize = 2;

/// Fold the middle of a deep trail: returns the folders hidden behind `…`,
/// leaving the deepest [`CRUMB_FOLDERS_TAIL`] in `folders`.
fn fold_crumb_folders<T>(folders: &mut Vec<T>) -> Vec<T> {
    if folders.len() <= CRUMB_FOLDERS_MAX {
        return Vec::new();
    }
    folders
        .drain(..folders.len() - CRUMB_FOLDERS_TAIL)
        .collect()
}

struct Crumb {
    name: SharedString,
    glyph: Option<&'static str>,
    current: bool,
    target: CrumbTarget,
}

enum CrumbTarget {
    Devices,
    Locations,
    Location(String, Option<String>),
    Folder(String),
    /// The `…` fold; opens [`Shell::render_project_crumb_menu`].
    More,
}

/// Cloud as a place to keep a project: while it's on and reaches at least
/// one GitHub repository. It has no registry row of its own (session
/// machines do), so the row stands in for the logical Cloud device.
fn add_space_cloud_device(state: &crate::state::AppState) -> Option<Device> {
    if !state.cloud_enabled()
        || state
            .cloud_repos
            .as_ref()
            .is_none_or(|repos| repos.is_empty())
    {
        return None;
    }
    let id = state.cloud_status.as_ref()?.device_id.clone()?;
    Some(Device {
        id,
        name: "Cloud".into(),
        platform: zeron_proto::CLOUD_PLATFORM.into(),
        last_seen_at: None,
        created_at: None,
        version: None,
        cursor_sdk_version: None,
        capabilities: Vec::new(),
    })
}

fn device_glyph(platform: &str) -> &'static str {
    match platform {
        zeron_proto::CLOUD_PLATFORM => icons::CLOUD,
        "macos" | "darwin" => icons::LAPTOP,
        "web" => icons::GLOBAL,
        "ios" | "android" => icons::SMARTPHONE,
        _ => icons::MONITOR,
    }
}

/// Segment-aware "is `path` at or under `base`" (`/media/a` is not under
/// `/media/ab`); a root base covers everything. Either separator counts, so
/// Windows drive paths (`D:\` under `D:\`) work too.
fn path_under(path: &str, base: &str) -> bool {
    folder_path_key(base)
        .trim_end_matches(['/', '\\'])
        .is_empty()
        || path_rest(path, base).is_some()
}

/// What follows `base` in `path` when [`path_under`] holds, in `path`'s
/// lexical key: `""` at `base`, `/code` (`\code`) below it.
fn path_rest(path: &str, base: &str) -> Option<String> {
    let path = folder_path_key(path);
    let base = folder_path_key(base);
    let rest = path.strip_prefix(base.trim_end_matches(['/', '\\']))?;
    (rest.is_empty() || rest.starts_with(['/', '\\'])).then(|| rest.to_owned())
}

/// `path` as the path field spells it: from `~` when it is in `home`
/// (`~/code`, `~\code` on Windows), as it is anywhere else. A home at a
/// filesystem root stays spelled out.
fn tilde_path(path: &str, home: Option<&str>) -> String {
    home.filter(|home| parent_path(home).is_some())
        .and_then(|home| path_rest(path, home))
        .map_or_else(|| path.to_owned(), |rest| format!("~{rest}"))
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

// Handle-based rail host for the spaces dropdown: its list is a plain
// tracked scroller, so the trait's default metrics/press/drag (off the live
// ScrollHandle) apply unchanged.
impl popover::ScrollRailHost for Shell {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        &mut self.spaces_menu_bar
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.spaces_menu.get().map(|menu| menu.list_scroll.clone())
    }
}

impl Shell {
    // ---- space filter ----

    /// Close the space-filter dropdown through the exit animation (no-op when
    /// it isn't open). Every close path funnels here so the menu always
    /// animates out instead of vanishing.
    pub(super) fn close_spaces_menu(&mut self, cx: &mut Context<Self>) {
        if self.spaces_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.spaces_menu);
            cx.notify();
        }
    }

    /// Open the new-session canvas in a just-added space, preserving the
    /// sidebar's current project filter.
    pub(super) fn land_in_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.cancel_pinned_session_drag(cx);
        self.set_route(Route::Chat, cx);
        self.focus_composer(cx);
        // "All" stays as-is; an explicit project filter follows the new
        // project so the first send lands in a visible session.
        if self.settings.space_filter.is_some() {
            self.settings.space_filter = Some(space_id.clone());
        }
        self.nav.push(NavEntry::Chat(String::new()));
        self.settings.last_space_id = Some(space_id.clone());
        self.state.update(cx, |s, cx| {
            // Leave the chat first: that restores the canvas pick set aside
            // when it opened, which must not win over the project just added.
            s.select_chat(None, cx);
            s.select_space(Some(space_id), cx);
        });
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- sidebar sections ----

    pub(super) fn sidebar_session_transfer_is_valid(
        &self,
        payload: &SidebarSessionDrag,
        cx: &App,
    ) -> bool {
        if payload.filter != self.settings.space_filter
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&payload.profile_key)
        {
            return false;
        }
        let state = self.state.read(cx);
        let visible: HashSet<String> = state
            .sidebar_chats(Utc::now(), payload.filter.as_deref())
            .into_iter()
            .map(|(_, chat)| chat.id.clone())
            .collect();
        if !visible.contains(&payload.chat_id) {
            return false;
        }
        let pins = self.sidebar_pins_for_profile(&payload.profile_key, cx);
        let current: Vec<_> = pins.into_iter().filter(|id| visible.contains(id)).collect();
        current == *payload.visible_ids
    }

    pub(super) fn cancel_sidebar_session_transfer(&mut self, cx: &mut Context<Self>) {
        self.chat_status_hover = None;
        self.cancel_pinned_session_drag(cx);
        if let Some(mut transfer) = self.sidebar_session_transfer.take() {
            transfer.preview = None;
            if !self.reduced_motion && self.sidebar_session_transfer_is_valid(&transfer.payload, cx)
            {
                self.sidebar_session_return = Some(SidebarSessionReturn {
                    transfer,
                    epoch: self.pinned_session_drag_generation,
                    started: std::time::Instant::now(),
                });
                let epoch = self.pinned_session_drag_generation;
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(
                                SIDEBAR_DRAG_SCROLL_FRAME_MS,
                            ))
                            .await;
                        let keep_running = this
                            .update(cx, |shell, cx| {
                                let Some(returning) = shell.sidebar_session_return.as_ref() else {
                                    return false;
                                };
                                if returning.epoch != epoch {
                                    return false;
                                }
                                if returning.started.elapsed() >= TAB_SLIDE.total()
                                    && returning.transfer.slide.started.elapsed()
                                        >= TAB_SLIDE.total()
                                    && returning.transfer.source_collapse.started.elapsed()
                                        >= TAB_SLIDE.total()
                                    && returning
                                        .transfer
                                        .section_gaps
                                        .values()
                                        .all(|gap| gap.started.elapsed() >= TAB_SLIDE.total())
                                {
                                    shell.sidebar_session_return = None;
                                    cx.notify();
                                    return false;
                                }
                                cx.notify();
                                true
                            })
                            .unwrap_or(false);
                        if !keep_running {
                            break;
                        }
                    }
                })
                .detach();
                self.pinned_session_drag_generation =
                    self.pinned_session_drag_generation.wrapping_add(1);
            }
            cx.notify();
        }
    }

    pub(super) fn update_pinned_session_drag(
        &mut self,
        payload: &SidebarSessionDrag,
        over: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.pinned_open
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&payload.profile_key)
        {
            self.cancel_pinned_session_drag(cx);
            return;
        }
        let Some(from) = payload
            .visible_ids
            .iter()
            .position(|id| id == &payload.chat_id)
        else {
            self.cancel_pinned_session_drag(cx);
            return;
        };
        if over >= payload.visible_ids.len() {
            self.cancel_pinned_session_drag(cx);
            return;
        }
        match &mut self.pinned_session_drag {
            Some(drag)
                if drag.chat_id == payload.chat_id
                    && drag.filter == payload.filter
                    && drag.profile_key == payload.profile_key
                    && drag.visible_ids.as_ref() == payload.visible_ids.as_ref()
                    && drag.over != over =>
            {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch = drag.epoch.wrapping_add(1);
                cx.notify();
            }
            Some(drag)
                if drag.chat_id == payload.chat_id
                    && drag.filter == payload.filter
                    && drag.profile_key == payload.profile_key
                    && drag.visible_ids.as_ref() == payload.visible_ids.as_ref() => {}
            _ => {
                self.pinned_session_drag_generation =
                    self.pinned_session_drag_generation.wrapping_add(1);
                self.pinned_session_drag = Some(PinnedSessionDragState {
                    chat_id: payload.chat_id.clone(),
                    visible_ids: payload.visible_ids.clone(),
                    from,
                    over,
                    prev_over: from,
                    epoch: 0,
                    filter: payload.filter.clone(),
                    profile_key: payload.profile_key.clone(),
                    pointer_y: None,
                    viewport_top: 0.0,
                    viewport_bottom: 0.0,
                    generation: self.pinned_session_drag_generation,
                    autoscroll_active: false,
                });
                cx.notify();
            }
        }
    }

    pub(super) fn pinned_session_drag_is_valid(&self, cx: &App) -> bool {
        let Some(drag) = self.pinned_session_drag.as_ref() else {
            return true;
        };
        if !self.pinned_open
            || drag.filter != self.settings.space_filter
            || self.active_sidebar_pin_profile_key(cx).as_deref() != Some(&drag.profile_key)
        {
            return false;
        }
        let current_pins = self.sidebar_pins_for_profile(&drag.profile_key, cx);
        let state = self.state.read(cx);
        let in_filter = state.project_filter(drag.filter.as_deref());
        let visible_ids: HashSet<String> = state
            .overview_chats(Utc::now())
            .into_iter()
            .filter(|(_, chat)| in_filter(chat))
            .map(|(_, chat)| chat.id.clone())
            .collect();
        let current_ids = current_pins
            .iter()
            .filter(|id| visible_ids.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        pinned_drag_snapshot_is_valid(&drag.chat_id, drag.visible_ids.as_ref(), &current_ids)
    }

    pub(super) fn cancel_pinned_session_drag(&mut self, cx: &mut Context<Self>) {
        if self.pinned_session_drag.take().is_some() {
            self.pinned_session_drag_generation =
                self.pinned_session_drag_generation.wrapping_add(1);
            cx.notify();
        }
    }

    pub(super) fn close_sidebar_view_menu(&mut self, cx: &mut Context<Self>) {
        self.clean_chat_armed = false;
        if let Some(menu) = self.sidebar_view_menu.open_mut() {
            menu.hover_intent.reset();
        }
        if self.sidebar_view_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.sidebar_view_menu);
            cx.notify();
        }
    }

    pub(super) fn open_sidebar_view_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_spaces_menu(cx);
        let focus = cx.focus_handle();
        self.sidebar_view_menu.open(SidebarViewMenu {
            submenu: None,
            submenu_active: None,
            submenu_bounds: None,
            submenu_on_left: false,
            hover_intent: popover::HoverIntent::default(),
            active: None,
            focus: focus.clone(),
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn sidebar_view_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(menu) = self.sidebar_view_menu.open_mut() else {
            return;
        };
        menu.hover_intent.cancel();
        let key = match popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        ) {
            popover::MenuKey::Up => "up",
            popover::MenuKey::Down => "down",
            _ => event.keystroke.key.as_str(),
        };
        match key {
            "escape" => self.close_sidebar_view_menu(cx),
            "up" | "down" => {
                let delta = if key == "up" { -1 } else { 1 };
                menu.active = popover::menu_step(menu.active, 2, delta);
                cx.notify();
            }
            "enter" | "space" if !event.is_held => match menu.active.unwrap_or(0) {
                0 => self.activate_clean_chat(cx),
                _ => {
                    self.close_sidebar_view_menu(cx);
                    let section = self.settings.settings_section.reopenable();
                    self.open_settings(section, cx);
                }
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The drawer's "..." menu in Aurea: Clean Chat (asks once more before
    /// it wipes the history) and Settings.
    fn render_sidebar_view_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let theme = &theme.for_popup();
        let Some(menu_state) = self.sidebar_view_menu.get() else {
            return div().into_any_element();
        };
        let active = menu_state.active;
        let focus = menu_state.focus.clone();
        let armed = self.clean_chat_armed;
        // Plain menu rows at their natural height (user request: smaller,
        // more natural boxes).
        let row = |ix: usize, id: &'static str| {
            popover::menu_row_nav(theme, false, active == Some(ix), id).id(id)
        };
        let clean_label = if armed { "Click again to confirm" } else { "Clean Chat" };
        let clean = row(0, "sidebar-clean-chat")
            // The row already owns `on_hover` (its hover blend): follow the
            // pointer with mouse moves instead.
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(menu) = this.sidebar_view_menu.open_mut()
                    && menu.active != Some(0)
                {
                    menu.active = Some(0);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.activate_clean_chat(cx);
                cx.stop_propagation();
            }))
            .child(
                icon(icons::TRASH_BIN_MINIMALISTIC)
                    .size(px(14.0))
                    .text_color(if armed { theme.danger } else { theme.text_muted }),
            )
            .child(
                div()
                    .flex_1()
                    .when(armed, |el| el.text_color(theme.danger))
                    .child(clean_label),
            );
        let open_settings = row(1, "sidebar-open-settings")
            // The row already owns `on_hover` (its hover blend): follow the
            // pointer with mouse moves instead.
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(menu) = this.sidebar_view_menu.open_mut()
                    && menu.active != Some(1)
                {
                    menu.active = Some(1);
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_sidebar_view_menu(cx);
                let section = this.settings.settings_section.reopenable();
                this.open_settings(section, cx);
                cx.stop_propagation();
            }))
            .child(
                icon(icons::SETTINGS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(div().flex_1().child("Settings"));
        // Sized to its two rows, not to the drawer.
        popover::popover_card(theme)
            .w(px(184.0))
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.sidebar_view_menu_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.close_sidebar_view_menu(cx);
            }))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(clean)
            .child(open_settings)
            .into_any_element()
    }

    /// Clean Chat: the first activation arms it, the second deletes every
    /// chat (archived and side chats too) and leaves an empty one to start
    /// from zero.
    fn activate_clean_chat(&mut self, cx: &mut Context<Self>) {
        if !self.clean_chat_armed {
            self.clean_chat_armed = true;
            cx.notify();
            return;
        }
        self.clean_chat_armed = false;
        self.close_sidebar_view_menu(cx);
        let chats: Vec<String> = self
            .state
            .read(cx)
            .chats
            .iter()
            .map(|chat| chat.id.clone())
            .collect();
        for chat in chats {
            self.delete_chat(chat, cx);
        }
        self.state.update(cx, |s, cx| s.select_chat(None, cx));
        cx.notify();
    }

    /// The sidebar's view options ("…"): a titlebar control at the sidebar's
    /// trailing edge, opening the Organize / Sort / Show menu.
    pub(super) fn render_sidebar_view_trigger(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view_open = self.sidebar_view_menu.is_open();
        let view_focus = self.sidebar_view_trigger_focus.clone();
        let view_trigger = div()
            .id("sidebar-view-options")
            .role(gpui::Role::Button)
            .aria_label("Sidebar view options")
            .aria_expanded(view_open)
            .track_focus(&view_focus)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border.opacity(0.0))
            .focus_visible(|el| el.border_color(theme.border_strong))
            .cursor_pointer()
            .text_color(theme.text_muted)
            .bg(if view_open {
                theme.glass_hover()
            } else {
                theme.glass_hover().opacity(0.0)
            })
            .hover(|el| el.bg(theme.glass_hover()))
            // Over the titlebar's drag strip: block it, as the other titlebar
            // controls do, so a click never starts a window drag.
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.sidebar_view_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                if this.sidebar_view_menu.take_press_was_open() {
                    this.close_sidebar_view_menu(cx);
                } else {
                    this.open_sidebar_view_menu(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(
                    event.keystroke.key.to_ascii_lowercase().as_str(),
                    "enter" | "space" | "arrowdown"
                ) {
                    cx.stop_propagation();
                    if this.sidebar_view_menu.is_open()
                        && !event.keystroke.key.eq_ignore_ascii_case("arrowdown")
                    {
                        this.close_sidebar_view_menu(cx);
                    } else if !this.sidebar_view_menu.is_open() {
                        this.open_sidebar_view_menu(window, cx);
                    }
                }
            }))
            .tooltip(|_, cx| cx.new(|_| SidebarViewOptionsTooltip).into())
            .tooltip_show_delay(std::time::Duration::from_millis(350))
            .child(
                icon(icons::MORE_HORIZONTAL)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );
        let view_trigger = if self.sidebar_view_menu.get().is_some() {
            let closing = self.sidebar_view_menu.closing_since();
            let menu = self.render_sidebar_view_menu(theme, cx);
            view_trigger.relative().child(popover::anchored_menu_right(
                "sidebar-view-options-menu",
                menu,
                closing,
            ))
        } else {
            view_trigger
        };
        view_trigger.into_any_element()
    }

    /// Flat top-to-bottom chat ids exactly as [`Self::render_active_rows`]
    /// draws them: pins first, then the user's sort, device grouping,
    /// and local-device promotion. Jump shortcuts and session cycling read
    /// this projection so keyboard order never drifts from the screen.
    pub(super) fn sidebar_visible_order(&self, cx: &Context<Self>) -> Vec<String> {
        let filter = self.settings.space_filter.clone();
        let profile_key = self.active_sidebar_pin_profile_key(cx);
        let saved_pins = self.active_sidebar_pins(cx);
        let frozen_pinned = self
            .pinned_session_drag
            .as_ref()
            .filter(|drag| {
                drag.filter == filter && profile_key.as_deref() == Some(&drag.profile_key)
            })
            .map(|drag| drag.visible_ids.clone());
        let pinned_order = frozen_pinned
            .as_ref()
            .map_or(saved_pins.as_slice(), |ids| ids.as_slice());
        let state = self.state.read(cx);
        let mut chats: Vec<zeron_proto::Chat> = state
            .sidebar_chats(Utc::now(), filter.as_deref())
            .into_iter()
            .map(|(_, chat)| chat.clone())
            .collect();
        chats.sort_by(|left, right| compare_sidebar_chats(self.settings.sidebar_sort, left, right));
        let (pinned_chats, chats): (Vec<_>, Vec<_>) = chats
            .into_iter()
            .partition(|chat| pinned_order.contains(&chat.id));
        let custom_sections = self.active_sidebar_sections(cx);
        let mut custom_order = Vec::new();
        let listed: HashSet<&str> = chats.iter().map(|chat| chat.id.as_str()).collect();
        for section in &custom_sections {
            if !section.collapsed {
                custom_order.extend(
                    section
                        .session_ids
                        .iter()
                        .filter(|id| listed.contains(id.as_str()))
                        .cloned(),
                );
            }
        }
        let chats: Vec<_> = chats
            .into_iter()
            .filter(|chat| {
                !custom_sections
                    .iter()
                    .any(|section| section.session_ids.contains(&chat.id))
            })
            .collect();
        let ordered = if self.settings.sidebar_organization != SidebarOrganization::InOneList {
            let mut groups: Vec<(Option<(String, String)>, Vec<zeron_proto::Chat>)> = Vec::new();
            for chat in chats {
                let key = Some((
                    if self.settings.sidebar_organization == SidebarOrganization::ByProject {
                        sidebar_project_group(state, &chat).0
                    } else {
                        chat.device_id.clone()
                    },
                    String::new(),
                ));
                if let Some((_, existing)) = groups.iter_mut().find(|(group, _)| group == &key) {
                    existing.push(chat);
                } else {
                    groups.push((key, vec![chat]));
                }
            }
            if self.settings.sidebar_organization == SidebarOrganization::ByDevice {
                promote_local_device_group(&mut groups, state.local_device_id.as_deref());
            }
            groups
                .into_iter()
                .flat_map(|(_, rows)| rows)
                .map(|chat| chat.id)
                .collect::<Vec<_>>()
        } else {
            chats.into_iter().map(|chat| chat.id).collect()
        };
        let ordered = pinned_chats
            .into_iter()
            .map(|chat| chat.id)
            .chain(custom_order.iter().cloned())
            .chain(ordered)
            .collect::<Vec<_>>();
        let mut visible = project_pinned_first(&ordered, pinned_order);
        if !self.sessions_open
            && self.settings.sidebar_organization == SidebarOrganization::InOneList
        {
            visible.retain(|id| pinned_order.contains(id) || custom_order.contains(id));
        }
        if !self.pinned_open {
            let pins: HashSet<&str> = pinned_order.iter().map(String::as_str).collect();
            visible.retain(|id| !pins.contains(id.as_str()));
        }
        visible
    }

    /// The group a chat's row sits under when the sidebar groups by device or
    /// by project; `None` in one list.
    fn sidebar_group_key(&self, state: &AppState, chat: &zeron_proto::Chat) -> Option<String> {
        match self.settings.sidebar_organization {
            SidebarOrganization::ByDevice => Some(chat.device_id.clone()),
            SidebarOrganization::ByProject => Some(sidebar_project_group(state, chat).0),
            SidebarOrganization::InOneList => None,
        }
    }

    /// A device/project group's key in `sidebar_collapsed_groups`.
    fn sidebar_group_collapse_key(&self, group: &str) -> String {
        let organization = match self.settings.sidebar_organization {
            SidebarOrganization::ByDevice => "device",
            SidebarOrganization::ByProject => "project",
            SidebarOrganization::InOneList => "list",
        };
        format!("{organization}:{group}")
    }

    /// Open whatever hides `chat_id`'s sidebar row — the sidebar itself, the
    /// Pinned / Sessions / Archived disclosure, its custom section or group,
    /// the Archived page — so an inline rename has a row to draw on. Each
    /// disclosure opens with its usual motion. False when the row cannot be
    /// shown: the project filter hides it (the filter is the user's to
    /// clear), or a synced section could not be expanded. Either way the
    /// sidebar notice says why.
    pub(super) fn reveal_sidebar_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        let (active, archived) = {
            let state = self.state.read(cx);
            let active = state
                .sidebar_chats(Utc::now(), self.settings.space_filter.as_deref())
                .into_iter()
                .find(|(_, chat)| chat.id == chat_id)
                .map(|(_, chat)| chat.clone());
            let archived = self
                .archived_sidebar_chats(cx)
                .iter()
                .position(|chat| chat.id == chat_id);
            (active, archived)
        };
        if active.is_none() && archived.is_none() {
            self.show_notice(crate::toast::ToastKind::Error, "Clear the project filter to rename this session", cx);
            return false;
        }
        if let Some(chat) = &active {
            if self.active_sidebar_pins(cx).contains(&chat.id) {
                if !self.pinned_open {
                    self.pinned_open = true;
                    self.queue_sidebar_reveal("pinned");
                }
            } else if let Some(section) = self
                .active_sidebar_sections(cx)
                .into_iter()
                .find(|section| section.session_ids.contains(&chat.id))
            {
                if section.collapsed {
                    if !self.change_sidebar_section(
                        zeron_proto::SidebarSectionChange::Collapse {
                            id: section.id.clone(),
                            collapsed: false,
                        },
                        cx,
                    ) {
                        return false;
                    }
                    self.queue_sidebar_reveal(&format!("custom:{}", section.id));
                }
            } else if let Some(group) = self.sidebar_group_key(self.state.read(cx), chat) {
                let collapse_key = self.sidebar_group_collapse_key(&group);
                if self.sidebar_collapsed_groups.remove(&collapse_key) {
                    self.queue_sidebar_reveal(&format!("group:{collapse_key}"));
                }
            } else if !self.sessions_open {
                self.sessions_open = true;
                self.queue_sidebar_reveal("sessions");
            }
        } else if let Some(index) = archived {
            if !self.settings.sidebar_show_archived {
                self.settings.sidebar_show_archived = true;
                self.schedule_save(cx);
            }
            if !self.archived_open {
                self.archived_open = true;
                self.archived_shown = ARCHIVED_INITIAL_ROWS;
                self.queue_sidebar_reveal("archived");
            }
            // Page the shelf just far enough to include the row, in the
            // same steps "Show more" takes.
            let shown = self.archived_shown.max(ARCHIVED_INITIAL_ROWS);
            if index >= shown {
                self.archived_shown = ARCHIVED_INITIAL_ROWS
                    + (index + 1 - ARCHIVED_INITIAL_ROWS).div_ceil(ARCHIVED_PAGE_ROWS)
                        * ARCHIVED_PAGE_ROWS;
            }
        }
        if self.settings.sidebar_collapsed {
            self.toggle_sidebar(cx);
        }
        cx.notify();
        true
    }

    /// Mark a disclosure opened for a reveal: its motion starts on the next
    /// render, where the body height is known. Like a header click, it
    /// replaces the resort glide for this movement.
    fn queue_sidebar_reveal(&mut self, key: &str) {
        self.sidebar_reveal_motions.insert(key.to_owned());
        self.sidebar_prev_order.clear();
        self.sidebar_resort.clear();
        self.sidebar_new_keys.clear();
    }

    /// The Archived shelf's chats under the project filter, in sidebar order.
    fn archived_sidebar_chats(&self, cx: &App) -> Vec<zeron_proto::Chat> {
        let state = self.state.read(cx);
        let in_filter = state.project_filter(self.settings.space_filter.as_deref());
        let mut rows: Vec<zeron_proto::Chat> = state
            .chats
            .iter()
            // Spawned children stay out of the Archived section too — the
            // same top-level rule as `visible_chats`.
            .filter(|c| c.archived && c.is_top_level())
            .filter(|chat| in_filter(chat))
            .cloned()
            .collect();
        rows.sort_by(|left, right| compare_sidebar_chats(self.settings.sidebar_sort, left, right));
        rows
    }

    // ---- add-space flow ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        self.release_home_folder_search(cx);
        self.command_palette = None;
        self.project_crumb_menu = popover::Popup::default();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_space_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search devices…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                // Typing `/` after a query that names a folder descends into
                // it — the query reads as a path segment, so the slash IS the
                // pick (shell-style). Otherwise the slash stays in the query
                // (it matches nothing, which is honest feedback).
                if this.add_space_slash_descend(cx) {
                    return;
                }
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                    flow.list_scroll.set_offset(gpui::Point::default());
                }
                this.search_home_folders(cx);
                cx.notify();
            }
        });
        self.add_space = Some(AddSpaceFlow {
            step: ProjectStep::Devices,
            location: None,
            device: None,
            search,
            browser: Loadable::Idle,
            drives: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            cloning: None,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            crumb_scroll: gpui::ScrollHandle::new(),
            crumb_key: String::new(),
            focus_pending: true,
            load_task: None,
            drives_task: None,
            submit_task: None,
            home_search: HomeFolderSearch::default(),
            path_edit: None,
            _search_events: search_events,
        });
        cx.notify();
    }

    /// Close the new-project picker, releasing the host's home index.
    pub(super) fn close_add_space(&mut self, cx: &mut Context<Self>) {
        self.release_home_folder_search(cx);
        self.add_space = None;
        cx.notify();
    }

    /// Whether the picker browses the Home location, where the query also
    /// searches every folder under home.
    fn add_space_in_home(&self) -> bool {
        self.add_space.as_ref().is_some_and(|flow| {
            flow.step == ProjectStep::Folders
                && flow
                    .location
                    .as_ref()
                    .is_some_and(|(_, path)| path.is_none())
        })
    }

    /// Look the query up under the device's home. The first call in Home
    /// also starts the host's index, so the first real query is fast.
    fn search_home_folders(&mut self, cx: &mut Context<Self>) {
        let in_home = self.add_space_in_home();
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let query = flow.search.read(cx).text().trim().to_owned();
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        let search = &mut flow.home_search;
        search.generation += 1;
        search.loading = false;
        search.indexing = false;
        search.task = None;
        search.results.clear();
        let Some(engine) = engine.filter(|_| in_home && !search.unsupported) else {
            return;
        };
        if !search.started {
            // Its own task: typing must not cancel it.
            search.started = true;
            search.warm_task = Some(request_home_folders(
                engine.clone(),
                device_id.clone(),
                local.clone(),
                String::new(),
                None,
                cx,
            ));
        }
        if query.chars().count() < HOME_FOLDER_MIN_QUERY_CHARS
            || crate::pickers::is_typed_path(&query)
        {
            return;
        }
        search.loading = true;
        let generation = Some(search.generation);
        search.task = Some(request_home_folders(
            engine, device_id, local, query, generation, cx,
        ));
    }

    /// Tell the host the picker is done with its home index (it stays when a
    /// projectless chat warmed it).
    fn release_home_folder_search(&mut self, cx: &mut Context<Self>) {
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let search = std::mem::take(&mut flow.home_search);
        flow.home_search.unsupported = search.unsupported;
        let (true, Some(engine)) = (search.started, engine) else {
            return;
        };
        let mut params = serde_json::Map::new();
        add_space_target(
            &mut params,
            flow.device.as_ref().map(|d| d.id.as_str()),
            local.as_deref(),
        );
        cx.spawn(async move |_, _| {
            if let Err(error) = engine
                .client()
                .call(
                    methods::RELEASE_HOME_FOLDER_SEARCH,
                    serde_json::Value::Object(params),
                )
                .await
            {
                tracing::debug!(%error, "home folder search release failed");
            }
        })
        .detach();
    }

    /// Selecting a device advances to its locations.
    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        let cloud = zeron_proto::is_cloud_device(&device.id, &device.platform);
        if cloud {
            self.state
                .update(cx, |state, cx| state.refresh_cloud_repos(cx));
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.focus_pending = true;
        flow.step = if cloud {
            ProjectStep::Repositories
        } else {
            ProjectStep::Locations
        };
        flow.location = None;
        flow.load_task = None;
        flow.drives_task = None;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.device = Some(device);
        flow.browser = Loadable::Idle;
        flow.drives = Loadable::Idle;
        flow.browser_path = None;
        flow.home = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        flow.path_edit = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| {
            input.set_placeholder(
                if cloud {
                    "Search repositories…"
                } else {
                    "Search locations…"
                },
                cx,
            );
            input.set_text("", cx);
        });
        if !cloud {
            self.load_space_drives(cx);
        }
        cx.notify();
    }

    fn add_space_goto_location(
        &mut self,
        name: String,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if path.is_some() {
            self.release_home_folder_search(cx);
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.focus_pending = true;
        flow.step = ProjectStep::Folders;
        flow.location = Some((name, path.clone()));
        flow.browser_repo = false;
        flow.path_edit = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| {
            input.set_placeholder("Search folders…", cx);
            input.set_text("", cx);
        });
        self.load_space_folders(path, cx);
        // Entering Home starts the host's index (an empty query only warms).
        self.search_home_folders(cx);
    }

    fn add_space_back_to(&mut self, step: ProjectStep, cx: &mut Context<Self>) {
        self.release_home_folder_search(cx);
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        // Cloud has no locations: its crumb goes back to its repositories.
        let step = match (&flow.device, step) {
            (Some(device), ProjectStep::Locations)
                if zeron_proto::is_cloud_device(&device.id, &device.platform) =>
            {
                ProjectStep::Repositories
            }
            (_, step) => step,
        };
        flow.focus_pending = true;
        flow.step = step;
        flow.load_task = None;
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.location = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        flow.path_edit = None;
        flow.list_scroll.set_offset(gpui::Point::default());
        if step == ProjectStep::Devices {
            flow.drives_task = None;
            flow.device = None;
            flow.drives = Loadable::Idle;
            flow.home = None;
        }
        let search = flow.search.clone();
        search.update(cx, |input, cx| {
            input.set_placeholder(
                match step {
                    ProjectStep::Devices => "Search devices…",
                    ProjectStep::Repositories => "Search repositories…",
                    _ => "Search locations…",
                },
                cx,
            );
            input.set_text("", cx);
        });
        cx.notify();
    }

    fn add_space_devices(&self, cx: &App) -> Vec<Device> {
        let Some(flow) = &self.add_space else {
            return Vec::new();
        };
        let state = self.state.read(cx);
        // Session machines are Cloud's, not devices to keep a project on.
        let mut devices: Vec<Device> = state
            .devices
            .iter()
            .filter(|device| crate::cloud::is_listed(device))
            .cloned()
            .collect();
        // With GitHub connected, a project can live on Cloud alone: one of
        // the repositories Cloud reaches, not a folder on any device.
        if let Some(cloud) = add_space_cloud_device(state) {
            devices.push(cloud);
        }
        let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
        popover::filter_indices(flow.search.read(cx).text(), &names)
            .into_iter()
            .map(|ix| devices[ix].clone())
            .collect()
    }

    /// The Repositories step's rows: Cloud's repositories matching the
    /// query, most recently pushed first.
    fn add_space_repos(&self, cx: &App) -> Vec<zeron_proto::GithubRepo> {
        let Some(flow) = &self.add_space else {
            return Vec::new();
        };
        let mut repos: Vec<zeron_proto::GithubRepo> = self
            .state
            .read(cx)
            .cloud_repos
            .iter()
            .flatten()
            .cloned()
            .collect();
        repos.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));
        let names: Vec<_> = repos.iter().map(|r| r.full_name.as_str()).collect();
        popover::filter_indices(flow.search.read(cx).text(), &names)
            .into_iter()
            .map(|ix| repos[ix].clone())
            .collect()
    }

    /// Make `repo` a Cloud project (or open the one Cloud already has).
    fn add_space_create_cloud(&mut self, repo: zeron_proto::GithubRepo, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(cloud) = add_space_cloud_device(self.state.read(cx)) else {
            return;
        };
        let name = repo
            .full_name
            .rsplit('/')
            .next()
            .unwrap_or(&repo.full_name)
            .to_string();
        // Where a session machine checks the repository out.
        let path = format!("/home/user/{name}");
        self.add_space_create_on(engine, cloud, path, true, Some(repo.full_name), cx);
    }

    fn add_space_locations(&self, cx: &App) -> Vec<(String, Option<String>)> {
        let Some(flow) = &self.add_space else {
            return Vec::new();
        };
        let locations: Vec<_> = std::iter::once(("Home".to_string(), None))
            .chain(
                flow.drives
                    .ready()
                    .into_iter()
                    .flatten()
                    .map(|d| (d.name.clone(), Some(d.path.clone()))),
            )
            .collect();
        let names: Vec<_> = locations.iter().map(|(name, _)| name.as_str()).collect();
        popover::filter_indices(flow.search.read(cx).text(), &names)
            .into_iter()
            .map(|ix| locations[ix].clone())
            .collect()
    }

    /// ListDrives on the flow's device (relay-forwarded when remote).
    /// Failures stay silent — the section just shows Home; the folder
    /// browser's own error row already covers "device didn't respond".
    fn load_space_drives(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        flow.drives = Loadable::Loading;
        flow.drives_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            // Only target remote devices — local calls skip the relay.
            if let (Some(target), local) = (&device_id, &local)
                && local.as_deref() != Some(target.as_str())
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_DRIVES, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.drives = match result {
                        Ok(value) => match serde_json::from_value::<DriveListing>(value) {
                            Ok(listing) => Loadable::Ready(listing.drives),
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<zeron_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        if flow.step != ProjectStep::Folders {
            return Vec::new();
        }
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// The Folders step's rows: the open folder's matching subfolders, then
    /// matches from anywhere under home not already listed.
    fn add_space_folder_rows(&self, cx: &App) -> Vec<FolderRow> {
        let local = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let open = flow.browser.ready().map(|listing| listing.path.as_str());
        let listed: HashSet<String> = open
            .map(|open| {
                local
                    .iter()
                    .map(|entry| crate::pickers::child_path(open, &entry.name))
                    .chain(std::iter::once(open.to_owned()))
                    .map(|path| folder_path_key(&path))
                    .collect()
            })
            .unwrap_or_default();
        let home = if self.add_space_in_home() {
            flow.home_search.results.as_slice()
        } else {
            &[]
        };
        let query = flow.search.read(cx).text().trim().to_owned();
        // A pasted remote is the one thing to do: clone it here.
        if let Some(url) = git_url(&query) {
            return vec![FolderRow::Clone(CloneRow {
                label: git_url_label(&url),
                url,
                github: false,
                private: false,
            })];
        }
        // Otherwise the user's GitHub repositories matching a name-like query
        // follow the folders (what Cloud reaches; GitHub connected).
        let needle = query.to_lowercase();
        let github: Vec<FolderRow> = if needle.is_empty()
            || needle.starts_with(['/', '~', '.'])
        {
            Vec::new()
        } else {
            let mut repos: Vec<&zeron_proto::GithubRepo> = self
                .state
                .read(cx)
                .cloud_repos
                .iter()
                .flatten()
                .filter(|repo| repo.full_name.to_lowercase().contains(&needle))
                .collect();
            repos.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));
            repos
                .into_iter()
                .take(CLONE_MATCHES)
                .map(|repo| {
                    FolderRow::Clone(CloneRow {
                        url: repo.clone_url.clone(),
                        label: repo.full_name.clone(),
                        github: true,
                        private: repo.private,
                    })
                })
                .collect()
        };
        local
            .into_iter()
            .map(FolderRow::Local)
            .chain(
                home.iter()
                    .filter(|found| !listed.contains(&folder_path_key(&found.path)))
                    .cloned()
                    .map(FolderRow::Home),
            )
            .chain(github)
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    /// A path-shaped query with no matching rows browses the typed path
    /// instead — `/disk2⏎` must work, not sit on "No folders match" (an
    /// absolute query can never match a folder name anyway).
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        match flow.step {
            ProjectStep::Devices => {
                if let Some(device) = self.add_space_devices(cx).get(flow.active).cloned() {
                    self.add_space_pick_device(device, cx);
                }
                return;
            }
            ProjectStep::Locations => {
                if let Some((name, path)) = self.add_space_locations(cx).get(flow.active).cloned() {
                    self.add_space_goto_location(name, path, cx);
                }
                return;
            }
            ProjectStep::Repositories => {
                if let Some(repo) = self.add_space_repos(cx).get(flow.active).cloned() {
                    self.add_space_create_cloud(repo, cx);
                }
                return;
            }
            ProjectStep::Folders => {}
        }
        let rows = self.add_space_folder_rows(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if rows.is_empty() {
            let text = flow.search.read(cx).text().to_string();
            if let Some(target) = crate::pickers::typed_path_target(&text, flow.home.as_deref()) {
                self.add_space_descend(target, false, cx);
            }
            return;
        }
        let entry = match rows.get(flow.active) {
            Some(FolderRow::Local(entry)) => entry,
            Some(FolderRow::Home(found)) => {
                let (path, is_repo) = (found.path.clone(), found.is_repo);
                self.add_space_descend(path, is_repo, cx);
                return;
            }
            Some(FolderRow::Clone(clone)) => {
                let clone = clone.clone();
                self.add_space_clone(clone, cx);
                return;
            }
            None => return,
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Slash-descend: when the query ends in `/` and the part before it names
    /// a folder of the current listing (exact name — matching casing wins
    /// over a case-colliding sibling — else a unique prefix), descend into it
    /// as though it were picked. Returns whether it fired —
    /// descending clears the query, so the caller must not keep acting on the
    /// old text.
    fn add_space_slash_descend(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .add_space
            .as_ref()
            .is_none_or(|f| f.step != ProjectStep::Folders)
        {
            return false;
        }
        // A typed PATH jump: an absolute (`/disk2/`), drive-rooted (`D:\x\`)
        // or home-relative (`~/x/`) query browses that path directly — mounts
        // at unconventional roots (and anywhere else) are reachable without a
        // Locations row. Same trailing-separator trigger as the folder-name
        // descend below.
        {
            let Some(flow) = self.add_space.as_ref() else {
                return false;
            };
            let text = flow.search.read(cx).text().to_string();
            if crate::pickers::is_typed_path(&text) && text.ends_with(['/', '\\']) {
                let target = crate::pickers::typed_path_target(&text, flow.home.as_deref());
                let Some(target) = target else {
                    // Path-shaped but unresolvable (`~/…` before home is
                    // known) — leave the query alone.
                    return false;
                };
                self.add_space_descend(target, false, cx);
                return true;
            }
        }
        let target = {
            let Some(flow) = self.add_space.as_ref() else {
                return false;
            };
            let text = flow.search.read(cx).text().to_string();
            let Some(query) = text.strip_suffix('/') else {
                return false;
            };
            if query.is_empty() || query.contains('/') {
                return false;
            }
            let Some(listing) = flow.browser.ready() else {
                return false;
            };
            let dirs = browser_rows(listing);
            let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
            crate::pickers::segment_target(&names, query).map(|ix| {
                (
                    crate::pickers::child_path(&listing.path, &dirs[ix].name),
                    dirs[ix].is_repo,
                )
            })
        };
        let Some((full, is_repo)) = target else {
            return false;
        };
        self.add_space_descend(full, is_repo, cx);
        true
    }

    /// The tab-completion target: the highlighted row when the query prefixes
    /// its name, else the first prefix match (filtering ranks those first).
    /// `(full name, remaining suffix)`; `None` on an empty query or when the
    /// match is already complete.
    fn add_space_completion(&self, cx: &App) -> Option<(String, String)> {
        let flow = self.add_space.as_ref()?;
        let query = flow.search.read(cx).text().to_string();
        if query.is_empty() {
            return None;
        }
        let rows = self.add_space_filtered(cx);
        let entry = rows
            .get(flow.active)
            .filter(|e| completion_prefix_len(&e.name, &query).is_some())
            .or_else(|| {
                rows.iter()
                    .find(|e| completion_prefix_len(&e.name, &query).is_some())
            })?;
        let len = completion_prefix_len(&entry.name, &query)?;
        if len >= entry.name.len() {
            return None;
        }
        Some((entry.name.clone(), entry.name[len..].to_string()))
    }

    /// ⇥: accept the completion — the query becomes the full folder name
    /// (the ghost the input was previewing). Descending stays on `/`/⏎.
    fn add_space_accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some((name, _)) = self.add_space_completion(cx) else {
            return;
        };
        if let Some(flow) = self.add_space.as_ref() {
            let search = flow.search.clone();
            search.update(cx, |input, cx| input.set_text(name, cx));
        }
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// ListFolders on the flow's device (relay-forwarded when remote).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.focus_pending = true;
        flow.path_edit = None;
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        let Some(engine) = engine else {
            flow.browser = Loadable::Error("Device is not connected".into());
            cx.notify();
            return;
        };
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let result = list_folders_on(engine, path, device_id, local).await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(listing) => {
                            // A pathless browse resolved home — remember it
                            // so the breadcrumbs can fold it into the
                            // device crumb.
                            if went_home {
                                flow.home = Some(listing.path.clone());
                            }
                            Loadable::Ready(listing)
                        }
                        Err(error) => Loadable::Error(error),
                    };
                }
                // A path field opened while this loaded: ⇥ can use it now.
                shell.add_space_refresh_path_completion(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the space for the browser's current folder.
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.submit_busy || flow.step != ProjectStep::Folders {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        self.add_space_create(engine, device, path, git_detected, cx);
    }

    /// Make `path` on `device` a project and land in it (an existing one for
    /// the same folder is reused).
    fn add_space_create(
        &mut self,
        engine: crate::state::EngineHandle,
        device: Device,
        path: String,
        git_detected: bool,
        cx: &mut Context<Self>,
    ) {
        self.add_space_create_on(engine, device, path, git_detected, None, cx);
    }

    /// `github_repo` only for a Cloud project, which has no device to read
    /// its repository from.
    fn add_space_create_on(
        &mut self,
        engine: crate::state::EngineHandle,
        device: Device,
        path: String,
        git_detected: bool,
        github_repo: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Same (device, folder) already has a space → just switch to it. The
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling. On Cloud,
        // the same repository is the same project.
        if let Some(existing) = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| {
                s.device_id == device.id
                    && (same_folder_path(&s.path, &path)
                        || github_repo.as_deref().is_some_and(|repo| {
                            s.github_repo
                                .as_deref()
                                .is_some_and(|own| own.eq_ignore_ascii_case(repo))
                        }))
            })
            .map(|s| s.id.clone())
        {
            self.close_add_space(cx);
            self.land_in_space(existing, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            repository_id: github_repo.as_deref().map(|repo| {
                zeron_proto::view::cloud_repository_id(&self.state.read(cx).spaces, repo)
            }),
            github_repo: github_repo.clone(),
            icon: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let mut params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        if let Some(repo) = github_repo {
            params["githubRepo"] = serde_json::Value::String(repo);
        }
        let submit_id = space_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.close_add_space(cx);
                        shell.land_in_space(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.cloning = None;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Clone `clone` into the open folder on the chosen device (relayed to a
    /// remote one), then make the checkout the project.
    fn add_space_clone(&mut self, clone: CloneRow, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.submit_busy || flow.step != ProjectStep::Folders {
            return;
        }
        // "Here" is the listed folder: the trail names it again while the
        // clone runs, not a path still being typed.
        if flow.path_edit.take().is_some() {
            flow.focus_pending = true;
        }
        let (Some(device), Some(listing)) = (flow.device.clone(), flow.browser.ready()) else {
            return;
        };
        let parent = listing.path.clone();
        flow.submit_busy = true;
        flow.cloning = Some(clone.label.clone());
        flow.error = None;
        let mut params = serde_json::Map::new();
        params.insert("url".into(), clone.url.clone().into());
        params.insert("parent".into(), parent.into());
        add_space_target(&mut params, Some(&device.id), local.as_deref());
        let label = clone.label;
        let task = cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<zeron_proto::Repo>(methods::CLONE_REPO, params.into())
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(repo) => shell.add_space_create(engine, device, repo.path, true, cx),
                    Err(err) => {
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.cloning = None;
                            flow.error = Some(format!("Couldn't clone {label}: {err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Back traverses folders, then locations, then devices.
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = &self.add_space else {
            return;
        };
        match flow.step {
            ProjectStep::Devices => return,
            ProjectStep::Locations | ProjectStep::Repositories => {
                self.add_space_back_to(ProjectStep::Devices, cx)
            }
            ProjectStep::Folders => {
                let listing = flow.browser.ready();
                let root = flow
                    .location
                    .as_ref()
                    .and_then(|(_, path)| path.as_deref())
                    .or(flow.home.as_deref());
                let parent = listing
                    .filter(|l| !root.is_some_and(|root| same_folder_path(&l.path, root)))
                    .and_then(|l| parent_path(&l.path));
                if let Some(parent) = parent {
                    self.add_space_descend(parent, false, cx);
                } else {
                    self.add_space_back_to(ProjectStep::Locations, cx);
                }
            }
        }
    }

    // ---- typed path: the crumb row as an address bar ----

    /// Whether the device's paths are Windows-shaped (`\` separates them):
    /// what it listed says so; before that, its platform.
    fn add_space_windows_paths(&self) -> bool {
        self.add_space.as_ref().is_some_and(|flow| {
            flow.open_folder().map_or_else(
                || {
                    flow.device
                        .as_ref()
                        .is_some_and(|device| device.platform == "windows")
                },
                |path| is_windows_path(&path),
            )
        })
    }

    /// ⌘L, the pen, or a click beside the crumbs: the trail becomes the open
    /// folder's path, selected — a paste replaces it, → edits its end. On
    /// the device list it types a path on the highlighted device; ⌘L in the
    /// field selects the path again.
    pub(super) fn add_space_edit_path(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.step == ProjectStep::Devices {
            let Some(device) = self.add_space_devices(cx).get(flow.active).cloned() else {
                return;
            };
            // Cloud keeps repositories, not paths.
            if zeron_proto::is_cloud_device(&device.id, &device.platform) {
                return;
            }
            self.add_space_pick_device(device, cx);
        }
        let windows = self.add_space_windows_paths();
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if !matches!(flow.step, ProjectStep::Locations | ProjectStep::Folders) {
            return;
        }
        if let Some(edit) = flow.path_edit.as_mut() {
            edit.input.update(cx, |input, cx| input.select_all_text(cx));
            edit.focus_pending = true;
            cx.notify();
            return;
        }
        // The open folder, spelled from `~` under home and ending in a
        // separator so a name typed after it (→ first) goes inside.
        let mut text = flow.open_folder().map_or_else(
            || "~".to_string(),
            |path| escape_typed_path(&tilde_path(&path, flow.home.as_deref()), windows),
        );
        if !text.ends_with(['/', '\\']) {
            text.push(if windows { '\\' } else { '/' });
        }
        let input = cx.new(|cx| {
            ComposerInput::new(
                if windows {
                    r"Folder path, like C:\code"
                } else {
                    "Folder path, like ~/code"
                },
                cx,
            )
            .with_text_metrics(12.0, 16.0)
            .with_single_line()
            .with_accessibility_role(gpui::Role::TextInput)
        });
        input.update(cx, |input, cx| {
            input.set_text(text, cx);
            input.select_all_text(cx);
        });
        let events = cx.subscribe(&input, |this: &mut Shell, input, event, cx| match event {
            ComposerInputEvent::Edited => this.add_space_path_edited(cx),
            ComposerInputEvent::Submitted => this.add_space_open_typed_path(false, cx),
            // A folder copied in Finder or Explorer pastes as files, not text.
            ComposerInputEvent::PastedPaths(paths) => {
                if let Some(path) = paths.first() {
                    let windows = this.add_space_windows_paths();
                    let home = this.add_space.as_ref().and_then(|flow| flow.home.clone());
                    let path = tilde_path(&path.to_string_lossy(), home.as_deref());
                    let text = escape_typed_path(&path, windows);
                    input.update(cx, |input, cx| input.set_text(text, cx));
                }
            }
            _ => {}
        });
        // `~` and ⇥ under home need the device's home: list it once.
        let home_task = match (flow.home.is_none(), engine) {
            (true, Some(engine)) => {
                let device_id = flow.device.as_ref().map(|device| device.id.clone());
                Some(cx.spawn(async move |this, cx| {
                    let listing = list_folders_on(engine, None, device_id, local).await;
                    this.update(cx, |shell, cx| {
                        let (Ok(listing), Some(flow)) = (listing, shell.add_space.as_mut()) else {
                            return;
                        };
                        // Home's folders are ⇥'s source for `~/…`, unless it
                        // already lists another folder.
                        let key = folder_path_key(&listing.path);
                        if let Some(edit) = flow.path_edit.as_mut()
                            && edit
                                .siblings
                                .as_ref()
                                .is_none_or(|(known, _)| *known == key)
                        {
                            let folders = browser_rows(&listing).into_iter().cloned().collect();
                            edit.siblings = Some((key, Loadable::Ready(folders)));
                            edit.siblings_task = None;
                        }
                        flow.home.get_or_insert(listing.path);
                        // Whichever listing learned home first, `~` resolves now.
                        shell.add_space_refresh_path_completion(cx);
                        cx.notify();
                    })
                    .ok();
                }))
            }
            _ => None,
        };
        flow.path_edit = Some(PathEdit {
            input,
            opening: false,
            adding: false,
            error: None,
            siblings: None,
            focus_pending: true,
            open_task: None,
            siblings_task: None,
            _home_task: home_task,
            _events: events,
        });
        // The field, not the search, takes the keyboard.
        flow.focus_pending = false;
        self.add_space_refresh_path_completion(cx);
        cx.notify();
    }

    /// Esc in the path field: the crumbs again, the open folder untouched.
    /// `false` when no path field is open (Esc then closes the palette).
    pub(super) fn add_space_cancel_path_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(flow) = self.add_space.as_mut() else {
            return false;
        };
        if flow.path_edit.take().is_none() {
            return false;
        }
        flow.focus_pending = true;
        cx.notify();
        true
    }

    /// The path field's text changed: a fresh attempt (an answer for the
    /// old text must not open it), and ⇥ follows the folder being typed in.
    fn add_space_path_edited(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = self
            .add_space
            .as_mut()
            .and_then(|flow| flow.path_edit.as_mut())
        {
            edit.error = None;
            edit.opening = false;
            edit.open_task = None;
        }
        self.add_space_refresh_path_completion(cx);
        cx.notify();
    }

    /// Keep ⇥'s source on the typed path's parent folder: the open listing
    /// answers for itself; any other folder is listed once per parent.
    fn add_space_refresh_path_completion(&mut self, cx: &mut Context<Self>) {
        let windows = self.add_space_windows_paths();
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let open = flow.browser.ready();
        let base = flow.open_folder();
        let device_id = flow.device.as_ref().map(|device| device.id.clone());
        let Some(edit) = flow.path_edit.as_mut() else {
            return;
        };
        let text = edit.input.read(cx).text().to_owned();
        let (folder, _) = split_typed_leaf(&text, windows);
        let parent = if folder.is_empty() {
            base
        } else {
            resolve_typed_path(folder, base.as_deref(), flow.home.as_deref(), windows)
        };
        let Some(parent) = parent else {
            edit.siblings = None;
            edit.siblings_task = None;
            return;
        };
        let key = folder_path_key(&parent);
        if edit
            .siblings
            .as_ref()
            .is_some_and(|(known, _)| *known == key)
        {
            return;
        }
        if let Some(listing) = open.filter(|listing| same_folder_path(&listing.path, &parent)) {
            let folders = browser_rows(listing).into_iter().cloned().collect();
            edit.siblings = Some((key, Loadable::Ready(folders)));
            edit.siblings_task = None;
            return;
        }
        let Some(engine) = engine else {
            edit.siblings = None;
            return;
        };
        edit.siblings = Some((key.clone(), Loadable::Loading));
        edit.siblings_task = Some(cx.spawn(async move |this, cx| {
            let listing = list_folders_on(engine, Some(parent), device_id, local).await;
            this.update(cx, |shell, cx| {
                if let Some((known, folders)) = shell
                    .add_space
                    .as_mut()
                    .and_then(|flow| flow.path_edit.as_mut())
                    .and_then(|edit| edit.siblings.as_mut())
                    && *known == key
                {
                    *folders = match listing {
                        Ok(listing) => {
                            Loadable::Ready(browser_rows(&listing).into_iter().cloned().collect())
                        }
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// ⇥ in the path field: the name being typed completed to the folder
    /// of its parent it names exactly, else the first it prefixes
    /// (case-insensitively), separator appended — `(text after ⇥, the faint
    /// suffix previewing it)`.
    fn add_space_path_completion(&self, cx: &App) -> Option<(String, Option<SharedString>)> {
        let windows = self.add_space_windows_paths();
        let edit = self.add_space.as_ref()?.path_edit.as_ref()?;
        let Some((_, Loadable::Ready(folders))) = &edit.siblings else {
            return None;
        };
        let text = edit.input.read(cx).text();
        let (folder, name) = split_typed_leaf(text, windows);
        let name = unescape_typed_path(name, windows);
        if matches!(name.as_str(), "" | "." | "..") {
            return None;
        }
        // `code` stays `code` beside a `Code` listed first.
        let (entry, typed) = folders
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| (entry, entry.name.len()))
            .or_else(|| {
                folders
                    .iter()
                    .find_map(|entry| Some((entry, completion_prefix_len(&entry.name, &name)?)))
            })?;
        let separator = match folder.chars().last() {
            Some(separator @ ('/' | '\\')) => separator,
            _ if windows => '\\',
            _ => '/',
        };
        let ghost =
            (typed < entry.name.len()).then(|| SharedString::from(entry.name[typed..].to_string()));
        let entry_name = escape_typed_path(&entry.name, windows);
        Some((format!("{folder}{entry_name}{separator}"), ghost))
    }

    fn add_space_accept_path_completion(&mut self, cx: &mut Context<Self>) {
        let Some((text, _)) = self.add_space_path_completion(cx) else {
            return;
        };
        if let Some(edit) = self
            .add_space
            .as_ref()
            .and_then(|flow| flow.path_edit.as_ref())
        {
            let input = edit.input.clone();
            input.update(cx, |input, cx| input.set_text(text, cx));
        }
    }

    /// ↵ (⌘↵ with `add`) in the path field: list the typed path on the
    /// device; once it answers it is the open folder — and with ⌘↵ the new
    /// project. A path that doesn't list keeps the field and says why.
    fn add_space_open_typed_path(&mut self, add: bool, cx: &mut Context<Self>) {
        let windows = self.add_space_windows_paths();
        let engine = self.state.read(cx).engine().cloned();
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let base = flow.open_folder();
        let home = flow.home.clone();
        let device_id = flow.device.as_ref().map(|device| device.id.clone());
        let Some(edit) = flow.path_edit.as_mut() else {
            return;
        };
        // This text is already on its way (an edit would have stopped it):
        // ⌘↵ adds it when it lands, ↵ doesn't start it over.
        if edit.opening {
            edit.adding |= add;
            return;
        }
        let text = edit.input.read(cx).text().to_owned();
        let engine = match engine {
            Some(engine) if !text.trim().is_empty() => engine,
            engine => {
                edit.error = Some(if engine.is_none() {
                    "Device is not connected".into()
                } else {
                    "Type the folder's path".into()
                });
                cx.notify();
                return;
            }
        };
        edit.opening = true;
        edit.adding = add;
        edit.error = None;
        edit.open_task = Some(cx.spawn(async move |this, cx| {
            let mut home = home;
            let result: Result<FolderListing, String> = async {
                let resolve = |home: &Option<String>| {
                    let base = base.as_deref().or(home.as_deref());
                    resolve_typed_path(&text, base, home.as_deref(), windows)
                };
                let unresolved = || format!("Can't open {}", text.trim());
                let target = match resolve(&home) {
                    Some(target) => target,
                    // `~`, or a relative path with nothing open: learn the
                    // device's home first.
                    None if home.is_none() => {
                        let found =
                            list_folders_on(engine.clone(), None, device_id.clone(), local.clone())
                                .await?;
                        home = Some(found.path);
                        resolve(&home).ok_or_else(unresolved)?
                    }
                    None => return Err(unresolved()),
                };
                let listed = if home.is_none() {
                    // Home is learned alongside, so a path in it lands under
                    // Home rather than under its drive.
                    let (found, listed) = futures::join!(
                        list_folders_on(engine.clone(), None, device_id.clone(), local.clone()),
                        list_folders_on(engine, Some(target.clone()), device_id, local),
                    );
                    home = found.ok().map(|found| found.path);
                    listed
                } else {
                    list_folders_on(engine, Some(target.clone()), device_id, local).await
                };
                listed
                    .map_err(|error| path_open_error(&tilde_path(&target, home.as_deref()), &error))
            }
            .await;
            this.update(cx, |shell, cx| {
                shell.add_space_finish_typed_path(home, result, cx)
            })
            .ok();
        }));
        cx.notify();
    }

    /// The typed path's answer: its listing becomes the open folder under
    /// the location holding it (and the new project, for ⌘↵); an error
    /// stays in the field.
    fn add_space_finish_typed_path(
        &mut self,
        home: Option<String>,
        result: Result<FolderListing, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        // Home learned on the way stays learned, whatever the answer.
        if flow.home.is_none() {
            flow.home = home;
        }
        let Some(edit) = flow.path_edit.as_mut() else {
            return;
        };
        edit.opening = false;
        let add = std::mem::take(&mut edit.adding);
        let listing = match result {
            Ok(listing) => listing,
            Err(error) => {
                edit.error = Some(error.into());
                edit.focus_pending = true;
                cx.notify();
                return;
            }
        };
        let location = typed_path_location(
            &listing.path,
            flow.home.as_deref(),
            flow.drives.ready().map(Vec::as_slice).unwrap_or_default(),
        );
        // Outside Home its folder index goes, as when picking a drive.
        let leaves_home = location.1.is_some();
        // The open folder's git seed holds while that folder stays open.
        flow.browser_repo &= flow
            .browser
            .ready()
            .is_some_and(|open| same_folder_path(&open.path, &listing.path));
        flow.path_edit = None;
        flow.focus_pending = true;
        flow.step = ProjectStep::Folders;
        flow.location = Some(location);
        flow.browser_path = Some(listing.path.clone());
        flow.browser = Loadable::Ready(listing);
        flow.load_task = None;
        flow.active = 0;
        flow.error = None;
        flow.list_scroll.set_offset(gpui::Point::default());
        let search = flow.search.clone();
        if leaves_home {
            self.release_home_folder_search(cx);
        }
        search.update(cx, |input, cx| {
            input.set_placeholder("Search folders…", cx);
            // Clearing the query re-runs the Home search, warming it on
            // entry — not worth it when ⌘↵ is about to close the palette.
            if !(add && input.is_empty()) {
                input.set_text("", cx);
            }
        });
        if add {
            self.submit_add_space(cx);
        }
        cx.notify();
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key: ↑↓ (or ctrl-n/p) navigate, →/⏎ open the
    /// highlighted folder, ← up a level, ⇥ completes the query to the
    /// previewed folder name, ⌘⏎ add the OPEN folder, ⌫ (empty query) also
    /// goes up, esc closes, ⌘L types the path. (Typing `/` also descends —
    /// see the Edited subscription.) While the path field is open it edits
    /// its own text; ⇥ completes it, ⌘⏎ opens and adds it, esc puts the
    /// crumbs back.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // The address-bar chord.
        if keystroke.key == "l" && keystroke.modifiers == gpui::Modifiers::secondary_key() {
            self.add_space_edit_path(cx);
            cx.stop_propagation();
            return;
        }
        if self
            .add_space
            .as_ref()
            .is_some_and(|flow| flow.path_edit.is_some())
        {
            match keystroke.key.as_str() {
                "tab" if !keystroke.modifiers.modified() => {
                    self.add_space_accept_path_completion(cx)
                }
                // Not the Shell's focus walk: the keyboard stays in the field.
                "tab" if keystroke.modifiers.shift => {}
                // Only on Settings, where the Shell's Esc capture stands aside.
                "escape" => {
                    self.add_space_cancel_path_edit(cx);
                }
                "enter" if keystroke.modifiers == gpui::Modifiers::secondary_key() => {
                    self.add_space_open_typed_path(true, cx)
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_space_open_active(cx);
                return;
            }
            "left" => {
                self.add_space_go_up(cx);
                return;
            }
            // Unbound in "PaletteSearch" (like enter), so it bubbles here
            // instead of editing text — and stops here, short of the
            // Shell's focus walk.
            "tab" => {
                self.add_space_accept_completion(cx);
                cx.stop_propagation();
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.close_add_space(cx);
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = match self.add_space.as_ref().map(|f| f.step) {
                    Some(ProjectStep::Devices) => self.add_space_devices(cx).len(),
                    Some(ProjectStep::Locations) => self.add_space_locations(cx).len(),
                    Some(ProjectStep::Repositories) => self.add_space_repos(cx).len(),
                    _ => self.add_space_folder_rows(cx).len(),
                };
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // ⏎ opens the highlighted folder (an alias for →); the space is
            // added with ⌘⏎ — and the chord acts on the folder OPEN in the
            // breadcrumbs, not the highlight. The highlight auto-rests on the
            // first row, so a chord that took it would add arbitrary
            // subfolders; the usual target (a repo root full of subfolders)
            // is only ever "the folder you're standing in".
            popover::MenuKey::Enter => self.add_space_open_active(cx),
            popover::MenuKey::ModEnter => self.submit_add_space(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_space_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    pub(super) fn close_project_crumb_menu(&mut self, cx: &mut Context<Self>) {
        if self.project_crumb_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.project_crumb_menu);
            cx.notify();
        }
    }

    /// The folders folded into the `…` crumb, in path order.
    fn render_project_crumb_menu(
        &self,
        hidden: &[(String, String)],
        closing: Option<std::time::Instant>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = hidden.iter().enumerate().map(|(ix, (name, full))| {
            let full = full.clone();
            popover::menu_row(theme, false, format!("project-crumb-menu-{ix}"))
                .id(("project-crumb-menu", ix))
                .child(
                    icon(icons::FOLDER)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(div().min_w_0().truncate().child(name.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.project_crumb_menu = popover::Popup::default();
                    this.add_space_descend(full.clone(), false, cx);
                }))
        });
        let card = popover::popover_card(theme)
            .id("project-crumb-menu")
            .min_w(px(180.0))
            .max_w(px(280.0))
            .max_h(px(280.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_project_crumb_menu(cx)))
            .children(rows);
        popover::anchored_menu_below_layer(
            "project-crumb-menu",
            card.into_any_element(),
            closing,
            6.0,
            3,
        )
    }

    /// The same glass, header, row rhythm, scroll gutters and footer as Cmd+K.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let flow = self.add_space.as_mut()?;
        if let Some(edit) = flow.path_edit.as_mut() {
            // The path field keeps the keyboard while it is open: a click on
            // the card's chrome hands focus back to it, while focusing the
            // search field puts the crumbs back.
            let field = edit.input.focus_handle(cx);
            if std::mem::take(&mut edit.focus_pending)
                || window
                    .focused(cx)
                    .is_none_or(|focused| focused == flow.focus)
            {
                window.focus(&field, cx);
            } else if flow.search.focus_handle(cx).is_focused(window) {
                flow.path_edit = None;
            }
            flow.focus_pending = false;
        } else if std::mem::take(&mut flow.focus_pending) {
            window.focus(&flow.search.focus_handle(cx), cx);
        }
        let step = flow.step;
        let search = flow.search.clone();
        let focus = flow.focus.clone();
        let scroll = flow.list_scroll.clone();
        let device = flow.device.clone();
        let listing = flow.browser.ready().cloned();
        let location = flow.location.clone();
        let home = flow.home.clone();
        let browser_path = flow.browser_path.clone();
        let load_error = flow.browser.error().map(str::to_string);
        let error = flow.error.clone();
        let busy = flow.submit_busy;
        let cloning = flow.cloning.clone();
        let active = flow.active;
        let loading = matches!(flow.browser, Loadable::Idle | Loadable::Loading);
        let drives_loading = matches!(flow.drives, Loadable::Loading);
        let home_loading = flow.home_search.loading;
        let home_status = if flow.home_search.indexing {
            Some("Indexing…")
        } else if home_loading {
            Some("Searching…")
        } else {
            None
        };
        let path_field = flow
            .path_edit
            .as_ref()
            .map(|edit| (edit.input.clone(), edit.opening, edit.error.clone()));
        let ghost = self
            .add_space_completion(cx)
            .map(|(_, suffix)| SharedString::from(suffix));
        search.update(cx, |input, cx| {
            input.set_ghost(ghost, cx);
        });
        if let Some((input, ..)) = &path_field {
            let ghost = self
                .add_space_path_completion(cx)
                .and_then(|(_, ghost)| ghost);
            input.update(cx, |input, cx| input.set_ghost(ghost, cx));
        }
        // Cloud keeps repositories; every other device's crumbs are a path.
        let can_edit_path = matches!(step, ProjectStep::Locations | ProjectStep::Folders);
        let query = search.read(cx).text().to_string();
        // Cmd+K's row rhythm: 30px rows, 16px muted glyphs, 8px list gutters.
        let row = |ix: usize| {
            popover::menu_row(&theme, ix == active, format!("project-result-{ix}"))
                .id(("project-result", ix))
                .rounded(px(popover::PALETTE_ITEM_RADIUS))
                .min_h(px(30.0))
                .py(px(4.0))
                // Pointer motion moves the highlight, so hover and keyboard
                // never light two rows. Motion only: rows scrolling under a
                // resting pointer must not steal the keyboard's place.
                .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                    if let Some(flow) = this.add_space.as_mut()
                        && flow.active != ix
                    {
                        flow.active = ix;
                        cx.notify();
                    }
                }))
        };
        let glyph_el = |glyph: &'static str| {
            icon(glyph)
                .size(px(16.0))
                .flex_none()
                .text_color(theme.text_muted)
        };
        let label_el = |label: String| {
            div().flex_1().min_w_0().child(popover::search_highlight(
                label.into(),
                Some(&query),
                &theme,
            ))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match step {
            ProjectStep::Devices => {
                for (ix, device) in self.add_space_devices(cx).into_iter().enumerate() {
                    let online = self.state.read(cx).device_online(&device.id, Utc::now());
                    let name = device.name.clone();
                    rows.push(
                        row(ix)
                            .child(glyph_el(device_glyph(&device.platform)))
                            .child(label_el(name))
                            .child(
                                div()
                                    .size(px(5.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(if online {
                                        theme.success
                                    } else {
                                        theme.text_faint
                                    }),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_pick_device(device.clone(), cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            ProjectStep::Locations => {
                for (ix, (name, path)) in self.add_space_locations(cx).into_iter().enumerate() {
                    let glyph = if path.is_none() {
                        icons::HOME
                    } else {
                        icons::HARD_DRIVE
                    };
                    let label = name.clone();
                    rows.push(
                        row(ix)
                            .child(glyph_el(glyph))
                            .child(label_el(label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_goto_location(name.clone(), path.clone(), cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            ProjectStep::Repositories => {
                let spaces = self.state.read(cx).spaces.clone();
                let device_names: std::collections::HashMap<String, String> = self
                    .state
                    .read(cx)
                    .devices
                    .iter()
                    .map(|d| (d.id.clone(), d.name.clone()))
                    .collect();
                for (ix, repo) in self.add_space_repos(cx).into_iter().enumerate() {
                    // Already a project somewhere: say where, so a Cloud-only
                    // one is a deliberate pick.
                    let elsewhere = spaces
                        .iter()
                        .find(|space| {
                            space
                                .github_repo
                                .as_deref()
                                .is_some_and(|own| own.eq_ignore_ascii_case(&repo.full_name))
                                && !zeron_proto::is_cloud_device(&space.device_id, "")
                        })
                        .and_then(|space| device_names.get(&space.device_id).cloned());
                    let private = repo.private;
                    rows.push(
                        row(ix)
                            .child(glyph_el(icons::GITHUB_MARK))
                            .child(label_el(repo.full_name.clone()))
                            .when_some(elsewhere, |el, device| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.text_muted.opacity(0.75))
                                        .child(format!("On {device}")),
                                )
                            })
                            .when(private, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.text_muted.opacity(0.75))
                                        .child("Private"),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_create_cloud(repo.clone(), cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            ProjectStep::Folders => {
                if !loading && load_error.is_none() {
                    let base = listing.as_ref().map(|l| l.path.as_str()).unwrap_or("");
                    let mut first_home = true;
                    let mut first_github = true;
                    for (ix, folder) in self.add_space_folder_rows(cx).into_iter().enumerate() {
                        let entry = match folder {
                            FolderRow::Clone(clone) => {
                                let el = clone_row(row(ix), &clone, &query, &theme, cx);
                                rows.push(if clone.github && std::mem::take(&mut first_github) {
                                    div()
                                        .flex()
                                        .flex_col()
                                        .when(ix > 0, |el| el.pt(px(6.0)))
                                        .child(home_folders_header_titled(&theme, "From GitHub"))
                                        .child(el)
                                        .into_any_element()
                                } else {
                                    el.into_any_element()
                                });
                                continue;
                            }
                            FolderRow::Local(entry) => entry,
                            FolderRow::Home(found) => {
                                let el = home_folder_row(row(ix), &found, &query, &theme, cx);
                                rows.push(if std::mem::take(&mut first_home) {
                                    div()
                                        .flex()
                                        .flex_col()
                                        .when(ix > 0, |el| el.pt(px(6.0)))
                                        .child(home_folders_header(&theme, home_status))
                                        .child(el)
                                        .into_any_element()
                                } else {
                                    el.into_any_element()
                                });
                                continue;
                            }
                        };
                        let full = crate::pickers::child_path(base, &entry.name);
                        let is_repo = entry.is_repo;
                        rows.push(
                            row(ix)
                                .child(glyph_el(icons::FOLDER))
                                .child(label_el(entry.name))
                                .when(is_repo, |el| {
                                    el.child(
                                        icon(icons::GIT_BRANCH)
                                            .size(px(14.0))
                                            .flex_none()
                                            .text_color(theme.text_muted),
                                    )
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.add_space_descend(full.clone(), is_repo, cx)
                                }))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        if let Some(flow) = self.add_space.as_mut() {
            flow.active = flow.active.min(rows.len().saturating_sub(1));
        }
        let count = rows.len();
        // End spacing belongs to the content, so it scrolls out of the fade
        // instead of leaving a permanent gutter beside the chrome (Cmd+K).
        let rows = rows.into_iter().enumerate().map(|(ix, content)| {
            div()
                .flex_none()
                .px(px(8.0))
                .when(ix == 0, |row| row.pt(px(8.0)))
                .when(ix + 1 == count, |row| row.pb(px(8.0)))
                .child(content)
        });
        let mut results = div()
            .id("project-results")
            .min_h_0()
            .max_h(px(command_palette::palette_results_height(viewport)))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_LIST_GAP))
            .children(rows);
        if step == ProjectStep::Folders && loading {
            results = results.child(div().p(px(8.0)).child(popover::skeleton_rows(
                "project-loading",
                &theme,
                5,
                cx.entity_id(),
                cx,
            )));
        } else if let Some(message) = load_error.filter(|_| step == ProjectStep::Folders) {
            results = results.child(
                popover::error_row(&theme, &message).p(px(16.0)).child(
                    popover::btn_ghost(&theme, "Retry", "project-retry")
                        .id("project-retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        })),
                ),
            );
        } else if count == 0 && !(step == ProjectStep::Locations && drives_loading) {
            let (title, hint) = match step {
                ProjectStep::Devices => ("No devices found", "Try another device name.".into()),
                ProjectStep::Repositories => (
                    "No repositories found",
                    "Cloud lists the repositories the Zeron GitHub App can reach — add more in Settings → Cloud."
                        .into(),
                ),
                ProjectStep::Locations => {
                    ("No locations found", "Try Home or a drive name.".into())
                }
                // In the path field ⌘↵ adds the TYPED path and ← moves the caret.
                ProjectStep::Folders if query.is_empty() && path_field.is_some() => (
                    "No folders here",
                    "Type another path, or press Esc for the folder trail.".to_string(),
                ),
                ProjectStep::Folders if query.is_empty() => (
                    "No folders here",
                    format!(
                        "Add this folder with {}, or go back with ←.",
                        crate::settings::badge_combo("mod-enter")
                    ),
                ),
                ProjectStep::Folders if home_loading => (
                    "Searching Home…",
                    "Looking for matching folders anywhere under Home.".to_string(),
                ),
                ProjectStep::Folders if home_status == Some("Indexing…") => (
                    "No folders match yet",
                    "Home is still being indexed — keep typing or try again.".to_string(),
                ),
                ProjectStep::Folders => (
                    "No folders match",
                    "Type a path like ~/code or /mnt to jump there.".to_string(),
                ),
            };
            results = results.child(command_palette::palette_empty(&theme, title, hint));
        }
        if step == ProjectStep::Locations && drives_loading {
            results = results.child(
                div()
                    .px(px(16.0))
                    .pb(px(8.0))
                    .text_color(theme.text_muted)
                    .text_size(crate::typography::ui_rems(11.0))
                    .child("Loading locations…"),
            );
        }
        let results = command_palette::palette_results_fade(results, &scroll);

        // Breadcrumbs: one line that scrolls sideways under edge fades rather
        // than wrapping, and follows the open folder as the path grows. Deep
        // paths fold their middle folders into a `…` menu.
        let mut specs: Vec<Crumb> = vec![Crumb {
            name: "New project".into(),
            glyph: None,
            current: step == ProjectStep::Devices,
            target: CrumbTarget::Devices,
        }];
        let mut crumb_key = String::new();
        if let Some(device) = device {
            crumb_key.push_str(&device.id);
            specs.push(Crumb {
                name: device.name.into(),
                glyph: Some(device_glyph(&device.platform)),
                current: matches!(step, ProjectStep::Locations | ProjectStep::Repositories),
                target: CrumbTarget::Locations,
            });
        }
        let mut hidden: Vec<(String, String)> = Vec::new();
        // While the path is typed, the field stands in for the location and
        // folder crumbs; the device crumb stays to say where it is typed.
        if let Some((name, path)) = location.filter(|_| path_field.is_none()) {
            let root = path.clone().or(home.clone());
            // While a descend loads, keep showing the REQUESTED path so the
            // trail doesn't collapse to the location and pop back.
            let open_path = listing
                .as_ref()
                .map(|l| l.path.clone())
                .or(browser_path)
                .or(root.clone());
            let at_root = open_path.is_none()
                || open_path
                    .as_deref()
                    .zip(root.as_deref())
                    .is_some_and(|(open, root)| same_folder_path(open, root));
            crumb_key.push_str(&name);
            specs.push(Crumb {
                name: name.clone().into(),
                glyph: Some(if path.is_none() {
                    icons::HOME
                } else {
                    icons::HARD_DRIVE
                }),
                current: at_root,
                target: CrumbTarget::Location(name, path),
            });
            if let Some(open_path) = open_path {
                crumb_key.push_str(&open_path);
                let mut folders: Vec<(String, String)> = breadcrumbs(&open_path)
                    .into_iter()
                    .filter(|(_, full)| !root.as_deref().is_some_and(|root| path_under(root, full)))
                    .collect();
                hidden = fold_crumb_folders(&mut folders);
                if !hidden.is_empty() {
                    specs.push(Crumb {
                        name: "…".into(),
                        glyph: None,
                        current: false,
                        target: CrumbTarget::More,
                    });
                }
                for (name, full) in folders {
                    specs.push(Crumb {
                        name: name.into(),
                        glyph: None,
                        current: same_folder_path(&full, &open_path),
                        target: CrumbTarget::Folder(full),
                    });
                }
            }
        }
        if hidden.is_empty() && self.project_crumb_menu.get().is_some() {
            self.project_crumb_menu = popover::Popup::default();
        }
        let menu_open = self.project_crumb_menu.is_open();
        let menu_closing = self.project_crumb_menu.closing_since();
        let menu_mounted = self.project_crumb_menu.get().is_some();
        let mut trail: Vec<AnyElement> = Vec::new();
        for (ix, spec) in specs.into_iter().enumerate() {
            if ix > 0 {
                trail.push(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_faint)
                        .into_any_element(),
                );
            }
            let group: SharedString = format!("project-crumb-{ix}").into();
            let more = matches!(spec.target, CrumbTarget::More);
            let color = if spec.current || (more && menu_open) {
                theme.text
            } else {
                theme.text_muted
            };
            let long = spec.name.chars().count() > 26;
            let mut el = div()
                .id(group.clone())
                .group(group.clone())
                .relative()
                .flex_none()
                .h(px(24.0))
                .px(px(6.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .text_color(color)
                .when(more, |el| el.min_w(px(24.0)).justify_center())
                .when(more && menu_open, |el| el.bg(theme.element_hover))
                .when_some(spec.glyph, |el, glyph| {
                    el.child(
                        icon(glyph)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(color)
                            .group_hover(group.clone(), |s| s.text_color(theme.text)),
                    )
                })
                .child(div().max_w(px(180.0)).truncate().child(spec.name.clone()))
                .when(more || long, |el| {
                    el.tooltip(crate::settings::widgets::text_tooltip(if more {
                        SharedString::from("Show hidden folders")
                    } else {
                        spec.name.clone()
                    }))
                });
            // The open crumb is where you already are: no hover, no click.
            if !spec.current {
                el = el
                    .cursor_pointer()
                    .role(gpui::Role::Button)
                    .aria_label(if more {
                        SharedString::from("Show hidden folders")
                    } else {
                        spec.name.clone()
                    })
                    .hover(|s| s.bg(theme.element_hover).text_color(theme.text));
                el = match spec.target {
                    CrumbTarget::Devices => el.on_click(cx.listener(|this, _, _, cx| {
                        this.add_space_back_to(ProjectStep::Devices, cx)
                    })),
                    CrumbTarget::Locations => el.on_click(cx.listener(|this, _, _, cx| {
                        this.add_space_back_to(ProjectStep::Locations, cx)
                    })),
                    CrumbTarget::Location(name, path) => {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            this.add_space_goto_location(name.clone(), path.clone(), cx)
                        }))
                    }
                    CrumbTarget::Folder(full) => el.on_click(cx.listener(move |this, _, _, cx| {
                        this.add_space_descend(full.clone(), false, cx)
                    })),
                    CrumbTarget::More => el
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _, _, _| {
                                this.project_crumb_menu.note_trigger_press()
                            }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            // A press that found the menu open closes it.
                            if this.project_crumb_menu.take_press_was_open() {
                                this.close_project_crumb_menu(cx);
                            } else {
                                this.project_crumb_menu.open(());
                            }
                            cx.notify();
                        })),
                };
            }
            if more && menu_mounted {
                el = el.child(self.render_project_crumb_menu(&hidden, menu_closing, &theme, cx));
            }
            trail.push(el.into_any_element());
        }
        let crumb_scroll = self.add_space.as_mut().map(|flow| {
            // Reveal the open folder whenever the path changes; otherwise
            // leave the strip where the user scrolled it.
            if flow.crumb_key != crumb_key {
                flow.crumb_key = crumb_key;
                // An editable trail ends in the click-to-type spacer: reveal
                // through it, or the right fade stays on the open folder.
                flow.crumb_scroll
                    .scroll_to_item(trail.len().saturating_sub(usize::from(!can_edit_path)));
            }
            flow.crumb_scroll.clone()
        })?;
        let edit_path_label: SharedString =
            format!("Edit path {}", crate::settings::badge_combo("mod-l")).into();
        let trail_row = match &path_field {
            // Address-bar style: the device crumb, then the typed path with
            // its ↵ (or what it is waiting for) at the end.
            Some((input, opening, error)) => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .gap(px(2.0))
                .children(trail)
                .child(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .id("project-path")
                        .ml(px(4.0))
                        .flex_1()
                        .min_w_0()
                        .h(px(26.0))
                        .pl(px(8.0))
                        .pr(px(3.0))
                        .rounded(px(7.0))
                        .bg(crate::theme::ink(0.05))
                        .border_1()
                        .border_color(if error.is_some() {
                            theme.danger
                        } else {
                            theme.accent.opacity(0.6)
                        })
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            icon(icons::FOLDER)
                                .size(px(14.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h(px(16.0))
                                .overflow_hidden()
                                .child(input.clone()),
                        )
                        .child(if *opening {
                            div()
                                .flex_none()
                                .px(px(5.0))
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted)
                                .child("Opening…")
                                .into_any_element()
                        } else {
                            div()
                                .id("project-path-open")
                                .group("project-path-open")
                                .size(px(20.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .role(gpui::Role::Button)
                                .aria_label("Open this path")
                                .tooltip(crate::settings::widgets::text_tooltip("Open this path"))
                                .hover(|s| s.bg(crate::theme::wash(0.10)))
                                // It sits where the pen was: the second click
                                // of a double-click on the pen isn't a request.
                                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                                    if event.click_count() < 2 {
                                        this.add_space_open_typed_path(false, cx)
                                    }
                                }))
                                .child(
                                    icon(icons::RETURN)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .group_hover("project-path-open", |s| {
                                            s.text_color(theme.text)
                                        }),
                                )
                                .into_any_element()
                        }),
                )
                .into_any_element(),
            None => crate::edge_fade::edge_faded(
                18.0,
                false,
                false,
                div()
                    .id("project-crumbs")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .overflow_x_scroll()
                    .track_scroll(&crumb_scroll)
                    .children(trail)
                    // The row's empty end types the path instead, like a
                    // file manager's address bar.
                    .when(can_edit_path, |trail| {
                        trail.child(
                            div()
                                .id("project-crumb-edit")
                                .flex_1()
                                .min_w(px(24.0))
                                .h_full()
                                .cursor(gpui::CursorStyle::IBeam)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.add_space_edit_path(cx)),
                                ),
                        )
                    }),
            )
            .fade_left(true)
            .fade_right(true)
            .fade_overflow_x(&crumb_scroll)
            .into_any_element(),
        };
        // ← mirrors the Left key: up one level, and back to Cmd+K from the
        // first step.
        let back_label = if step == ProjectStep::Devices {
            "Back to commands"
        } else {
            "Back"
        };
        let crumbs = div()
            .h(px(36.0))
            .flex_none()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.06))
            .text_size(crate::typography::ui_rems(12.0))
            .child(
                div()
                    .id("project-crumb-back")
                    .group("project-crumb-back")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .role(gpui::Role::Button)
                    .aria_label(back_label)
                    .tooltip(crate::settings::widgets::text_tooltip(back_label))
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.add_space.as_ref().map(|f| f.step) == Some(ProjectStep::Devices) {
                            this.close_add_space(cx);
                            this.toggle_command_palette(window, cx);
                        } else {
                            this.add_space_go_up(cx);
                        }
                    }))
                    .child(
                        icon(icons::ARROW_LEFT)
                            .size(px(16.0))
                            .text_color(theme.text_muted)
                            .group_hover("project-crumb-back", |s| s.text_color(theme.text)),
                    ),
            )
            .child(trail_row)
            .when(can_edit_path && path_field.is_none(), |row| {
                row.child(
                    div()
                        .id("project-path-edit")
                        .group("project-path-edit")
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("Edit path")
                        .tooltip(crate::settings::widgets::text_tooltip(
                            edit_path_label.clone(),
                        ))
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| this.add_space_edit_path(cx)))
                        .child(
                            icon(icons::PEN)
                                .size(px(14.0))
                                .text_color(theme.text_muted)
                                .group_hover("project-path-edit", |s| s.text_color(theme.text)),
                        ),
                )
            });
        let path_error = path_field.as_ref().and_then(|(_, _, error)| error.clone());

        let shortcut = {
            let id = ShortcutId::NewProject;
            let combo = self.settings.keymap.get(id);
            let valid = Keystroke::parse(&platform_combo(combo)).is_ok();
            crate::settings::badge_combo(if valid { combo } else { id.default_combo() })
        };
        // While the path is typed, ⌘↵ and the button add the TYPED path.
        let editing = path_field.is_some();
        let can_add = !busy
            && match &path_field {
                Some((input, opening, _)) => !opening && !input.read(cx).text().trim().is_empty(),
                None => listing.is_some(),
            };
        let footer = if editing {
            command_palette::palette_footer()
                .child(command_palette::command_key_hint(
                    &theme,
                    if cfg!(target_os = "macos") {
                        "⇥"
                    } else {
                        "Tab"
                    },
                    "Complete",
                ))
                .child(command_palette::command_key_hint(&theme, "↵", "Open"))
                .child(command_palette::command_key_hint(&theme, "Esc", "Cancel"))
        } else {
            command_palette::palette_footer()
                .child(command_palette::command_key_hint(&theme, "↑ ↓", "Navigate"))
                .child(command_palette::command_key_hint(
                    &theme,
                    "↵",
                    match step {
                        ProjectStep::Folders => "Open",
                        ProjectStep::Repositories => "Add",
                        _ => "Select",
                    },
                ))
                .when(step != ProjectStep::Devices, |el| {
                    el.child(command_palette::command_key_hint(&theme, "←", "Back"))
                })
                .child(command_palette::command_key_hint(&theme, "Esc", "Close"))
                .when(can_edit_path, |el| {
                    el.child(command_palette::command_key_hint(
                        &theme,
                        &crate::settings::badge_combo("mod-l"),
                        "Edit path",
                    ))
                })
        }
        .when(step == ProjectStep::Folders || editing, |el| {
            el.child(div().flex_1()).child(
                div()
                    .id("project-add")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    // Even 3px around the key chip, concentric corners
                    // (chip 5px + 3px); negative margins keep the footer
                    // height and the label on the footer's right inset.
                    .pl(px(3.0))
                    .pr(px(8.0))
                    .py(px(3.0))
                    .my(px(-3.0))
                    .mr(px(-8.0))
                    .rounded(px(8.0))
                    .role(gpui::Role::Button)
                    .aria_label("Add project")
                    .when(can_add, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.bg(crate::theme::card_selected_bg()))
                            .active(|s| s.opacity(0.8))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if editing {
                                    this.add_space_open_typed_path(true, cx)
                                } else {
                                    this.submit_add_space(cx)
                                }
                            }))
                    })
                    .when(!can_add, |el| el.opacity(0.5))
                    .child(popover::kbd_hint(
                        &theme,
                        &crate::settings::badge_combo("mod-enter"),
                    ))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(match (&cloning, busy) {
                                (Some(label), _) => format!("Cloning {label}…"),
                                (None, true) => "Adding…".to_string(),
                                (None, false) => "Add project".to_string(),
                            })),
                    ),
            )
        });
        let card =
            command_palette::palette_card("add-space-palette", &focus, viewport, &theme)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    this.add_space_key(event, cx)
                }))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    // The crumb menu floats outside the card; its own
                    // mouse-down-out dismisses it without closing the palette.
                    if this.project_crumb_menu.get().is_some() {
                        return;
                    }
                    this.close_add_space(cx);
                }))
                .child(command_palette::palette_header(
                    &theme,
                    search.into_any_element(),
                    popover::kbd_hint(&theme, &shortcut),
                ))
                .child(crumbs)
                // Why the typed path didn't open, right under the field; the
                // listing below is still the folder that was open.
                .when_some(path_error, |el, error| {
                    el.child(
                        div()
                            .id("project-path-error")
                            .px(px(16.0))
                            .pt(px(8.0))
                            .pb(px(2.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(results)
                .when_some(error, |el, error| {
                    el.child(
                        div()
                            .px(px(16.0))
                            .pb(px(8.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(footer);
        Some(command_palette::palette_overlay(viewport, card))
    }

    // ---- space context menu / rename / delete overlays ----

    fn close_space_menu(&mut self, cx: &mut Context<Self>) {
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.space_menu);
            cx.notify();
        }
    }

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.close_space_menu(cx);
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Project name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.get().cloned() {
            let closing = self.space_menu.closing_since();
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_space_menu(cx);
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_space_menu(cx);
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at(
                "space-context-menu",
                position,
                menu,
                closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                        cx.stop_propagation();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename project"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this project".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove project?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}

/// Synthetic responses for the isolated native screenshot fixture only.
#[cfg(feature = "project-palette-fixture")]
impl Shell {
    pub fn fixture_project_responses(&mut self, cx: &mut Context<Self>) {
        if std::env::var_os("ZERON_FIXTURE_BACKGROUND").is_some() {
            self.composer
                .read(cx)
                .pickers()
                .clone()
                .update(cx, |pickers, cx| pickers.fixture_model_catalog(cx));
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.is_some() && !matches!(flow.drives, Loadable::Ready(_)) {
            flow.drives = Loadable::Ready(
                serde_json::from_value(serde_json::json!([
                    {"name":"Projects", "path":"/projects"},
                    {"name":"System", "path":"/"}
                ]))
                .unwrap(),
            );
            cx.notify();
        }
        if flow.step == ProjectStep::Folders && !matches!(flow.browser, Loadable::Ready(_)) {
            let path = flow
                .browser_path
                .clone()
                .unwrap_or_else(|| "/home/alex".into());
            if flow.browser_path.is_none() {
                flow.home = Some(path.clone());
            }
            let names = match path.as_str() {
                "/home/alex" => vec![
                    "Desktop",
                    "Documents",
                    "Downloads",
                    "Movies",
                    "Music",
                    "Pictures",
                    "Projects",
                    "Public",
                    "dotfiles",
                    "notes",
                    "sandbox",
                    "scratch",
                ],
                "/projects" | "/home/alex/Projects" => vec!["fieldnotes", "mobile-app", "website"],
                _ => vec!["assets", "docs", "src", "tests"],
            };
            flow.browser = Loadable::Ready(
                serde_json::from_value(serde_json::json!({
                    "path":path,
                    "entries":names.into_iter().map(|name| serde_json::json!({
                        "name":name,"isDir":true,"isRepo":name=="fieldnotes"
                    })).collect::<Vec<_>>()
                }))
                .unwrap(),
            );
            cx.notify();
        }
    }
}

/// The typed-path capture's typing target and read-outs
/// (`examples/new-project-path-fixture.rs`).
#[cfg(feature = "project-path-fixture")]
impl Shell {
    /// The New project input holding the keyboard — the path field or the
    /// search — to type into through its input handler, as the platform does.
    pub fn fixture_project_input(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<ComposerInput>> {
        let flow = self.add_space.as_ref()?;
        flow.path_edit
            .as_ref()
            .map(|edit| edit.input.clone())
            .into_iter()
            .chain([flow.search.clone()])
            .find(|input| input.focus_handle(cx).is_focused(window))
    }

    /// Where the New project palette stands; `None` while it is closed.
    pub fn fixture_project_summary(&self, cx: &App) -> Option<String> {
        let flow = self.add_space.as_ref()?;
        let edit = flow.path_edit.as_ref();
        Some(format!(
            "step={:?} open={:?} field={:?} error={:?}",
            flow.step,
            flow.browser.ready().map(|listing| listing.path.as_str()),
            edit.map(|edit| edit.input.read(cx).text().to_owned()),
            edit.and_then(|edit| edit.error.as_deref()),
        ))
    }
}

/// The drawer's "..." menu: Clean Chat and Settings.
#[cfg(test)]
mod drawer_menu_tests {
    use super::*;

    fn shell(cx: &mut gpui::TestAppContext, path: &std::path::Path) -> gpui::WindowHandle<Shell> {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: path.into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        })
    }

    /// The menu builds (it panicked when a row took two hover listeners) and
    /// Clean Chat asks before it deletes.
    #[gpui::test]
    fn the_drawer_menu_builds_and_clean_chat_asks_first(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let window = shell(cx, dir.path());
        window
            .update(cx, |shell, window, cx| {
                shell.open_sidebar_view_menu(window, cx);
                let theme = Theme::of(cx).clone();
                let _menu = shell.render_sidebar_view_menu(&theme, cx);
                assert!(!shell.clean_chat_armed);
                shell.activate_clean_chat(cx);
                assert!(shell.clean_chat_armed, "the first click only arms it");
                let _armed = shell.render_sidebar_view_menu(&theme, cx);
                shell.close_sidebar_view_menu(cx);
                assert!(!shell.clean_chat_armed, "closing the menu disarms it");
            })
            .unwrap();
    }
}
