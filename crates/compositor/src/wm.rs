//! Window manager v0: click to raise, drag by the title bar.
//!
//! A window is any root child with `"window": true`. Input is handled entirely
//! inside the compositor, with no round trip to the app that owns the window;
//! that is the "thick compositor" rule from the feature list.

use protocol::{NodeId, SceneOp};
use serde_json::{Value, json};

use crate::scene::{Scene, num};

pub const TITLE_BAR_HEIGHT: f32 = 36.0;

#[derive(Debug, Default)]
pub struct WindowManager {
    drag: Option<Drag>,
}

#[derive(Debug)]
struct Drag {
    window: NodeId,
    grab_dx: f32,
    grab_dy: f32,
}

impl WindowManager {
    /// Topmost window under the point, if any.
    pub fn window_at(&self, scene: &Scene, x: f32, y: f32) -> Option<NodeId> {
        scene
            .top_level()
            .iter()
            .rev()
            .copied()
            .find(|&id| is_window(scene, id) && scene.local_rect(id).is_some_and(|r| r.contains(x, y)))
    }

    pub fn pointer_down(&mut self, scene: &mut Scene, x: f32, y: f32) {
        let Some(id) = self.window_at(scene, x, y) else { return };
        scene.raise(id);
        let r = scene.local_rect(id).expect("window exists");
        let title = num(&scene.node(id).expect("window exists").props, "title_height", TITLE_BAR_HEIGHT);
        if y - r.y < title {
            self.drag = Some(Drag { window: id, grab_dx: x - r.x, grab_dy: y - r.y });
        }
    }

    pub fn pointer_move(&mut self, scene: &mut Scene, x: f32, y: f32) {
        if let Some(d) = &self.drag {
            let props = json!({"x": (x - d.grab_dx).round(), "y": (y - d.grab_dy).round()});
            scene.apply(SceneOp::Set {
                id: d.window,
                props: props.as_object().expect("object literal").clone(),
            });
        }
    }

    /// End any drag. Returns the window that was being dragged.
    pub fn pointer_up(&mut self) -> Option<NodeId> {
        self.drag.take().map(|d| d.window)
    }

    /// Raise the bottom-most window, so repeating this visits every window.
    /// Does nothing during a drag. Returns the window it raised.
    pub fn raise_next(&mut self, scene: &mut Scene) -> Option<NodeId> {
        if self.drag.is_some() {
            return None;
        }
        let bottom = scene.top_level().iter().copied().find(|&id| is_window(scene, id))?;
        scene.raise(bottom);
        Some(bottom)
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
}

/// Whether root child `id` is a window on the current workspace (others are hidden).
pub fn is_window(scene: &Scene, id: NodeId) -> bool {
    scene.node(id).is_some_and(|n| {
        n.props.get("window").and_then(Value::as_bool) == Some(true)
            && n.props.get("hidden").and_then(Value::as_bool) != Some(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::ROOT;
    use protocol::NodeKind;

    fn window(id: NodeId, x: f32, y: f32) -> [SceneOp; 2] {
        let props = json!({"window": true, "x": x, "y": y, "w": 100, "h": 100});
        [
            SceneOp::Create { id, kind: NodeKind::Group, props: props.as_object().unwrap().clone() },
            SceneOp::Insert { parent: ROOT, child: id, index: usize::MAX },
        ]
    }

    #[test]
    fn click_raises_and_title_drag_moves() {
        let mut scene = Scene::new();
        scene.apply_all(window(1, 0.0, 0.0));
        scene.apply_all(window(2, 50.0, 50.0));
        let mut wm = WindowManager::default();

        // Overlap region belongs to the top window (2); clicking window 1 raises it.
        assert_eq!(wm.window_at(&scene, 60.0, 60.0), Some(2));
        wm.pointer_down(&mut scene, 10.0, 10.0);
        assert_eq!(scene.top_level(), &[2, 1]);
        assert!(wm.is_dragging());

        wm.pointer_move(&mut scene, 30.0, 25.0);
        wm.pointer_up();
        let r = scene.local_rect(1).unwrap();
        assert_eq!((r.x, r.y), (20.0, 15.0));
    }

    #[test]
    fn clicking_the_body_raises_without_dragging() {
        let mut scene = Scene::new();
        scene.apply_all(window(1, 0.0, 0.0));
        let mut wm = WindowManager::default();
        wm.pointer_down(&mut scene, 10.0, 80.0);
        assert!(!wm.is_dragging());
    }
}
