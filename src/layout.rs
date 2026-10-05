use leptos::ev::MouseEvent;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::app::Ctx;
use crate::util;

/// A folded pane's width.
const RAIL: f64 = 34.0;
/// The diff and code keep at least this much room: a window too narrow for
/// the side panes' widths shrinks them, down to their minimum, instead.
const MIDDLE_MIN: f64 = 320.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dock {
    Left,
    Right,
}

impl Dock {
    fn open_key(self) -> &'static str {
        match self {
            Dock::Left => "sidebar-open",
            Dock::Right => "conversation-open",
        }
    }

    fn width_key(self) -> &'static str {
        match self {
            Dock::Left => "sidebar-width",
            Dock::Right => "conversation-width",
        }
    }

    fn default_width(self) -> f64 {
        match self {
            Dock::Left => 300.0,
            Dock::Right => 380.0,
        }
    }

    fn min_width(self) -> f64 {
        match self {
            Dock::Left => 200.0,
            Dock::Right => 280.0,
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Dock::Left => ".app > .sidebar",
            Dock::Right => ".app > .conversation",
        }
    }

    /// Dragging right widens the left pane and narrows the right one.
    fn grow(self) -> f64 {
        match self {
            Dock::Left => 1.0,
            Dock::Right => -1.0,
        }
    }
}

/// A side pane: folded into a rail or open at a width, both remembered.
#[derive(Clone, Copy)]
pub struct Pane {
    dock: Dock,
    pub open: RwSignal<bool>,
    pub width: RwSignal<f64>,
}

impl Pane {
    fn new(dock: Dock) -> Self {
        let width = util::load::<f64>(dock.width_key())
            .unwrap_or(dock.default_width())
            .max(dock.min_width());
        Self {
            dock,
            open: RwSignal::new(util::load(dock.open_key()).unwrap_or(true)),
            width: RwSignal::new(width),
        }
    }

    fn track(self) -> String {
        if self.open.get() {
            format!(
                "minmax({}px, {}px)",
                self.dock.min_width(),
                self.width.get()
            )
        } else {
            format!("{RAIL}px")
        }
    }

    fn reset(self) {
        self.width.set(self.dock.default_width());
        self.open.set(true);
    }
}

#[derive(Clone, Copy)]
struct Drag {
    pane: Pane,
    start_x: f64,
    start_width: f64,
    /// The widest the pane can go and still leave the middle its room.
    max: f64,
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub sidebar: Pane,
    pub conversation: Pane,
    drag: RwSignal<Option<Drag>>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            sidebar: Pane::new(Dock::Left),
            conversation: Pane::new(Dock::Right),
            drag: RwSignal::new(None),
        }
    }
}

impl Layout {
    /// The app grid's columns: tree | diff | conversation.
    pub fn columns(self) -> String {
        format!(
            "{} minmax({MIDDLE_MIN}px, 1fr) {}",
            self.sidebar.track(),
            self.conversation.track()
        )
    }

    pub fn resizing(self) -> bool {
        self.drag.with(Option::is_some)
    }

    fn start_drag(self, pane: Pane, x: f64) {
        let other = match pane.dock {
            Dock::Left => self.conversation,
            Dock::Right => self.sidebar,
        };
        let window_width = window()
            .inner_width()
            .ok()
            .and_then(|w| w.as_f64())
            .unwrap_or_default();
        self.drag.set(Some(Drag {
            pane,
            start_x: x,
            // As laid out, which a narrow window may have shrunk.
            start_width: rendered_width(pane.dock).unwrap_or(RAIL),
            max: window_width - rendered_width(other.dock).unwrap_or(RAIL) - MIDDLE_MIN,
        }));
    }

    /// Follows the pointer; past halfway between the rail and the minimum
    /// width, the pane folds.
    fn drag_to(self, x: f64) {
        let Some(drag) = self.drag.get_untracked() else {
            return;
        };
        // A click on the edge shouldn't overwrite the width it was shrunk from.
        if x == drag.start_x {
            return;
        }
        let Pane { dock, open, width } = drag.pane;
        let wanted = drag.start_width + dock.grow() * (x - drag.start_x);
        let unfold = wanted >= (RAIL + dock.min_width()) / 2.0;
        if open.get_untracked() != unfold {
            open.set(unfold);
        }
        if unfold {
            let max = drag.max.max(dock.min_width());
            width.set(wanted.clamp(dock.min_width(), max).round());
        }
    }
}

fn rendered_width(dock: Dock) -> Option<f64> {
    let element = document().query_selector(dock.selector()).ok()??;
    let element = element.dyn_into::<web_sys::HtmlElement>().ok()?;
    Some(element.offset_width().into())
}

/// Remembers the panes, and follows a drag anywhere in the window once it
/// starts on an edge.
pub fn setup(layout: Layout) {
    for pane in [layout.sidebar, layout.conversation] {
        Effect::new(move |_| util::store(pane.dock.open_key(), &pane.open.get()));
        Effect::new(move |_| util::store(pane.dock.width_key(), &pane.width.get()));
    }

    let end = move || {
        if layout.drag.with_untracked(Option::is_some) {
            layout.drag.set(None);
        }
    };
    let moves = window_event_listener(leptos::ev::mousemove, move |ev| {
        // The button came up outside the window.
        if ev.buttons() & 1 == 0 {
            end();
        } else {
            layout.drag_to(ev.client_x().into());
        }
    });
    let ups = window_event_listener(leptos::ev::mouseup, move |_| end());
    let blurs = window_event_listener(leptos::ev::blur, move |_| end());
    on_cleanup(move || {
        moves.remove();
        ups.remove();
        blurs.remove();
    });
}

/// A pane's inner edge, open or folded: drag it to resize the pane,
/// double-click it for the default width.
#[component]
pub fn ResizeHandle(pane: Pane) -> impl IntoView {
    let layout = expect_context::<Ctx>().layout;
    let start = move |ev: MouseEvent| {
        if ev.button() == 0 {
            // Otherwise the drag selects text on its way.
            ev.prevent_default();
            layout.start_drag(pane, ev.client_x().into());
        }
    };
    let active = move || {
        layout
            .drag
            .with(|d| d.is_some_and(|d| d.pane.dock == pane.dock))
    };
    view! {
        <div
            class="resize-handle"
            class:active=active
            on:mousedown=start
            on:dblclick=move |_| pane.reset()
        ></div>
    }
}
