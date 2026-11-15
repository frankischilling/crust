use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const MAX_CHAT_PANES: usize = 32;

/// The final UI layout is handed to shutdown separately from the command queue.
/// This keeps a pending search edit safe when the window closes during debounce.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneLayoutSnapshot {
    pub active_channel: String,
    pub panes: Vec<(String, f32)>,
    pub focused: usize,
    pub layout: Option<PaneLayout>,
    pub views: Vec<PaneViewSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneSearchSettings {
    pub open: bool,
    pub username: String,
    pub keyword: String,
    pub regex: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneViewSettings {
    pub search: PaneSearchSettings,
    /// None follows all enabled filters. An empty selection applies none.
    pub filters: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Split geometry refers to the ordered pane list by index. Pane contents are
/// stored separately so unavailable channels can be removed during startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout {
    Pane {
        index: usize,
    },
    Split {
        axis: SplitAxis,
        fraction: f32,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    /// Drop invalid, duplicate, and unavailable leaves, collapsing empty splits.
    /// The map converts the saved pane indices to the surviving pane order.
    pub fn remap(&self, indices: &[Option<usize>]) -> Option<Self> {
        fn visit(
            node: &PaneLayout,
            indices: &[Option<usize>],
            seen: &mut HashSet<usize>,
            depth: usize,
        ) -> Option<PaneLayout> {
            if depth >= MAX_CHAT_PANES || seen.len() >= MAX_CHAT_PANES {
                return None;
            }
            match node {
                PaneLayout::Pane { index } => {
                    let index = indices.get(*index).copied().flatten()?;
                    (index < MAX_CHAT_PANES && seen.insert(index))
                        .then_some(PaneLayout::Pane { index })
                }
                PaneLayout::Split {
                    axis,
                    fraction,
                    first,
                    second,
                } => {
                    match (
                        visit(first, indices, seen, depth + 1),
                        visit(second, indices, seen, depth + 1),
                    ) {
                        (Some(first), Some(second)) => Some(PaneLayout::Split {
                            axis: *axis,
                            fraction: if fraction.is_finite() {
                                fraction.clamp(0.001, 0.999)
                            } else {
                                0.5
                            },
                            first: Box::new(first),
                            second: Box::new(second),
                        }),
                        (Some(node), None) | (None, Some(node)) => Some(node),
                        (None, None) => None,
                    }
                }
            }
        }
        visit(self, indices, &mut HashSet::new(), 0)
    }

    pub fn validated(&self, pane_count: usize) -> Option<Self> {
        let indices: Vec<_> = (0..pane_count.min(MAX_CHAT_PANES)).map(Some).collect();
        let layout = self.remap(&indices)?;
        let mut leaves = Vec::new();
        layout.indices(&mut leaves);
        (leaves.len() == indices.len()).then_some(layout)
    }

    pub fn indices(&self, output: &mut Vec<usize>) {
        match self {
            Self::Pane { index } => output.push(*index),
            Self::Split { first, second, .. } => {
                first.indices(output);
                second.indices(output);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(first: PaneLayout, second: PaneLayout) -> PaneLayout {
        PaneLayout::Split {
            axis: SplitAxis::Vertical,
            fraction: 0.35,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn unavailable_panes_collapse_without_changing_surviving_geometry() {
        let layout = split(
            PaneLayout::Pane { index: 0 },
            split(PaneLayout::Pane { index: 1 }, PaneLayout::Pane { index: 2 }),
        );
        assert_eq!(
            layout.remap(&[None, Some(0), Some(1)]),
            Some(split(
                PaneLayout::Pane { index: 0 },
                PaneLayout::Pane { index: 1 },
            ))
        );
    }

    #[test]
    fn invalid_or_duplicate_indices_cannot_hide_a_valid_pane() {
        let layout = split(PaneLayout::Pane { index: 0 }, PaneLayout::Pane { index: 0 });
        assert!(layout.validated(2).is_none());
        assert_eq!(layout.validated(1), Some(PaneLayout::Pane { index: 0 }));
        assert!(PaneLayout::Pane { index: usize::MAX }
            .validated(2)
            .is_none());
    }
}
