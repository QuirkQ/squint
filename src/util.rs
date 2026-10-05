use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{ScrollBehavior, ScrollLogicalPosition};

use crate::api;

/// "5m", "3h", "2d", "4mo" since an RFC 3339 timestamp.
pub fn ago(timestamp: &str) -> String {
    let then = js_sys::Date::parse(timestamp);
    if then.is_nan() {
        return String::new();
    }
    let minutes = ((js_sys::Date::now() - then) / 60_000.0).max(0.0) as u64;
    match minutes {
        0 => "now".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 24 => format!("{}h", m / 60),
        m if m < 60 * 24 * 30 => format!("{}d", m / (60 * 24)),
        m if m < 60 * 24 * 365 => format!("{}mo", m / (60 * 24 * 30)),
        m => format!("{}y", m / (60 * 24 * 365)),
    }
}

/// "45s", "4m 38s", "1h 2m" from `start` to `end`, or until now while
/// something is still running.
pub fn duration(start: Option<&str>, end: Option<&str>) -> Option<String> {
    let start = js_sys::Date::parse(start?);
    let end = end.map_or_else(js_sys::Date::now, js_sys::Date::parse);
    if start.is_nan() || end.is_nan() {
        return None;
    }
    let seconds = ((end - start) / 1000.0).max(0.0) as u64;
    Some(match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, s % 3600 / 60),
    })
}

/// `/Users/me/.local/share/x` → `~/.local/share/x`.
pub fn tilde(path: &str) -> String {
    let mut parts = path.splitn(4, '/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(""), Some("Users" | "home"), Some(_user), Some(rest)) => format!("~/{rest}"),
        _ => path.to_string(),
    }
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn open_external(url: String) {
    spawn_local(async move {
        if let Err(error) = api::open_url(&url).await {
            leptos::logging::warn!("{error}");
        }
    });
}

/// Scrolls an element into view (centred) and briefly highlights it, or its
/// table row for a line-number cell. Returns false if there is no such element.
pub fn reveal(id: &str) -> bool {
    scroll_to(
        id,
        ScrollBehavior::Smooth,
        ScrollLogicalPosition::Center,
        true,
    )
}

/// [`reveal`] without the smooth scroll, which crawls across a long file.
pub fn reveal_now(id: &str) -> bool {
    scroll_to(
        id,
        ScrollBehavior::Instant,
        ScrollLogicalPosition::Center,
        true,
    )
}

pub fn scroll_center(id: &str) {
    scroll_to(
        id,
        ScrollBehavior::Instant,
        ScrollLogicalPosition::Center,
        false,
    );
}

/// Scrolls only as far as needed to show the element, e.g. a list's
/// selected row.
pub fn scroll_nearest(id: &str) {
    scroll_to(
        id,
        ScrollBehavior::Instant,
        ScrollLogicalPosition::Nearest,
        false,
    );
}

fn scroll_to(
    id: &str,
    behavior: ScrollBehavior,
    block: ScrollLogicalPosition,
    flash: bool,
) -> bool {
    let Some(element) = document().get_element_by_id(id) else {
        return false;
    };
    let options = web_sys::ScrollIntoViewOptions::new();
    options.set_behavior(behavior);
    options.set_block(block);
    element.scroll_into_view_with_scroll_into_view_options(&options);
    if !flash {
        return true;
    }

    let target = element.closest("tr").ok().flatten().unwrap_or(element);
    let classes = target.class_list();
    let _ = classes.remove_1("flash");
    // Re-adding on the next frame restarts the animation.
    request_animation_frame(move || {
        let _ = classes.add_1("flash");
    });
    true
}

pub fn scroll_top(id: &str) {
    if let Some(element) = document().get_element_by_id(id) {
        element.set_scroll_top(0);
    }
}

pub fn focus(id: &str) {
    if let Some(input) = document()
        .get_element_by_id(id)
        .and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        let _ = input.focus();
        input.select();
    }
}

/// Focuses a text box with the caret at the end of its text. Returns false
/// if there is no such element.
pub fn focus_text(id: &str) -> bool {
    let Some(area) = document()
        .get_element_by_id(id)
        .and_then(|el| el.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    else {
        return false;
    };
    let _ = area.focus();
    let end = area.value().len() as u32;
    let _ = area.set_selection_range(end, end);
    true
}

/// Nothing has the keyboard focus, so taking it steals it from nobody.
pub fn nothing_focused() -> bool {
    document()
        .active_element()
        .is_none_or(|active| active.tag_name() == "BODY")
}

pub fn blur(id: &str) {
    if let Some(element) = document()
        .get_element_by_id(id)
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = element.blur();
    }
}

pub fn query_all(selector: &str) -> Vec<web_sys::Element> {
    let Ok(list) = document().query_selector_all(selector) else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|i| list.item(i)?.dyn_into().ok())
        .collect()
}

/// The page's selected text, when it's a single line worth searching for.
pub fn selected_text() -> Option<String> {
    let text: String = window().get_selection().ok()??.to_string().into();
    let text = text.trim();
    (!text.is_empty() && !text.contains('\n') && text.len() <= 200).then(|| text.to_string())
}

/// The identifier under the pointer inside a `td.code` cell, for ⌘-click.
pub fn word_at_point(x: i32, y: i32) -> Option<String> {
    let (node, offset) = caret_at(x as f64, y as f64)?;
    let element = match node.dyn_ref::<web_sys::Element>() {
        Some(element) => element.clone(),
        None => node.parent_element()?,
    };
    let cell = element.closest("td.code").ok()??;
    // The text before the caret, measured over the whole cell: words can
    // span the <mark>s of word-level highlights.
    let range = document().create_range().ok()?;
    range.set_start(&cell, 0).ok()?;
    range.set_end(&node, offset).ok()?;
    let before: String = range.to_string().into();
    let text = cell.text_content()?;
    shared::code::word_at(&text, before.len()).map(str::to_string)
}

/// The standard `caretPositionFromPoint` where there is one, else WebKit's
/// `caretRangeFromPoint`. Neither is in every engine, so both are looked up.
fn caret_at(x: f64, y: f64) -> Option<(web_sys::Node, u32)> {
    let doc: JsValue = document().into();
    let call = |name: &str| -> Option<JsValue> {
        let function = js_sys::Reflect::get(&doc, &name.into()).ok()?;
        let function = function.dyn_ref::<js_sys::Function>()?;
        let result = function.call2(&doc, &x.into(), &y.into()).ok()?;
        (!result.is_null() && !result.is_undefined()).then_some(result)
    };
    let field = |object: &JsValue, name: &str| js_sys::Reflect::get(object, &name.into()).ok();
    let (node, offset) = if let Some(position) = call("caretPositionFromPoint") {
        (field(&position, "offsetNode")?, field(&position, "offset")?)
    } else {
        let range = call("caretRangeFromPoint")?;
        (
            field(&range, "startContainer")?,
            field(&range, "startOffset")?,
        )
    };
    Some((node.dyn_into().ok()?, offset.as_f64()? as u32))
}

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

pub fn load<T: DeserializeOwned>(key: &str) -> Option<T> {
    let json = storage()?.get_item(key).ok()??;
    serde_json::from_str(&json).ok()
}

pub fn store<T: Serialize>(key: &str, value: &T) {
    if let (Some(storage), Ok(json)) = (storage(), serde_json::to_string(value)) {
        let _ = storage.set_item(key, &json);
    }
}

#[component]
pub fn Avatar(url: String, #[prop(optional)] size: Option<u32>) -> impl IntoView {
    let size = size.unwrap_or(20);
    view! { <img class="avatar" src=url width=size height=size alt="" loading="lazy"/> }
}

#[component]
pub fn PullIcon(draft: bool) -> impl IntoView {
    let (class, path) = if draft {
        ("pr-icon draft", DRAFT_PR_ICON)
    } else {
        ("pr-icon open", OPEN_PR_ICON)
    };
    view! {
        <svg class=class viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
            <path d=path></path>
        </svg>
    }
}

#[component]
pub fn RepoIcon() -> impl IntoView {
    view! {
        <svg class="repo-icon" viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path d=REPO_ICON></path>
        </svg>
    }
}

/// An icon in the text colour, for a button that names itself in its tooltip.
#[component]
pub fn Icon(path: &'static str, #[prop(optional)] size: Option<u32>) -> impl IntoView {
    let size = size.unwrap_or(16);
    view! {
        <svg class="svg-icon" viewBox="0 0 16 16" width=size height=size aria-hidden="true">
            <path d=path fill-rule="evenodd"></path>
        </svg>
    }
}

// Paths from GitHub's Octicons (MIT).
const OPEN_PR_ICON: &str = "M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.573.677A.25.25 0 0 1 10 .854V2.5h1A2.5 2.5 0 0 1 13.5 5v5.628a2.251 2.251 0 1 1-1.5 0V5a1 1 0 0 0-1-1h-1v1.646a.25.25 0 0 1-.427.177L7.177 3.427a.25.25 0 0 1 0-.354ZM3.75 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm0 9.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm8.25.75a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0Z";
const DRAFT_PR_ICON: &str = "M3.25 1A2.25 2.25 0 0 1 4 5.372v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.251 2.251 0 0 1 3.25 1Zm9.5 14a2.25 2.25 0 1 1 0-4.5 2.25 2.25 0 0 1 0 4.5ZM2.5 3.25a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0ZM3.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm9.5 0a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5ZM14 7.5a1.25 1.25 0 1 1-2.5 0 1.25 1.25 0 0 1 2.5 0Zm0-4.25a1.25 1.25 0 1 1-2.5 0 1.25 1.25 0 0 1 2.5 0Z";
const REPO_ICON: &str = "M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.25.25 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z";
pub const GITHUB_ICON: &str = "M8 0c4.42 0 8 3.58 8 8a8.013 8.013 0 0 1-5.45 7.59c-.4.08-.55-.17-.55-.38 0-.27.01-1.13.01-2.2 0-.75-.25-1.23-.54-1.48 1.78-.2 3.65-.88 3.65-3.95 0-.88-.31-1.59-.82-2.15.08-.2.36-1.02-.08-2.12 0 0-.67-.22-2.2.82-.64-.18-1.32-.27-2-.27-.68 0-1.36.09-2 .27-1.53-1.03-2.2-.82-2.2-.82-.44 1.1-.16 1.92-.08 2.12-.51.56-.82 1.28-.82 2.15 0 3.06 1.86 3.75 3.64 3.95-.23.2-.44.55-.51 1.07-.46.21-1.61.55-2.33-.66-.15-.24-.6-.83-1.23-.82-.67.01-.27.38.01.53.34.19.73.9.82 1.13.16.45.68 1.31 2.69.94 0 .67.01 1.3.01 1.49 0 .21-.15.45-.55.38A7.995 7.995 0 0 1 0 8c0-4.42 3.58-8 8-8Z";
pub const UNFOLD_ICON: &str = "m8.177.677 2.896 2.896a.25.25 0 0 1-.177.427H8.75v1.25a.75.75 0 0 1-1.5 0V4H5.104a.25.25 0 0 1-.177-.427L7.823.677a.25.25 0 0 1 .354 0ZM7.25 10.75a.75.75 0 0 1 1.5 0V12h2.146a.25.25 0 0 1 .177.427l-2.896 2.896a.25.25 0 0 1-.354 0l-2.896-2.896A.25.25 0 0 1 5.104 12H7.25v-1.25Zm-5-2a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5ZM6 8a.75.75 0 0 1-.75.75h-.5a.75.75 0 0 1 0-1.5h.5A.75.75 0 0 1 6 8Zm2.25.75a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5ZM12 8a.75.75 0 0 1-.75.75h-.5a.75.75 0 0 1 0-1.5h.5A.75.75 0 0 1 12 8Zm2.25.75a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5Z";
pub const FOLD_ICON: &str = "M10.896 2H8.75V.75a.75.75 0 0 0-1.5 0V2H5.104a.25.25 0 0 0-.177.427l2.896 2.896a.25.25 0 0 0 .354 0l2.896-2.896A.25.25 0 0 0 10.896 2ZM8.75 15.25a.75.75 0 0 1-1.5 0V14H5.104a.25.25 0 0 1-.177-.427l2.896-2.896a.25.25 0 0 1 .354 0l2.896 2.896a.25.25 0 0 1-.177.427H8.75v1.25Zm-6.5-6.5a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5ZM6 8a.75.75 0 0 1-.75.75h-.5a.75.75 0 0 1 0-1.5h.5A.75.75 0 0 1 6 8Zm2.25.75a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5ZM12 8a.75.75 0 0 1-.75.75h-.5a.75.75 0 0 1 0-1.5h.5A.75.75 0 0 1 12 8Zm2.25.75a.75.75 0 0 0 0-1.5h-.5a.75.75 0 0 0 0 1.5h.5Z";
pub const FILE_CODE_ICON: &str = "M4 1.75C4 .784 4.784 0 5.75 0h5.586c.464 0 .909.184 1.237.513l2.914 2.914c.329.328.513.773.513 1.237v8.586A1.75 1.75 0 0 1 14.25 15h-9a.75.75 0 0 1 0-1.5h9a.25.25 0 0 0 .25-.25V6h-2.75A1.75 1.75 0 0 1 10 4.25V1.5H5.75a.25.25 0 0 0-.25.25v2.5a.75.75 0 0 1-1.5 0Zm1.72 4.97a.75.75 0 0 1 1.06 0l2 2a.75.75 0 0 1 0 1.06l-2 2a.75.75 0 0 1-1.06-1.06l1.47-1.47-1.47-1.47a.75.75 0 0 1 0-1.06ZM3.28 7.78 1.81 9.25l1.47 1.47a.751.751 0 0 1-.018 1.042.751.751 0 0 1-1.042.018l-2-2a.75.75 0 0 1 0-1.06l2-2a.751.751 0 0 1 1.042.018.751.751 0 0 1 .018 1.042Zm8.22-6.218V4.25c0 .138.112.25.25.25h2.688l-.011-.013-2.914-2.914-.013-.011Z";
pub const DIFF_ICON: &str = "M8.75 1.75V5H12a.75.75 0 0 1 0 1.5H8.75v3.25a.75.75 0 0 1-1.5 0V6.5H4A.75.75 0 0 1 4 5h3.25V1.75a.75.75 0 0 1 1.5 0ZM4 13h8a.75.75 0 0 1 0 1.5H4A.75.75 0 0 1 4 13Z";
pub const SEARCH_ICON: &str = "M10.68 11.74a6 6 0 0 1-7.922-8.982 6 6 0 0 1 8.982 7.922l3.04 3.04a.749.749 0 0 1-.326 1.275.749.749 0 0 1-.734-.215ZM11.5 7a4.499 4.499 0 1 0-8.997 0A4.499 4.499 0 0 0 11.5 7Z";
// Square-cornered, to match the app: a diff in one column or two, and Zed.
pub const UNIFIED_ICON: &str = "M1 2h14v12H1Zm1.5 1.5v9h11v-9ZM4 5.5h8V7H4Zm0 3.5h8v1.5H4Z";
pub const SPLIT_ICON: &str = "M1 2h14v12H1Zm1.5 1.5v9h11v-9Zm4.75 0h1.5v9h-1.5ZM3.75 5.5h2.5V7h-2.5Zm0 3.5h2.5v1.5h-2.5Zm6-3.5h2.5V7h-2.5Zm0 3.5h2.5v1.5h-2.5Z";
pub const ZED_ICON: &str =
    "M1 1h14v14H1Zm1.5 1.5v11h11v-11ZM4.5 4.5h7V6L7 10h4.5v1.5h-7V10L9 6H4.5Z";
