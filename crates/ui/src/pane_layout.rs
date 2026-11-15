use crust_core::model::{PaneLayout, SplitAxis};
use egui::{pos2, vec2, Rect, Vec2};

const DIVIDER: f32 = 1.0;
const MIN_PANE_SIZE: Vec2 = vec2(96.0, 110.0);

#[derive(Clone, Debug)]
pub(super) enum LayoutNode {
    Pane(u64),
    Split {
        axis: SplitAxis,
        fraction: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ReopenPosition {
    pub sibling: u64,
    pub siblings: Vec<u64>,
    pub axis: SplitAxis,
    pub fraction: f32,
    pub before: bool,
}

pub(super) struct Divider {
    pub path: Vec<bool>,
    pub axis: SplitAxis,
    pub line: Rect,
    pub container: Rect,
    pub min_fraction: f32,
    pub max_fraction: f32,
}

impl LayoutNode {
    pub fn from_saved(saved: &PaneLayout, ids: &[u64]) -> Option<Self> {
        match saved {
            PaneLayout::Pane { index } => ids.get(*index).copied().map(Self::Pane),
            PaneLayout::Split {
                axis,
                fraction,
                first,
                second,
            } => Some(Self::Split {
                axis: *axis,
                fraction: *fraction,
                first: Box::new(Self::from_saved(first, ids)?),
                second: Box::new(Self::from_saved(second, ids)?),
            }),
        }
    }

    pub fn saved(&self, ids: &[u64]) -> Option<PaneLayout> {
        match self {
            Self::Pane(id) => ids
                .iter()
                .position(|pane| pane == id)
                .map(|index| PaneLayout::Pane { index }),
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => Some(PaneLayout::Split {
                axis: *axis,
                fraction: *fraction,
                first: Box::new(first.saved(ids)?),
                second: Box::new(second.saved(ids)?),
            }),
        }
    }

    pub fn horizontal(panes: &[(u64, f32)]) -> Option<Self> {
        let (&(id, _), rest) = panes.split_first()?;
        if rest.is_empty() {
            return Some(Self::Pane(id));
        }
        let weight = |weight: f32| {
            if weight.is_finite() && weight > 0.0 {
                f64::from(weight)
            } else {
                1.0
            }
        };
        let total: f64 = panes.iter().map(|(_, frac)| weight(*frac)).sum();
        Some(Self::Split {
            axis: SplitAxis::Horizontal,
            fraction: (weight(panes[0].1) / total) as f32,
            first: Box::new(Self::Pane(id)),
            second: Box::new(Self::horizontal(rest)?),
        })
    }

    pub fn first_pane(&self) -> u64 {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }

    pub fn contains(&self, pane: u64) -> bool {
        match self {
            Self::Pane(id) => *id == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    pub fn insert(&mut self, pane: u64, position: ReopenPosition) -> bool {
        let mut ids = Vec::new();
        self.pane_ids(&mut ids);
        let wrap = matches!(self, Self::Pane(id) if *id == position.sibling)
            || (ids.len() > 1 && ids == position.siblings);
        if wrap {
            let previous = std::mem::replace(self, Self::Pane(pane));
            let new = Self::Pane(pane);
            let (first, second) = if position.before {
                (new, previous)
            } else {
                (previous, new)
            };
            *self = Self::Split {
                axis: position.axis,
                fraction: position.fraction,
                first: Box::new(first),
                second: Box::new(second),
            };
            return true;
        }
        match self {
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.insert(pane, position.clone()) || second.insert(pane, position)
            }
        }
    }

    fn pane_ids(&self, ids: &mut Vec<u64>) {
        match self {
            Self::Pane(id) => ids.push(*id),
            Self::Split { first, second, .. } => {
                first.pane_ids(ids);
                second.pane_ids(ids);
            }
        }
    }

    pub fn position(&self, pane: u64) -> Option<ReopenPosition> {
        match self {
            Self::Pane(_) => None,
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => {
                if matches!(first.as_ref(), Self::Pane(id) if *id == pane) {
                    let mut siblings = Vec::new();
                    second.pane_ids(&mut siblings);
                    Some(ReopenPosition {
                        sibling: second.first_pane(),
                        siblings,
                        axis: *axis,
                        fraction: *fraction,
                        before: true,
                    })
                } else if matches!(second.as_ref(), Self::Pane(id) if *id == pane) {
                    let mut siblings = Vec::new();
                    first.pane_ids(&mut siblings);
                    Some(ReopenPosition {
                        sibling: first.first_pane(),
                        siblings,
                        axis: *axis,
                        fraction: *fraction,
                        before: false,
                    })
                } else {
                    first.position(pane).or_else(|| second.position(pane))
                }
            }
        }
    }

    pub fn without(self, pane: u64) -> Option<Self> {
        match self {
            Self::Pane(id) => (id != pane).then_some(Self::Pane(id)),
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => match (first.without(pane), second.without(pane)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    fraction,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }

    pub fn swap(&mut self, a: u64, b: u64) {
        match self {
            Self::Pane(id) => {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
            Self::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    fn min_size(&self) -> Vec2 {
        match self {
            Self::Pane(_) => MIN_PANE_SIZE,
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let first = first.min_size();
                let second = second.min_size();
                match axis {
                    SplitAxis::Horizontal => {
                        vec2(first.x + second.x + DIVIDER, first.y.max(second.y))
                    }
                    SplitAxis::Vertical => {
                        vec2(first.x.max(second.x), first.y + second.y + DIVIDER)
                    }
                }
            }
        }
    }

    pub fn geometry(&self, rect: Rect) -> (Vec<(u64, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        self.place(rect, &mut Vec::new(), &mut panes, &mut dividers);
        (panes, dividers)
    }

    fn place(
        &self,
        rect: Rect,
        path: &mut Vec<bool>,
        panes: &mut Vec<(u64, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Pane(id) => panes.push((*id, rect)),
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => {
                let (length, first_min, second_min) = match axis {
                    SplitAxis::Horizontal => {
                        (rect.width(), first.min_size().x, second.min_size().x)
                    }
                    SplitAxis::Vertical => (rect.height(), first.min_size().y, second.min_size().y),
                };
                let available = (length - DIVIDER).max(0.0);
                let (min_fraction, max_fraction) = if available > first_min + second_min {
                    (first_min / available, 1.0 - second_min / available)
                } else {
                    let balance = first_min / (first_min + second_min);
                    (balance, balance)
                };
                let fraction = fraction.clamp(min_fraction, max_fraction);
                let offset = available * fraction;
                let divider_size = DIVIDER.min(length.max(0.0));
                let (first_rect, line, second_rect) = match axis {
                    SplitAxis::Horizontal => {
                        let x = rect.left() + offset;
                        (
                            Rect::from_min_max(rect.min, pos2(x, rect.bottom())),
                            Rect::from_min_max(
                                pos2(x, rect.top()),
                                pos2(x + divider_size, rect.bottom()),
                            ),
                            Rect::from_min_max(pos2(x + divider_size, rect.top()), rect.max),
                        )
                    }
                    SplitAxis::Vertical => {
                        let y = rect.top() + offset;
                        (
                            Rect::from_min_max(rect.min, pos2(rect.right(), y)),
                            Rect::from_min_max(
                                pos2(rect.left(), y),
                                pos2(rect.right(), y + divider_size),
                            ),
                            Rect::from_min_max(pos2(rect.left(), y + divider_size), rect.max),
                        )
                    }
                };
                dividers.push(Divider {
                    path: path.clone(),
                    axis: *axis,
                    line,
                    container: rect,
                    min_fraction,
                    max_fraction,
                });
                path.push(false);
                first.place(first_rect, path, panes, dividers);
                *path.last_mut().unwrap() = true;
                second.place(second_rect, path, panes, dividers);
                path.pop();
            }
        }
    }

    pub fn resize(&mut self, path: &[bool], fraction: f32) {
        let Self::Split {
            fraction: current,
            first,
            second,
            ..
        } = self
        else {
            return;
        };
        if let Some((&go_second, rest)) = path.split_first() {
            if go_second {
                second.resize(rest, fraction);
            } else {
                first.resize(rest, fraction);
            }
        } else if fraction.is_finite() {
            *current = fraction.clamp(0.01, 0.99);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_layout_tiles_both_axes_and_retains_stable_pane_ids() {
        let mut layout = LayoutNode::horizontal(&[(10, 0.4), (20, 0.6)]).unwrap();
        assert!(layout.insert(
            30,
            ReopenPosition {
                sibling: 20,
                siblings: vec![20],
                axis: SplitAxis::Vertical,
                fraction: 0.5,
                before: false
            }
        ));
        let rect = Rect::from_min_size(pos2(12.0, 20.0), vec2(1000.0, 700.0));
        let (panes, dividers) = layout.geometry(rect);
        assert_eq!(
            panes.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [10, 20, 30]
        );
        assert_eq!(panes[0].1.height(), rect.height());
        assert_eq!(panes[1].1.left(), panes[2].1.left());
        assert_eq!(panes[1].1.bottom() + 1.0, panes[2].1.top());
        assert_eq!(panes[2].1.max, rect.max);
        assert_eq!(dividers.len(), 2);
        for (_, pane) in &panes {
            assert!(rect.contains_rect(*pane));
        }
        layout.swap(10, 30);
        assert_eq!(layout.geometry(rect).0[0].0, 30);
    }

    #[test]
    fn reopening_a_closed_nested_pane_restores_its_axis_and_fraction() {
        let mut layout = LayoutNode::horizontal(&[(10, 0.4), (20, 0.6)]).unwrap();
        layout.insert(
            30,
            ReopenPosition {
                sibling: 20,
                siblings: vec![20],
                axis: SplitAxis::Vertical,
                fraction: 0.65,
                before: false,
            },
        );
        let before = layout.saved(&[10, 20, 30]);
        let position = layout.position(30).unwrap();
        let mut layout = layout.without(30).unwrap();
        assert!(layout.insert(30, position));
        assert_eq!(layout.saved(&[10, 20, 30]), before);
        let position = layout.position(10).unwrap();
        let mut layout = layout.without(10).unwrap();
        assert!(layout.insert(10, position));
        assert_eq!(
            layout.saved(&[10, 20, 30]),
            before,
            "a whole neighboring split group remains intact when the pane is reopened"
        );
    }

    #[test]
    fn tiny_windows_keep_every_pane_rect_nonnegative() {
        let mut layout = LayoutNode::horizontal(&[(10, 0.01), (20, 0.99)]).unwrap();
        layout.insert(
            30,
            ReopenPosition {
                sibling: 20,
                siblings: vec![20],
                axis: SplitAxis::Vertical,
                fraction: 0.95,
                before: false,
            },
        );
        for size in [
            vec2(0.0, 0.0),
            vec2(1.0, 1.0),
            vec2(60.0, 40.0),
            vec2(500.0, 120.0),
        ] {
            let rect = Rect::from_min_size(pos2(5.0, 10.0), size);
            for (_, pane) in layout.geometry(rect).0 {
                assert!(pane.width() >= 0.0 && pane.height() >= 0.0);
                assert!(rect.contains_rect(pane));
            }
        }
    }

    #[test]
    fn legacy_widths_survive_conversion_to_nested_geometry() {
        let ids = [1, 2, 3, 4];
        let layout = LayoutNode::horizontal(&[(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.4)]).unwrap();
        let saved = layout.saved(&ids).unwrap();
        let restored = LayoutNode::from_saved(&saved, &ids).unwrap();
        let panes = restored
            .geometry(Rect::from_min_size(pos2(0.0, 0.0), vec2(2000.0, 700.0)))
            .0;
        for ((_, rect), expected) in panes.iter().zip([0.1, 0.2, 0.3, 0.4]) {
            assert!((rect.width() / 1997.0 - expected).abs() < 0.001);
        }
    }
}
