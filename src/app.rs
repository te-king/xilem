// Copyright 2022 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use accesskit::TreeUpdate;
use glazier::kurbo::Size;
use glazier::{IdleHandle, IdleToken, WindowHandle};
use parley::FontContext;
use tokio::runtime::Runtime;
use vello::{SceneBuilder, SceneFragment};

use crate::event::{AsyncWake, MessageResult};
use crate::id::IdPath;
use crate::widget::{
    AccessCx, BoxConstraints, CxState, EventCx, LayoutCx, PaintCx, Pod, UpdateCx, WidgetState,
};
use crate::{
    event::Message,
    id::Id,
    view::{Cx, View},
    widget::{Event, Widget},
};

pub struct App<T, V: View<T>> {
    req_chan: tokio::sync::mpsc::Sender<AppReq>,
    response_chan: tokio::sync::mpsc::Receiver<RenderResponse<V, V::State>>,
    return_chan: tokio::sync::mpsc::Sender<(V, V::State, HashSet<Id>)>,
    id: Option<Id>,
    events: Vec<Message>,
    window_handle: WindowHandle,
    root_state: WidgetState,
    root_pod: Option<Pod>,
    size: Size,
    cx: Cx,
    font_cx: FontContext,
    pub(crate) rt: Runtime,
    // This is allocated an id for AccessKit, but as we get multi-window,
    // there should be a real window object with id.
    window_id: Id,
    pub(crate) accesskit_connected: bool,
}

/// The standard delay for waiting for async futures.
const RENDER_DELAY: Duration = Duration::from_millis(5);

/// State that's kept in a separate task for running the app
struct AppTask<T, V: View<T>, F: FnMut(&mut T) -> V> {
    req_chan: tokio::sync::mpsc::Receiver<AppReq>,
    response_chan: tokio::sync::mpsc::Sender<RenderResponse<V, V::State>>,
    return_chan: tokio::sync::mpsc::Receiver<(V, V::State, HashSet<Id>)>,

    data: T,
    app_logic: F,
    view: Option<V>,
    state: Option<V::State>,
    idle_handle: Option<IdleHandle>,
    pending_async: HashSet<Id>,
    ui_state: UiState,
}

/// A message sent from the main UI thread to the app task
pub(crate) enum AppReq {
    SetIdleHandle(IdleHandle),
    Events(Vec<Message>),
    Wake(IdPath),
    // Parameter indicates whether it should be delayed for async
    Render(bool),
}

/// A response sent to a render request.
struct RenderResponse<V, S> {
    prev: Option<V>,
    view: V,
    state: Option<S>,
}

#[derive(PartialEq)]
enum UiState {
    /// Starting state, ready for events and render requests.
    Start,
    /// Received render request, haven't responded yet.
    Delayed,
    /// An async completion woke the UI thread.
    WokeUI,
}

#[derive(Clone, Default)]
pub struct WakeQueue(Arc<Mutex<Vec<IdPath>>>);

impl<T: Send + 'static, V: View<T> + 'static> App<T, V>
where
    V::Element: Widget + 'static,
    V::State: 'static,
{
    /// Create a new app instance.
    pub fn new(data: T, app_logic: impl FnMut(&mut T) -> V + Send + 'static) -> Self {
        // Create a new tokio runtime. Doing it here is hacky, we should allow
        // the client to do it.
        let rt = Runtime::new().unwrap();

        // Note: there is danger of deadlock if exceeded; think this through.
        const CHANNEL_SIZE: usize = 1000;
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(CHANNEL_SIZE);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(1);
        let (return_tx, return_rx) = tokio::sync::mpsc::channel(1);

        // We have a separate thread to forward wake requests (mostly generated
        // by the custom waker when we poll) to the async task. Maybe there's a
        // better way, but this is expedient.
        //
        // It's a sync_channel because sender needs to be sync to work in an async
        // context. Consider crossbeam and flume channels as alternatives.
        let req_tx_clone = req_tx.clone();
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(10);
        std::thread::spawn(move || {
            while let Ok(id_path) = wake_rx.recv() {
                let _ = req_tx_clone.blocking_send(AppReq::Wake(id_path));
            }
        });
        let cx = Cx::new(&wake_tx);

        // spawn app task
        rt.spawn(async move {
            let mut app_task = AppTask {
                req_chan: req_rx,
                response_chan: response_tx,
                return_chan: return_rx,
                data,
                app_logic,
                view: None,
                state: None,
                idle_handle: None,
                pending_async: HashSet::new(),
                ui_state: UiState::Start,
            };
            app_task.run().await;
        });
        App {
            req_chan: req_tx,
            response_chan: response_rx,
            return_chan: return_tx,
            id: None,
            root_pod: None,
            events: Vec::new(),
            window_handle: Default::default(),
            root_state: WidgetState::new(),
            size: Default::default(),
            cx,
            font_cx: FontContext::new(),
            rt,
            window_id: Id::next(),
            accesskit_connected: false,
        }
    }

    pub fn connect(&mut self, window_handle: WindowHandle) {
        self.window_handle = window_handle.clone();
        if let Some(idle_handle) = window_handle.get_idle_handle() {
            let _ = self
                .req_chan
                .blocking_send(AppReq::SetIdleHandle(idle_handle));
        }
    }

    pub fn size(&mut self, size: Size) {
        self.size = size;
    }

    pub fn accessibility(&mut self) -> TreeUpdate {
        let mut update = TreeUpdate::default();
        let root_pod = self.root_pod.as_mut().unwrap();
        let mut window_node = accesskit::Node {
            role: accesskit::Role::Window,
            children: vec![root_pod.id().into()],
            name: Some("xilem window".into()),
            ..Default::default()
        };
        if let Ok(scale) = self.window_handle.get_scale() {
            window_node.transform = Some(Box::new(accesskit::kurbo::Affine::scale_non_uniform(
                scale.x(),
                scale.y(),
            )));
        }
        update
            .nodes
            .push((self.window_id.into(), Arc::new(window_node)));
        update.tree = Some(accesskit::Tree::new(self.window_id.into()));
        let mut cx_state = CxState::new(&self.window_handle, &mut self.font_cx, &mut self.events);
        let mut access_cx = AccessCx {
            cx_state: &mut cx_state,
            widget_state: &mut &mut self.root_state,
            update: &mut update,
        };
        root_pod.accessibility(&mut access_cx);
        update
    }

    /// Run a paint cycle for the application.
    ///
    /// This is not just painting, but involves processing events, doing layout
    /// if needed, updating the accessibility tree, and then actually painting.
    pub fn paint(&mut self) {
        loop {
            self.send_events();
            // TODO: be more lazy re-rendering
            self.render();
            let root_pod = self.root_pod.as_mut().unwrap();
            let mut cx_state =
                CxState::new(&self.window_handle, &mut self.font_cx, &mut self.events);
            let mut update_cx = UpdateCx::new(&mut cx_state, &mut self.root_state);
            root_pod.update(&mut update_cx);
            let mut layout_cx = LayoutCx::new(&mut cx_state, &mut self.root_state);
            let bc = BoxConstraints::tight(self.size);
            root_pod.layout(&mut layout_cx, &bc);
            if cx_state.has_messages() {
                // Rerun app logic, primarily for LayoutObserver
                // We might want some debugging here if the number of iterations
                // becomes extreme.
                continue;
            }
            if self.accesskit_connected {
                let update = self.accessibility();
                // TODO: it would be cleaner to not use a closure here.
                self.window_handle.update_accesskit_if_active(|| update);
            }
            // Borrow again to avoid multiple borrows.
            // TODO: maybe make accessibility a method on CxState?
            let root_pod = self.root_pod.as_mut().unwrap();
            let mut cx_state =
                CxState::new(&self.window_handle, &mut self.font_cx, &mut self.events);
            let mut paint_cx = PaintCx::new(&mut cx_state, &mut self.root_state);
            root_pod.paint(&mut paint_cx);
            break;
        }
    }

    pub fn window_event(&mut self, event: Event) {
        self.ensure_root();
        let root_pod = self.root_pod.as_mut().unwrap();
        let mut cx_state = CxState::new(&self.window_handle, &mut self.font_cx, &mut self.events);
        let mut event_cx = EventCx::new(&mut cx_state, &mut self.root_state);
        root_pod.event(&mut event_cx, &event);
        self.send_events();
    }

    fn send_events(&mut self) {
        if !self.events.is_empty() {
            let events = std::mem::take(&mut self.events);
            let _ = self.req_chan.blocking_send(AppReq::Events(events));
        }
    }

    // Make sure the widget tree (root pod) is available
    fn ensure_root(&mut self) {
        if self.root_pod.is_none() {
            self.render();
        }
    }

    /// Run the app logic and update the widget tree.
    fn render(&mut self) {
        if self.render_inner(false) {
            self.render_inner(true);
        }
    }

    /// Run one pass of app logic.
    ///
    /// Return value is whether there are any pending async futures.
    fn render_inner(&mut self, delay: bool) -> bool {
        self.cx.pending_async.clear();
        let _ = self.req_chan.blocking_send(AppReq::Render(delay));
        if let Some(response) = self.response_chan.blocking_recv() {
            let state =
                if let Some(element) = self.root_pod.as_mut().and_then(|pod| pod.downcast_mut()) {
                    let mut state = response.state.unwrap();
                    let changes = response.view.rebuild(
                        &mut self.cx,
                        response.prev.as_ref().unwrap(),
                        self.id.as_mut().unwrap(),
                        &mut state,
                        element,
                    );
                    self.root_pod.as_mut().unwrap().mark(changes);
                    assert!(self.cx.is_empty(), "id path imbalance on rebuild");
                    state
                } else {
                    let (id, state, element) = response.view.build(&mut self.cx);
                    assert!(self.cx.is_empty(), "id path imbalance on build");
                    self.root_pod = Some(Pod::new(element));
                    self.id = Some(id);
                    state
                };
            let pending = std::mem::take(&mut self.cx.pending_async);
            let has_pending = !pending.is_empty();
            let _ = self
                .return_chan
                .blocking_send((response.view, state, pending));
            has_pending
        } else {
            false
        }
    }
}

impl<T, V: View<T>> App<T, V> {
    pub fn fragment(&self) -> &SceneFragment {
        self.root_pod.as_ref().unwrap().fragment()
    }
}

impl<T, V: View<T>, F: FnMut(&mut T) -> V> AppTask<T, V, F>
where
    V::Element: Widget + 'static,
{
    async fn run(&mut self) {
        let mut deadline = None;
        loop {
            let rx = self.req_chan.recv();
            let req = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, rx).await,
                None => Ok(rx.await),
            };
            match req {
                Ok(Some(req)) => match req {
                    AppReq::SetIdleHandle(handle) => self.idle_handle = Some(handle),
                    AppReq::Events(events) => {
                        for event in events {
                            let id_path = &event.id_path[1..];
                            self.view.as_ref().unwrap().message(
                                id_path,
                                self.state.as_mut().unwrap(),
                                event.body,
                                &mut self.data,
                            );
                        }
                    }
                    AppReq::Wake(id_path) => {
                        let result = self.view.as_ref().unwrap().message(
                            &id_path[1..],
                            self.state.as_mut().unwrap(),
                            Box::new(AsyncWake),
                            &mut self.data,
                        );
                        if matches!(result, MessageResult::RequestRebuild) {
                            // request re-render from UI thread
                            if self.ui_state == UiState::Start {
                                if let Some(handle) = self.idle_handle.as_mut() {
                                    handle.schedule_idle(IdleToken::new(42));
                                }
                                self.ui_state = UiState::WokeUI;
                            }
                            let id = id_path.last().unwrap();
                            self.pending_async.remove(&id);
                            if self.pending_async.is_empty() && self.ui_state == UiState::Delayed {
                                self.render().await;
                                deadline = None;
                            }
                        }
                    }
                    AppReq::Render(delay) => {
                        if !delay || self.pending_async.is_empty() {
                            self.render().await;
                            deadline = None;
                        } else {
                            deadline = Some(tokio::time::Instant::now() + RENDER_DELAY);
                            self.ui_state = UiState::Delayed;
                        }
                    }
                },
                Ok(None) => break,
                Err(_) => {
                    self.render().await;
                    deadline = None;
                }
            }
        }
    }

    async fn render(&mut self) {
        let view = (self.app_logic)(&mut self.data);
        let response = RenderResponse {
            prev: self.view.take(),
            view,
            state: self.state.take(),
        };
        if self.response_chan.send(response).await.is_err() {
            println!("error sending render response");
        }
        if let Some((view, state, pending)) = self.return_chan.recv().await {
            self.view = Some(view);
            self.state = Some(state);
            self.pending_async = pending;
        }
        self.ui_state = UiState::Start;
    }
}
