//! "Cameras" window: camera list CRUD, connection test, mosaic slot assignment, preferences.

use cam_config::{CameraConfig, CameraId, DEFAULT_RTSP_PORT, SLOT_COUNT, Secret, StreamKind, validate_camera};
use cam_stream::ProbeInfo;
use gpui::{
    App, Bounds, Context, Div, Entity, FocusHandle, Focusable, Global, KeyBinding, SharedString, Subscription, Task,
    TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, actions, div, prelude::*, px, size,
};

use crate::state::AppState;
use crate::ui::components::{ButtonKind, Dropdown, DropdownEvent, DropdownItem, TextInput, TextInputEvent, button, checkbox};
use crate::ui::theme;

actions!(settings, [CloseWindow, FocusNext, FocusPrev]);

const KEY_CONTEXT: &str = "Settings";
const SLOT_LABELS: [&str; SLOT_COUNT] = ["Top left", "Top right", "Bottom left", "Bottom right"];

struct SettingsWindow(WindowHandle<SettingsView>);
impl Global for SettingsWindow {}

struct KeysBound;
impl Global for KeysBound {}

/// Opens the settings window, or focuses it if it is already open.
pub fn open(cx: &mut App) {
    if !cx.has_global::<KeysBound>() {
        let ctx = Some(KEY_CONTEXT);
        cx.bind_keys([
            KeyBinding::new("cmd-w", CloseWindow, ctx),
            KeyBinding::new("tab", FocusNext, ctx),
            KeyBinding::new("shift-tab", FocusPrev, ctx),
        ]);
        cx.set_global(KeysBound);
    }

    if let Some(handle) = cx.try_global::<SettingsWindow>().map(|w| w.0)
        && handle.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        cx.activate(true);
        return;
    }

    let bounds = Bounds::centered(None, size(px(780.), px(560.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions { title: Some("Cameras".into()), ..Default::default() }),
        window_min_size: Some(size(px(680.), px(480.))),
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| cx.new(|cx| SettingsView::new(window, cx))) {
        Ok(handle) => {
            cx.set_global(SettingsWindow(handle));
            cx.activate(true);
        }
        Err(err) => tracing::error!(%err, "failed to open settings window"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// Form for a camera that does not exist yet; the id is fixed so probe/save agree.
    New(CameraId),
    Existing(CameraId),
}

impl Selection {
    fn id(self) -> CameraId {
        match self {
            Selection::New(id) | Selection::Existing(id) => id,
        }
    }
}

enum Probe {
    Idle,
    Running,
    Done(Result<ProbeInfo, String>),
}

pub struct SettingsView {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    selection: Selection,
    /// What the form was last loaded from (or saved as); the form is dirty when it differs.
    loaded: CameraConfig,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    stream: StreamKind,
    slot_dropdowns: Vec<Entity<Dropdown>>,
    /// Camera id behind each dropdown row (`None` = "No camera"); shared by the four slots.
    slot_options: Vec<Option<CameraId>>,
    form_error: Option<SharedString>,
    slot_error: Option<SharedString>,
    login_error: Option<SharedString>,
    smooth_error: Option<SharedString>,
    probe: Probe,
    probe_task: Option<Task<()>>,
    confirm_remove: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = AppState::global(cx);
        let name = cx.new(|cx| TextInput::new(cx, "e.g. Garage", 1));
        let host = cx.new(|cx| TextInput::new(cx, "192.168.1.100", 2));
        let port = cx.new(|cx| TextInput::new(cx, DEFAULT_RTSP_PORT.to_string(), 3).digits_only(true).max_len(5));
        let username = cx.new(|cx| TextInput::new(cx, "admin", 4));
        let password = cx.new(|cx| TextInput::new(cx, "", 5).masked(true));
        let slot_dropdowns: Vec<_> =
            (0..SLOT_COUNT).map(|i| cx.new(|cx| Dropdown::new(cx, vec![DropdownItem::new("No camera")], 0, 20 + i as isize))).collect();

        let mut subscriptions = Vec::new();
        for input in [&name, &host, &port, &username, &password] {
            subscriptions.push(cx.subscribe(input, |this, _, event, cx| match event {
                TextInputEvent::Change => {
                    this.form_error = None;
                    cx.notify();
                }
                TextInputEvent::Submit => this.save(cx),
            }));
        }
        for (slot, dropdown) in slot_dropdowns.iter().enumerate() {
            subscriptions.push(cx.subscribe(dropdown, move |this, _, event, cx| {
                let DropdownEvent::Selected(index) = *event;
                this.assign_slot(slot, index, cx);
            }));
        }
        subscriptions.push(cx.observe(&state, |this, _, cx| this.on_state_changed(cx)));

        let mut this = Self {
            state,
            focus_handle: cx.focus_handle(),
            selection: Selection::New(CameraId::new()),
            loaded: CameraConfig::new("", ""),
            name,
            host,
            port,
            username,
            password,
            stream: StreamKind::Main,
            slot_dropdowns,
            slot_options: vec![None],
            form_error: None,
            slot_error: None,
            login_error: None,
            smooth_error: None,
            probe: Probe::Idle,
            probe_task: None,
            confirm_remove: false,
            _subscriptions: subscriptions,
        };
        match this.sorted_cameras(cx).first().map(|c| c.id) {
            Some(id) => this.select(id, cx),
            None => this.new_camera(cx),
        }
        this.rebuild_slot_dropdowns(cx);
        window.focus(&this.name.read(cx).focus_handle(cx), cx);
        this
    }

    // --- data ---------------------------------------------------------------------------------

    fn sorted_cameras(&self, cx: &App) -> Vec<CameraConfig> {
        let mut cameras = self.state.read(cx).config().cameras.clone();
        cameras.sort_by_cached_key(|c| c.name.trim().to_lowercase());
        cameras
    }

    fn stored_camera(&self, cx: &App) -> Option<CameraConfig> {
        match self.selection {
            Selection::Existing(id) => self.state.read(cx).config().camera(id).cloned(),
            Selection::New(_) => None,
        }
    }

    /// The camera as described by the form right now (may be invalid).
    fn form_camera(&self, cx: &App) -> CameraConfig {
        let port_text = self.port.read(cx).text().trim().to_string();
        let port = if port_text.is_empty() { DEFAULT_RTSP_PORT } else { port_text.parse().unwrap_or(0) };
        CameraConfig {
            id: self.selection.id(),
            name: self.name.read(cx).text().trim().to_string(),
            host: self.host.read(cx).text().trim().to_string(),
            port,
            stream: self.stream,
            username: self.username.read(cx).text().trim().to_string(),
            password: Secret::new(self.password.read(cx).text()),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.form_camera(cx) != self.loaded
    }

    /// Fills the form from `camera` (a blank camera when `None`) and makes it the clean baseline.
    fn load_form(&mut self, camera: Option<&CameraConfig>, cx: &mut Context<Self>) {
        let (name, host, port, username, password, stream) = match camera {
            Some(c) => (c.name.clone(), c.host.clone(), c.port.to_string(), c.username.clone(), c.password.expose().to_string(), c.stream),
            None => (String::new(), String::new(), String::new(), String::new(), String::new(), StreamKind::Main),
        };
        self.loaded = match camera {
            Some(c) => c.clone(),
            None => {
                let mut blank = CameraConfig::new("", "");
                blank.id = self.selection.id();
                blank
            }
        };
        self.name.update(cx, |input, cx| input.set_text(name, cx));
        self.host.update(cx, |input, cx| input.set_text(host, cx));
        self.port.update(cx, |input, cx| input.set_text(port, cx));
        self.username.update(cx, |input, cx| input.set_text(username, cx));
        self.password.update(cx, |input, cx| {
            input.set_text(password, cx);
            input.set_masked(true, cx);
        });
        self.stream = stream;
        self.form_error = None;
        self.probe = Probe::Idle;
        self.probe_task = None;
        self.confirm_remove = false;
        cx.notify();
    }

    fn select(&mut self, id: CameraId, cx: &mut Context<Self>) {
        let Some(camera) = self.state.read(cx).config().camera(id).cloned() else {
            return;
        };
        self.selection = Selection::Existing(id);
        self.load_form(Some(&camera), cx);
    }

    fn new_camera(&mut self, cx: &mut Context<Self>) {
        self.selection = Selection::New(CameraId::new());
        self.load_form(None, cx);
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        self.rebuild_slot_dropdowns(cx);
        match self.selection {
            Selection::Existing(id) if self.state.read(cx).config().camera(id).is_none() => {
                // Removed elsewhere.
                match self.sorted_cameras(cx).first().map(|c| c.id) {
                    Some(id) => self.select(id, cx),
                    None => self.new_camera(cx),
                }
            }
            Selection::Existing(id) => {
                // Changed elsewhere (e.g. hand-edited file reloaded): refresh unless the user is
                // mid-edit, in which case their edits win.
                let stored = self.state.read(cx).config().camera(id).cloned();
                if let Some(stored) = stored
                    && stored != self.loaded
                    && !self.is_dirty(cx)
                {
                    self.load_form(Some(&stored), cx);
                }
            }
            Selection::New(_) => {}
        }
        cx.notify();
    }

    fn rebuild_slot_dropdowns(&mut self, cx: &mut Context<Self>) {
        let config = self.state.read(cx).config().clone();
        let cameras = self.sorted_cameras(cx);
        self.slot_options = std::iter::once(None).chain(cameras.iter().map(|c| Some(c.id))).collect();
        for (slot, dropdown) in self.slot_dropdowns.iter().enumerate() {
            let items = std::iter::once(DropdownItem::new("No camera"))
                .chain(cameras.iter().map(|c| {
                    let item = DropdownItem::new(c.name.clone());
                    match config.slot_of(c.id) {
                        Some(other) if other != slot => item.hint(format!("(in {})", SLOT_LABELS[other])),
                        _ => item,
                    }
                }))
                .collect();
            let selected = config.slots[slot].and_then(|id| self.slot_options.iter().position(|o| *o == Some(id))).unwrap_or(0);
            dropdown.update(cx, |dropdown, cx| dropdown.set_items(items, selected, cx));
        }
    }

    // --- actions ------------------------------------------------------------------------------

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.is_dirty(cx) {
            return;
        }
        let camera = self.form_camera(cx);
        let result = self.state.update(cx, |state, cx| state.update_config(cx, |config| config.upsert_camera(camera.clone())));
        match result {
            Ok(()) => {
                self.selection = Selection::Existing(camera.id);
                self.loaded = camera;
                self.form_error = None;
                self.confirm_remove = false;
            }
            Err(err) => self.form_error = Some(err.to_string().into()),
        }
        cx.notify();
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if matches!(self.probe, Probe::Running) {
            return;
        }
        let camera = self.form_camera(cx);
        if let Err(err) = validate_camera(&camera) {
            self.form_error = Some(err.to_string().into());
            cx.notify();
            return;
        }
        let future = self.state.read(cx).streams.probe(camera);
        self.form_error = None;
        self.probe = Probe::Running;
        self.probe_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(future).await;
            this.update(cx, |this, cx| {
                this.probe = Probe::Done(result);
                this.probe_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Selection::Existing(id) = self.selection else {
            return;
        };
        if !self.confirm_remove {
            self.confirm_remove = true;
            cx.notify();
            return;
        }
        let result = self.state.update(cx, |state, cx| {
            state.update_config(cx, |config| {
                config.remove_camera(id);
                Ok(())
            })
        });
        match result {
            Ok(()) => match self.sorted_cameras(cx).first().map(|c| c.id) {
                Some(next) => self.select(next, cx),
                None => self.new_camera(cx),
            },
            Err(err) => {
                self.confirm_remove = false;
                self.form_error = Some(err.to_string().into());
            }
        }
        cx.notify();
    }

    fn assign_slot(&mut self, slot: usize, option_index: usize, cx: &mut Context<Self>) {
        let camera = self.slot_options.get(option_index).copied().flatten();
        let result = self.state.update(cx, |state, cx| {
            state.update_config(cx, |config| {
                config.assign(slot, camera);
                Ok(())
            })
        });
        self.slot_error = result.err().map(|err| err.to_string().into());
        // Even on error the dropdown must show the persisted value again.
        self.rebuild_slot_dropdowns(cx);
        cx.notify();
    }

    fn set_open_at_login(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let result = cam_platform::set_open_at_login(enabled).map_err(|err| err.to_string()).and_then(|()| {
            self.state
                .update(cx, |state, cx| {
                    state.update_config(cx, |config| {
                        config.open_at_login = enabled;
                        Ok(())
                    })
                })
                .map_err(|err| err.to_string())
        });
        self.login_error = result.err().map(SharedString::from);
        cx.notify();
    }

    // --- rendering ----------------------------------------------------------------------------

    fn render_camera_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let cameras = self.sorted_cameras(cx);
        let selected = match self.selection {
            Selection::Existing(id) => Some(id),
            Selection::New(_) => None,
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(230.))
            .h_full()
            .border_r_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .child(section_title("Cameras").px_3().pt_3().pb_2())
            .child(
                div()
                    .id("camera-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .when(cameras.is_empty(), |this| {
                        this.child(div().px_2().py_1().text_color(theme::text_muted()).child("No cameras added yet."))
                    })
                    .children(cameras.into_iter().enumerate().map(|(ix, camera)| {
                        let id = camera.id;
                        let is_selected = selected == Some(id);
                        div()
                            .id(("camera", ix))
                            .flex()
                            .flex_col()
                            .px_2()
                            .py(px(6.))
                            .mb(px(2.))
                            .rounded_md()
                            .cursor_pointer()
                            .border_l_2()
                            .border_color(if is_selected { theme::accent() } else { gpui::rgba(0x00000000) })
                            .when(is_selected, |this| this.bg(theme::surface_hover()))
                            .hover(|s| s.bg(theme::surface_hover()))
                            .child(div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(camera.name.clone()))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text_muted())
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(format!("{}:{} · {}", camera.host, camera.port, camera.stream.label())),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                    })),
            )
            .child(
                div().p_3().border_t_1().border_color(theme::border()).child(
                    button("new-camera", "+ New Camera")
                        .disabled(matches!(self.selection, Selection::New(_)) && !self.is_dirty(cx))
                        .on_click(cx.listener(|this, _, _, cx| this.new_camera(cx))),
                ),
            )
    }

    fn render_form(&self, cx: &Context<Self>) -> impl IntoElement {
        let is_new = matches!(self.selection, Selection::New(_));
        let dirty = self.is_dirty(cx);
        let masked = self.password.read(cx).is_masked();
        let title: SharedString = match self.stored_camera(cx) {
            Some(camera) => camera.name.into(),
            None => "New Camera".into(),
        };
        let probe_status = match &self.probe {
            Probe::Idle => None,
            Probe::Running => Some((theme::text_muted(), "Testing…".to_string())),
            Probe::Done(Ok(info)) => {
                let fps = info.fps.map(|fps| format!(" · {fps:.0} fps")).unwrap_or_default();
                Some((theme::success(), format!("✓ {} {}×{}{}", info.codec, info.width, info.height, fps)))
            }
            Probe::Done(Err(err)) => Some((theme::danger(), format!("✗ {err}"))),
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(title))
            .child(field("Name", self.name.clone()))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Address (IP or hostname)", self.host.clone())))
                    .child(div().w(px(90.)).child(field("Port", self.port.clone()))),
            )
            .child(
                div().flex().flex_col().gap_1().child(label("Stream")).child(
                    div()
                        .flex()
                        .rounded_md()
                        .border_1()
                        .border_color(theme::border())
                        .overflow_hidden()
                        .child(self.render_stream_option("stream-main", StreamKind::Main, cx))
                        .child(self.render_stream_option("stream-sub", StreamKind::Sub, cx)),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field("Username", self.username.clone())))
                    .child(
                        div().flex_1().child(
                            field(
                                "Password",
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().flex_1().child(self.password.clone()))
                                    .child(button("toggle-password", if masked { "Show" } else { "Hide" }).on_click(cx.listener(
                                        |this, _, _, cx| this.password.update(cx, |input, cx| input.set_masked(!input.is_masked(), cx)),
                                    ))),
                            ),
                        ),
                    ),
            )
            .when_some(self.form_error.clone(), |this, err| this.child(div().text_color(theme::danger()).child(err)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button("test", "Test Connection")
                            .tab_index(6)
                            .disabled(matches!(self.probe, Probe::Running))
                            .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                    )
                    .when_some(probe_status, |this, (color, text)| {
                        this.child(div().flex_1().text_size(px(12.)).text_color(color).child(text))
                    })
                    .child(div().flex_1())
                    .when(!is_new, |this| {
                        let label = if self.confirm_remove { "Confirm Removal" } else { "Remove" };
                        this.child(
                            button("remove", label)
                                .kind(ButtonKind::Danger)
                                .tab_index(8)
                                .on_click(cx.listener(|this, _, _, cx| this.remove(cx))),
                        )
                    })
                    .child(
                        button("save", "Save")
                            .kind(ButtonKind::Primary)
                            .tab_index(7)
                            .disabled(!dirty)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }

    fn render_stream_option(&self, id: &'static str, kind: StreamKind, cx: &Context<Self>) -> impl IntoElement {
        let active = self.stream == kind;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .h(px(28.))
            .px_4()
            .cursor_pointer()
            .bg(if active { theme::accent() } else { theme::background() })
            .text_color(if active { theme::text() } else { theme::text_muted() })
            .hover(move |s| if active { s } else { s.bg(theme::surface_hover()) })
            .child(kind.label())
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.stream != kind {
                    this.stream = kind;
                    this.form_error = None;
                    cx.notify();
                }
            }))
    }

    fn render_mosaic(&self) -> impl IntoElement {
        let row = |a: usize, b: usize| {
            div()
                .flex()
                .gap_3()
                .child(div().flex_1().child(field(SLOT_LABELS[a], self.slot_dropdowns[a].clone())))
                .child(div().flex_1().child(field(SLOT_LABELS[b], self.slot_dropdowns[b].clone())))
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("Layout"))
            .child(row(0, 1))
            .child(row(2, 3))
            .when_some(self.slot_error.clone(), |this, err| this.child(div().text_color(theme::danger()).child(err)))
    }

    fn set_smooth_playback(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let result = self.state.update(cx, |state, cx| {
            state.update_config(cx, |config| {
                config.smooth_playback = enabled;
                Ok(())
            })
        });
        self.smooth_error = result.err().map(|err| err.to_string().into());
        cx.notify();
    }

    fn render_preferences(&self, cx: &Context<Self>) -> impl IntoElement {
        let config = self.state.read(cx).config();
        let (enabled, smooth) = (config.open_at_login, config.smooth_playback);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("Preferences"))
            .child(
                checkbox(
                    "open-at-login",
                    "Open at login",
                    enabled,
                    cx.listener(|this, value: &bool, _, cx| this.set_open_at_login(*value, cx)),
                )
                .tab_index(30),
            )
            .when_some(self.login_error.clone(), |this, err| this.child(div().text_color(theme::danger()).child(err)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        checkbox(
                            "smooth-playback",
                            "Smooth playback",
                            smooth,
                            cx.listener(|this, value: &bool, _, cx| this.set_smooth_playback(*value, cx)),
                        )
                        .tab_index(31),
                    )
                    .child(
                        div()
                            .w_full()
                            .pl(px(24.))
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child("Plays frames at the camera's pace, absorbing Wi-Fi jitter. Adds ~150 ms of latency."),
                    ),
            )
            .when_some(self.smooth_error.clone(), |this, err| this.child(div().text_color(theme::danger()).child(err)))
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrev, window, cx| window.focus_prev(cx))
            .flex()
            .size_full()
            .bg(theme::background())
            .text_color(theme::text())
            .text_size(px(13.))
            .child(self.render_camera_list(cx))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    // Without this a flex child's min width is its unwrapped content width, so long
                    // help text widens the column past the window instead of wrapping.
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .p_4()
                            .child(self.render_form(cx))
                            .child(divider())
                            .child(self.render_mosaic())
                            .child(divider())
                            .child(self.render_preferences(cx)),
                    ),
            )
    }
}

fn section_title(text: impl Into<SharedString>) -> Div {
    div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).child(text.into())
}

fn label(text: impl Into<SharedString>) -> Div {
    div().text_size(px(12.)).text_color(theme::text_muted()).child(text.into())
}

fn field(text: impl Into<SharedString>, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap_1().child(label(text)).child(control)
}

fn divider() -> Div {
    div().h(px(1.)).w_full().bg(theme::border())
}
