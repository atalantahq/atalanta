use std::sync::Arc;

use velocity_common::Element;

use crate::config::HeadlessConfig;
use crate::element_map::{element_path, render_tree_to_element_on_screen};
use crate::layout::LayoutEngine;
use crate::render_tree::RenderNode;
use crate::scroll::ScrollState;
use crate::surface::SoftwareSurface;
use crate::text::TextMeasurer;

/// Per-device headless session state.
///
/// Holds the current render tree, surface, and layout engine
/// for a single virtual device.
pub struct HeadlessSession {
    /// Session configuration.
    pub config: HeadlessConfig,
    /// Current render tree (set after layout inflation), laid out unscrolled.
    pub render_tree: Option<RenderNode>,
    /// Current element tree (derived from render tree after layout).
    pub element_tree: Option<Element>,
    /// Pixel surface for rendering.
    pub surface: SoftwareSurface,
    /// Layout engine instance.
    pub layout_engine: LayoutEngine,
    /// Shared text measurer for glyph rendering.
    pub text_measurer: Arc<TextMeasurer>,
    /// Installed app path.
    pub app_path: Option<String>,
    /// Whether the session has performed initial layout.
    pub layout_computed: bool,
    /// Scroll offsets, applied to the render tree for the hierarchy and screenshots.
    pub scroll: ScrollState,
    /// Bumped on every redraw; what it was at the last settle tells whether
    /// the screen reacted since (`take_reacted`).
    redraws: u64,
    settled_redraws: u64,
}

impl HeadlessSession {
    /// Create a new session with the given config and shared text measurer.
    pub fn new(
        config: HeadlessConfig,
        text_measurer: Arc<TextMeasurer>,
    ) -> Result<Self, SessionError> {
        let surface = SoftwareSurface::new(config.width, config.height)
            .map_err(|e| SessionError::SurfaceCreation(e.to_string()))?;

        Ok(Self {
            config,
            render_tree: None,
            element_tree: None,
            surface,
            layout_engine: LayoutEngine::with_measurer(text_measurer.clone()),
            text_measurer,
            app_path: None,
            layout_computed: false,
            scroll: ScrollState::default(),
            redraws: 0,
            settled_redraws: 0,
        })
    }

    /// Show a freshly inflated tree: its text scaled by `fontScale` (D-47),
    /// then laid out and rendered.
    pub fn load_tree(&mut self, mut tree: RenderNode) -> Result<(), SessionError> {
        let scale = self.config.font_scale;
        if scale.is_finite() && scale > 0.0 && scale != 1.0 {
            scale_fonts(&mut tree, scale);
        }
        self.set_render_tree(tree)
    }

    /// Set the render tree and compute layout + render.
    pub fn set_render_tree(&mut self, mut tree: RenderNode) -> Result<(), SessionError> {
        // Compute layout
        self.layout_engine
            .compute_layout(
                &mut tree,
                self.config.width as f32,
                self.config.height as f32,
            )
            .map_err(|e| SessionError::LayoutFailed(e.to_string()))?;

        // Store render tree
        self.render_tree = Some(tree);
        self.layout_computed = true;
        self.show();

        Ok(())
    }

    /// Render the laid-out tree at the current scroll offsets, and derive the
    /// element tree from what is shown.
    fn show(&mut self) {
        let Some(tree) = &self.render_tree else {
            return;
        };
        self.redraws += 1;
        let (width, height) = (self.config.width as f32, self.config.height as f32);
        self.scroll.clamp(tree, (width, height));
        let shown = self.scroll.apply(tree);

        // Render to surface with glyph rendering
        self.surface.render_tree(&shown, Some(&self.text_measurer));

        // Convert to Element tree
        self.element_tree = Some(render_tree_to_element_on_screen(&shown, width, height));
    }

    /// A finger dragged from `from` to `to`: scrolls the innermost scroll container
    /// under `from` (then its parents, then the window) with the finger, and
    /// re-renders. Returns whether anything moved.
    pub fn scroll(&mut self, from: (i32, i32), to: (i32, i32)) -> Result<bool, SessionError> {
        let tree = self.render_tree.as_ref().ok_or(SessionError::NoLayout)?;
        let screen = (self.config.width as f32, self.config.height as f32);
        let moved = self.scroll.drag(
            tree,
            screen,
            (from.0 as f32, from.1 as f32),
            (to.0 as f32, to.1 as f32),
        );
        if moved {
            self.show();
        }
        Ok(moved)
    }

    /// Scroll everything back to the top-left (a launch) and re-render.
    pub fn reset_scroll(&mut self) {
        self.scroll.reset();
        self.show();
    }

    /// The app stopped: the screen is empty until the next launch (VEL-357).
    pub fn stop(&mut self) {
        self.redraws += 1;
        let (width, height) = (self.config.width as i32, self.config.height as i32);
        self.render_tree = None;
        self.layout_computed = false;
        self.scroll.reset();
        self.surface.clear(crate::render_tree::Color::WHITE);
        self.element_tree = Some(Element {
            hint: None,
            platform_id: String::new(),
            label: None,
            text: None,
            element_type: "Screen".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            enabled: true,
            visible: true,
            children: Vec::new(),
            ..Default::default()
        });
    }

    /// Whether the screen was redrawn since the last call.
    pub fn take_reacted(&mut self) -> bool {
        let reacted = self.redraws != self.settled_redraws;
        self.settled_redraws = self.redraws;
        reacted
    }

    /// Get the current element hierarchy.
    pub fn get_hierarchy(&self) -> Option<&Element> {
        self.element_tree.as_ref()
    }

    /// Get a screenshot as PNG bytes.
    pub fn screenshot(&self) -> Result<Vec<u8>, SessionError> {
        self.surface
            .encode_png()
            .map_err(|e| SessionError::ScreenshotFailed(e.to_string()))
    }

    /// Get screen dimensions.
    pub fn screen_size(&self) -> (i32, i32) {
        (self.config.width as i32, self.config.height as i32)
    }

    /// Set the text of the node `element` is (located by its place in the
    /// tree, not its id: two fields may share an id, VEL-339) and re-render.
    pub fn update_text(&mut self, element: &Element, new_text: &str) -> Result<(), SessionError> {
        self.edit_text(element, new_text, false)
    }

    /// Type `typed` after what the field already holds, as the device
    /// drivers do (D-16, VEL-348). A placeholder shown in an empty field is
    /// not part of its text.
    pub fn append_text(&mut self, element: &Element, typed: &str) -> Result<(), SessionError> {
        self.edit_text(element, typed, true)
    }

    fn edit_text(
        &mut self,
        element: &Element,
        new_text: &str,
        append: bool,
    ) -> Result<(), SessionError> {
        let path = self
            .element_tree
            .as_ref()
            .and_then(|tree| element_path(tree, element))
            .ok_or_else(|| SessionError::ElementNotFound(element.platform_id.clone()))?;
        let mut tree = self.render_tree.take().ok_or(SessionError::NoLayout)?;
        let mut node = &mut tree;
        for &i in &path {
            match node.children.get_mut(i) {
                Some(child) => node = child,
                None => {
                    self.render_tree = Some(tree);
                    return Err(SessionError::ElementNotFound(element.platform_id.clone()));
                }
            }
        }
        let before = match &node.text {
            Some(t) if append && !node.style.placeholder => t.clone(),
            _ => String::new(),
        };
        node.text = Some(if node.style.password {
            // Bullets already stand for one character each.
            before + &"•".repeat(new_text.chars().count())
        } else {
            before + new_text
        });
        node.style.placeholder = false;

        // Re-layout and re-render
        self.set_render_tree(tree)
    }
}

fn scale_fonts(node: &mut RenderNode, scale: f32) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        n.style.font_size *= scale;
        stack.extend(n.children.iter_mut());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Failed to create rendering surface: {0}")]
    SurfaceCreation(String),
    #[error("Layout computation failed: {0}")]
    LayoutFailed(String),
    #[error("Screenshot encoding failed: {0}")]
    ScreenshotFailed(String),
    #[error("No layout has been loaded")]
    NoLayout,
    #[error("Element not found: {0}")]
    ElementNotFound(String),
}
