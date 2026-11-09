//! Browser accessibility adapter: exposes GPUI's AccessKit tree as real DOM.
//!
//! The canvas is opaque to assistive technology, so every AccessKit node is
//! mirrored into an invisible element under one overlay `<div>`, positioned
//! over the pixels it describes. Screen readers, braille displays, voice
//! control, find-in-page and touch exploration then work against ordinary
//! HTML semantics (`role`, `aria-*`, text content), and activating a mirrored
//! element is sent back to GPUI as an AccessKit [`ActionRequest`].
//!
//! Three rules keep the mirror from fighting the app:
//! - Pointer input never reaches the mirror (`pointer-events: none`); the
//!   canvas keeps handling it.
//! - Text editing stays on the IME textarea. A focused text input is exposed
//!   by labelling that textarea rather than by a mirrored element.
//! - DOM focus only moves when GPUI focus changes, and only on non-touch
//!   devices, where it follows keyboard navigation so screen readers announce
//!   it. Keys pressed while a mirrored element has focus are forwarded to
//!   GPUI like keys typed into the textarea.
//!
//! The tree is applied in full every frame. Elements are reused by node id,
//! and each attribute is written only when it differs, so unchanged nodes
//! cost a comparison and produce no accessibility events.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gpui::accesskit::{
    Action, ActionRequest, Live, Node, NodeId, Rect, Role, Toggled, TreeId, TreeUpdate,
};
use gpui::A11yCallbacks;
use wasm_bindgen::JsCast;

use crate::events::EventListenerHandle;
use crate::window::WebWindowInner;

const NODE_ATTR: &str = "data-gpui-node";

const ELEMENT_STYLE: &str = "position:absolute;box-sizing:border-box;margin:0;padding:0;\
border:0;color:transparent;background:transparent;outline:none;pointer-events:none;\
white-space:nowrap;overflow:visible;font:14px/1 sans-serif;";

const ROOT_STYLE: &str = "position:fixed;left:0;top:0;width:0;height:0;margin:0;padding:0;\
border:0;overflow:visible;pointer-events:none;z-index:1;";

/// Visually hidden but announced. Never `display: none`, which would remove
/// the regions from the accessibility tree.
const LIVE_STYLE: &str = "position:absolute;width:1px;height:1px;overflow:hidden;\
clip-path:inset(50%);white-space:nowrap;";

struct Mirrored {
    element: web_sys::HtmlElement,
    node: Node,
    children: Vec<NodeId>,
}

pub(crate) struct WebA11y {
    root: web_sys::HtmlElement,
    polite: web_sys::HtmlElement,
    assertive: web_sys::HtmlElement,
    callbacks: A11yCallbacks,
    nodes: HashMap<NodeId, Mirrored>,
    /// Last text announced for each live node, so only changes are spoken.
    announced: HashMap<NodeId, String>,
    /// Alternates a trailing space so a repeated message is announced again.
    toggle: bool,
    focus: Option<NodeId>,
    root_rect: (f64, f64, f64, f64),
    /// Set while the adapter itself moves DOM focus, so that `focusin` is
    /// not echoed back to GPUI as a focus request.
    focusing: Rc<Cell<bool>>,
    _listeners: Vec<EventListenerHandle>,
}

impl WebA11y {
    pub(crate) fn new(
        inner: &Rc<WebWindowInner>,
        callbacks: A11yCallbacks,
    ) -> anyhow::Result<Self> {
        let document = inner
            .browser_window
            .document()
            .ok_or_else(|| anyhow::anyhow!("No `document` found on window"))?;
        let body = document
            .body()
            .ok_or_else(|| anyhow::anyhow!("No `body` found on document"))?;
        let create = |style: &str| -> anyhow::Result<web_sys::HtmlElement> {
            let element: web_sys::HtmlElement = document
                .create_element("div")
                .map_err(|e| anyhow::anyhow!("Failed to create element: {e:?}"))?
                .unchecked_into();
            element.style().set_css_text(style);
            Ok(element)
        };

        let root = create(ROOT_STYLE)?;
        root.set_id("gpui-a11y");
        let polite = create(LIVE_STYLE)?;
        polite.set_attribute("aria-live", "polite").ok();
        let assertive = create(LIVE_STYLE)?;
        assertive.set_attribute("aria-live", "assertive").ok();
        root.append_child(&polite).ok();
        root.append_child(&assertive).ok();
        body.append_child(&root)
            .map_err(|e| anyhow::anyhow!("Failed to append accessibility root: {e:?}"))?;

        // The mirror describes the canvas, so the canvas itself is noise.
        inner.canvas.set_attribute("aria-hidden", "true").ok();

        let focusing = Rc::new(Cell::new(false));
        let target: &web_sys::EventTarget = root.as_ref();
        let weak = Rc::downgrade(inner);
        let listeners = vec![
            EventListenerHandle::add(target, "click", {
                let weak = weak.clone();
                move |event| {
                    let event: web_sys::Event = event.unchecked_into();
                    if let Some(node) = event_node(&event) {
                        event.prevent_default();
                        send_action(&weak, node, Action::Click);
                    }
                }
            }),
            EventListenerHandle::add(target, "focusin", {
                let weak = weak.clone();
                let focusing = focusing.clone();
                move |event| {
                    if focusing.get() {
                        return;
                    }
                    if let Some(node) = event_node(&event.unchecked_into()) {
                        send_action(&weak, node, Action::Focus);
                    }
                }
            }),
            EventListenerHandle::add(target, "keydown", {
                let weak = weak.clone();
                move |event| {
                    if let Some(inner) = weak.upgrade() {
                        inner.handle_key_down(event.unchecked_into());
                    }
                }
            }),
            EventListenerHandle::add(target, "keyup", move |event| {
                if let Some(inner) = weak.upgrade() {
                    inner.handle_key_up(event.unchecked_into());
                }
            }),
        ];

        Ok(Self {
            root,
            polite,
            assertive,
            callbacks,
            nodes: HashMap::new(),
            announced: HashMap::new(),
            toggle: false,
            focus: None,
            root_rect: (0.0, 0.0, 0.0, 0.0),
            focusing,
            _listeners: listeners,
        })
    }

    /// The adapter has no way to know whether assistive technology is
    /// running, so it activates at once and stays active.
    pub(crate) fn activate(&mut self, inner: &WebWindowInner) {
        if let Some(update) = (self.callbacks.activation)() {
            self.update(inner, update);
        }
    }

    pub(crate) fn action(&self, node: NodeId, action: Action) {
        (self.callbacks.action)(ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: node,
            data: None,
        });
    }

    pub(crate) fn update(&mut self, inner: &WebWindowInner, update: TreeUpdate) {
        self.place_root(inner);
        let scale = inner
            .state
            .try_borrow()
            .map(|state| state.scale_factor)
            .unwrap_or(1.0)
            .max(0.01) as f64;

        let mut incoming: HashMap<NodeId, Node> = update.nodes.into_iter().collect();
        let root_id = update.tree.as_ref().map(|tree| tree.root).unwrap_or(NodeId(0));

        // Depth-first from the root, so that DOM order is the tree's order,
        // which GPUI builds in paint (and so visual) order.
        let mut seen: HashMap<NodeId, ()> = HashMap::new();
        let mut root_children = Vec::new();
        let mut stack: Vec<(NodeId, bool, Rect)> = vec![(root_id, true, Rect::ZERO)];
        while let Some((id, is_root, origin)) = stack.pop() {
            let Some(node) = incoming.remove(&id) else {
                continue;
            };
            let own_origin = node.bounds().unwrap_or(origin);
            for child in node.children().iter().rev() {
                stack.push((*child, false, own_origin));
            }
            if is_root {
                // The window node is the overlay root itself.
                root_children = node.children().to_vec();
                continue;
            }
            seen.insert(id, ());
            self.apply_node(id, node, origin, scale);
        }

        let gone: Vec<NodeId> = self
            .nodes
            .keys()
            .filter(|id| !seen.contains_key(id))
            .copied()
            .collect();
        for id in gone {
            if let Some(mirrored) = self.nodes.remove(&id) {
                mirrored.element.remove();
            }
            self.announced.remove(&id);
        }

        // Children are attached after every node exists.
        let parents: Vec<(NodeId, Vec<NodeId>)> = self
            .nodes
            .iter()
            .map(|(id, mirrored)| (*id, mirrored.node.children().to_vec()))
            .collect();
        for (id, children) in parents {
            self.reconcile_children(Some(id), &children);
        }
        self.reconcile_children(None, &root_children);

        self.sync_focus(inner, update.focus);
    }

    fn place_root(&mut self, inner: &WebWindowInner) {
        let canvas: &web_sys::Element = inner.canvas.as_ref();
        let rect = canvas.get_bounding_client_rect();
        let next = (rect.left(), rect.top(), rect.width(), rect.height());
        if next != self.root_rect {
            self.root_rect = next;
            let style = self.root.style();
            style.set_property("left", &format!("{}px", next.0)).ok();
            style.set_property("top", &format!("{}px", next.1)).ok();
            style.set_property("width", &format!("{}px", next.2)).ok();
            style.set_property("height", &format!("{}px", next.3)).ok();
        }
    }

    fn apply_node(&mut self, id: NodeId, node: Node, origin: Rect, scale: f64) {
        let had_children;
        let mirrored = match self.nodes.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                let mirrored = entry.into_mut();
                if mirrored.node == node {
                    return;
                }
                had_children = !mirrored.node.children().is_empty();
                mirrored
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Some(element) = create_element(&self.root) else {
                    return;
                };
                element.set_attribute(NODE_ATTR, &id.0.to_string()).ok();
                had_children = false;
                entry.insert(Mirrored {
                    element,
                    node: Node::default(),
                    children: Vec::new(),
                })
            }
        };

        write_semantics(&mirrored.element, &node, had_children);
        write_geometry(&mirrored.element, node.bounds(), origin, scale);

        if let Some(live) = live_politeness(&node) {
            let text = text_of(&node).unwrap_or_default();
            let previous = self.announced.insert(id, text.clone());
            if !text.is_empty() && previous.as_deref() != Some(text.as_str()) {
                self.toggle = !self.toggle;
                let message = if self.toggle { text } else { format!("{text}\u{a0}") };
                let region = match live {
                    Live::Assertive => &self.assertive,
                    _ => &self.polite,
                };
                region.set_text_content(Some(&message));
            }
        }
        mirrored.node = node;
    }

    /// Puts `children`' elements under `parent` in order, moving only the
    /// ones that are out of place: re-inserting a focused element would blur
    /// it.
    fn reconcile_children(&mut self, parent: Option<NodeId>, children: &[NodeId]) {
        let container = match parent {
            None => self.root.clone(),
            Some(id) => match self.nodes.get(&id) {
                Some(mirrored) if mirrored.children.as_slice() == children => return,
                Some(mirrored) => mirrored.element.clone(),
                None => return,
            },
        };
        // The live regions are the overlay root's first two children.
        let skip = if parent.is_none() { 2 } else { 0 };
        let elements: Vec<web_sys::HtmlElement> = children
            .iter()
            .filter_map(|child| self.nodes.get(child).map(|m| m.element.clone()))
            .collect();
        let existing = container.children();
        for (index, element) in elements.iter().enumerate() {
            let at = existing.item((index + skip) as u32);
            let element_node: &web_sys::Element = element.as_ref();
            if at.as_ref() != Some(element_node) {
                container.insert_before(element, at.as_ref().map(|e| e.as_ref())).ok();
            }
        }
        if let Some(id) = parent
            && let Some(mirrored) = self.nodes.get_mut(&id)
        {
            mirrored.children = children.to_vec();
        }
    }

    fn sync_focus(&mut self, inner: &WebWindowInner, focus: NodeId) {
        let focus = self.nodes.contains_key(&focus).then_some(focus);
        let changed = self.focus != focus;
        let previous = std::mem::replace(&mut self.focus, focus);

        // A focused text input is edited through the IME textarea, which
        // takes its name; its mirror steps aside to avoid a duplicate.
        if let Some(id) = previous
            && let Some(mirrored) = self.nodes.get(&id)
            && is_text_input(mirrored.node.role())
        {
            mirrored.element.remove_attribute("aria-hidden").ok();
        }
        let Some(id) = focus else { return };
        let Some(mirrored) = self.nodes.get(&id) else {
            return;
        };
        if is_text_input(mirrored.node.role()) {
            mirrored.element.set_attribute("aria-hidden", "true").ok();
            inner.ime_mirror.describe(&mirrored.node);
            return;
        }
        if !changed || inner.touch_input {
            return;
        }
        // Only take focus from the textarea or the mirror itself, never from
        // other page content the app does not own.
        let active = inner
            .browser_window
            .document()
            .and_then(|document| document.active_element());
        let ours = match &active {
            None => true,
            Some(active) => {
                let root: &web_sys::Node = self.root.as_ref();
                inner.ime_mirror.is_element(active)
                    || root.contains(Some(active.as_ref()))
                    || active.tag_name() == "BODY"
            }
        };
        if ours {
            let options = web_sys::FocusOptions::new();
            options.set_prevent_scroll(true);
            self.focusing.set(true);
            mirrored.element.focus_with_options(&options).ok();
            self.focusing.set(false);
        }
    }

    pub(crate) fn remove(&self) {
        self.root.remove();
    }
}

fn send_action(inner: &Weak<WebWindowInner>, node: NodeId, action: Action) {
    if let Some(inner) = inner.upgrade()
        && let Ok(a11y) = inner.a11y.try_borrow()
        && let Some(a11y) = a11y.as_ref()
    {
        a11y.action(node, action);
    }
}

fn event_node(event: &web_sys::Event) -> Option<NodeId> {
    let target: web_sys::Element = event.target()?.dyn_into().ok()?;
    let element = target.closest(&format!("[{NODE_ATTR}]")).ok()??;
    element.get_attribute(NODE_ATTR)?.parse().ok().map(NodeId)
}

fn create_element(root: &web_sys::HtmlElement) -> Option<web_sys::HtmlElement> {
    let element: web_sys::HtmlElement = root
        .owner_document()?
        .create_element("div")
        .ok()?
        .unchecked_into();
    element.style().set_css_text(ELEMENT_STYLE);
    Some(element)
}

fn set_attr(element: &web_sys::Element, name: &str, value: Option<&str>) {
    let current = element.get_attribute(name);
    match value {
        Some(value) if current.as_deref() != Some(value) => {
            element.set_attribute(name, value).ok();
        }
        None if current.is_some() => {
            element.remove_attribute(name).ok();
        }
        _ => {}
    }
}

fn write_semantics(element: &web_sys::HtmlElement, node: &Node, had_children: bool) {
    let el: &web_sys::Element = element.as_ref();
    let role = node.role();
    let leaf = node.children().is_empty();
    // A text field's content is its value; its label stays a name.
    let text = if is_text_input(role) {
        node.value().map(str::to_owned)
    } else {
        text_of(node)
    };

    set_attr(el, "role", aria_role(role));
    // Leaves carry their text as content, which is what find-in-page and
    // reading by character see; containers and roles that cannot be named
    // by content get `aria-label`.
    let content = if leaf && names_from_content(role) { text.as_deref() } else { None };
    if leaf {
        if element.text_content().as_deref().unwrap_or("") != content.unwrap_or("") {
            element.set_text_content(content);
        }
    } else if !had_children {
        // Becoming a container: drop any leaf text before children arrive.
        element.set_text_content(None);
    }
    let label = if content.is_some() && !is_text_input(role) {
        None
    } else {
        node.label()
    };
    set_attr(el, "aria-label", label);
    set_attr(el, "aria-description", node.description());
    set_attr(el, "aria-keyshortcuts", node.keyboard_shortcut());
    set_attr(el, "aria-placeholder", node.placeholder());

    let checked = node.toggled().map(|toggled| match toggled {
        Toggled::True => "true",
        Toggled::False => "false",
        Toggled::Mixed => "mixed",
    });
    let (checked, pressed) = match role {
        Role::Button | Role::DefaultButton => (None, checked),
        _ => (checked, None),
    };
    set_attr(el, "aria-checked", checked);
    set_attr(el, "aria-pressed", pressed);
    set_attr(el, "aria-selected", node.is_selected().map(bool_str));
    set_attr(el, "aria-expanded", node.is_expanded().map(bool_str));
    set_attr(el, "aria-modal", node.is_modal().then_some("true"));
    set_attr(el, "aria-disabled", node.is_disabled().then_some("true"));
    set_attr(el, "aria-hidden", node.is_hidden().then_some("true"));

    let number = |value: Option<f64>| value.map(|v| format!("{v}"));
    set_attr(el, "aria-valuenow", number(node.numeric_value()).as_deref());
    set_attr(el, "aria-valuemin", number(node.min_numeric_value()).as_deref());
    set_attr(el, "aria-valuemax", number(node.max_numeric_value()).as_deref());
    let value_text = if node.numeric_value().is_some() { node.value() } else { None };
    set_attr(el, "aria-valuetext", value_text);
    let count = |value: Option<usize>| value.map(|v| v.to_string());
    set_attr(el, "aria-level", count(node.level()).as_deref());
    set_attr(el, "aria-posinset", count(node.position_in_set()).as_deref());
    set_attr(el, "aria-setsize", count(node.size_of_set()).as_deref());

    // Programmatically focusable only: GPUI owns the Tab order.
    let interactive = node.supports_action(Action::Focus) || node.supports_action(Action::Click);
    set_attr(el, "tabindex", interactive.then_some("-1"));
}

fn write_geometry(element: &web_sys::HtmlElement, bounds: Option<Rect>, origin: Rect, scale: f64) {
    let style = element.style();
    let (x, y, w, h) = match bounds {
        Some(b) => (
            (b.x0 - origin.x0) / scale,
            (b.y0 - origin.y0) / scale,
            b.width() / scale,
            b.height() / scale,
        ),
        None => (0.0, 0.0, 1.0, 1.0),
    };
    for (property, value) in [("left", x), ("top", y), ("width", w), ("height", h)] {
        style.set_property(property, &format!("{value:.1}px")).ok();
    }
    let font = (h * 0.7).clamp(1.0, 16.0);
    style.set_property("font-size", &format!("{font:.0}px")).ok();
}

fn bool_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn text_of(node: &Node) -> Option<String> {
    node.label()
        .or_else(|| node.value())
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

fn live_politeness(node: &Node) -> Option<Live> {
    match (node.live(), node.role()) {
        (Some(Live::Off), _) => None,
        (Some(live), _) => Some(live),
        (None, Role::Alert | Role::AlertDialog) => Some(Live::Assertive),
        (None, Role::Status | Role::Log | Role::Timer) => Some(Live::Polite),
        _ => None,
    }
}

pub(crate) fn is_text_input(role: Role) -> bool {
    matches!(
        role,
        Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::EmailInput
            | Role::NumberInput
            | Role::PasswordInput
            | Role::PhoneNumberInput
            | Role::UrlInput
            | Role::DateInput
            | Role::DateTimeInput
            | Role::WeekInput
            | Role::MonthInput
            | Role::TimeInput
            | Role::EditableComboBox
    )
}

fn names_from_content(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::DefaultButton
            | Role::CheckBox
            | Role::Switch
            | Role::RadioButton
            | Role::Link
            | Role::Heading
            | Role::Tab
            | Role::MenuItem
            | Role::MenuItemCheckBox
            | Role::MenuItemRadio
            | Role::MenuListOption
            | Role::ListBoxOption
            | Role::TreeItem
            | Role::Cell
            | Role::GridCell
            | Role::ColumnHeader
            | Role::RowHeader
            | Role::Tooltip
            | Role::Label
            | Role::TextRun
            | Role::Paragraph
            | Role::ListItem
            | Role::Status
            | Role::Alert
            | Role::Log
            | Role::Note
            | Role::Strong
            | Role::Emphasis
            | Role::Code
            | Role::Time
            | Role::Mark
            | Role::Caption
            | Role::Legend
            | Role::Term
            | Role::Definition
            | Role::Unknown
            | Role::GenericContainer
    ) || is_text_input(role)
}

/// The ARIA role for an AccessKit role. `None` leaves a plain `<div>`, which
/// the browser exposes as a generic container (or as text for a leaf).
fn aria_role(role: Role) -> Option<&'static str> {
    Some(match role {
        Role::Button | Role::DefaultButton => "button",
        Role::CheckBox => "checkbox",
        Role::Switch => "switch",
        Role::RadioButton => "radio",
        Role::RadioGroup => "radiogroup",
        Role::Link => "link",
        Role::Heading => "heading",
        Role::Paragraph => "paragraph",
        Role::List => "list",
        Role::ListItem => "listitem",
        Role::ListBox => "listbox",
        Role::ListBoxOption | Role::MenuListOption => "option",
        Role::Tab => "tab",
        Role::TabList => "tablist",
        Role::TabPanel => "tabpanel",
        Role::ProgressIndicator => "progressbar",
        Role::Meter => "meter",
        Role::Slider => "slider",
        Role::SpinButton => "spinbutton",
        Role::ScrollBar => "scrollbar",
        Role::SearchInput => "searchbox",
        Role::ComboBox | Role::EditableComboBox => "combobox",
        role if is_text_input(role) => "textbox",
        // Announced through the adapter's own live regions instead, which
        // exist before any message does.
        Role::Status | Role::Alert | Role::Log | Role::Timer => return None,
        Role::Dialog => "dialog",
        Role::AlertDialog => "alertdialog",
        Role::Toolbar => "toolbar",
        Role::Group => "group",
        Role::Region | Role::Section => "region",
        Role::Banner | Role::Header => "banner",
        Role::ContentInfo | Role::Footer => "contentinfo",
        Role::Main => "main",
        Role::Navigation => "navigation",
        Role::Complementary => "complementary",
        Role::Search => "search",
        Role::Form => "form",
        Role::Menu | Role::MenuListPopup => "menu",
        Role::MenuBar => "menubar",
        Role::MenuItem => "menuitem",
        Role::MenuItemCheckBox => "menuitemcheckbox",
        Role::MenuItemRadio => "menuitemradio",
        Role::Tree => "tree",
        Role::TreeItem => "treeitem",
        Role::TreeGrid => "treegrid",
        Role::Grid => "grid",
        Role::Table | Role::ListGrid => "table",
        Role::Row => "row",
        Role::RowGroup => "rowgroup",
        Role::Cell => "cell",
        Role::GridCell => "gridcell",
        Role::ColumnHeader => "columnheader",
        Role::RowHeader => "rowheader",
        Role::Image | Role::Canvas | Role::SvgRoot => "img",
        Role::Figure => "figure",
        Role::Tooltip => "tooltip",
        Role::Article => "article",
        Role::Document => "document",
        Role::Application => "application",
        Role::Splitter => "separator",
        Role::Note => "note",
        Role::Code => "code",
        Role::Strong => "strong",
        Role::Emphasis => "emphasis",
        Role::Term => "term",
        Role::Definition => "definition",
        Role::Math => "math",
        Role::Feed => "feed",
        Role::Marquee => "marquee",
        Role::Time => "time",
        Role::Mark => "mark",
        Role::Blockquote => "blockquote",
        Role::Caption => "caption",
        _ => return None,
    })
}
