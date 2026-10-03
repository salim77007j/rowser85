// Minimal taffy probe: does an anonymous-block leaf (our exact leaf style)
// stretch to the containing block, or shrink to its measured content?
use taffy::prelude::*;
use taffy::tree::{Baselines, CollapsibleMarginSet};
use taffy::{AvailableSpace, LayoutInput, LayoutOutput, RunMode, SizingMode, Style, TaffyTree};

fn main() {
    for (label, overflow_hidden) in [("overflow:Hidden", true), ("overflow:Visible", false)] {
        let mut tree: TaffyTree<()> = TaffyTree::new();
        let value = if overflow_hidden {
            taffy::Overflow::Hidden
        } else {
            taffy::Overflow::Visible
        };
        let ov = move || taffy::geometry::Point { x: value, y: value };
        let leaf_style = Style {
            display: Display::Block,
            overflow: ov(),
            align_self: Some(AlignSelf::START),
            ..Style::default()
        };
        let leaf = tree.new_leaf(leaf_style).unwrap();
        let container = tree
            .new_with_children(
                Style {
                    display: Display::Block,
                    ..Style::default()
                },
                &[leaf],
            )
            .unwrap();
        tree.compute_layout_with_measure(
            container,
            Size {
                width: AvailableSpace::Definite(1360.0),
                height: AvailableSpace::Definite(860.0),
            },
            |input: LayoutInput,
             _node: taffy::NodeId,
             _ctx: Option<&mut ()>,
             _style: &Style|
             -> LayoutOutput {
                let width = match input.known_dimensions.width {
                    Some(w) => Some(w.max(0.0)),
                    None => match input.available_space.width {
                        AvailableSpace::Definite(w) => Some(w.max(0.0)),
                        _ => None,
                    },
                };
                let _ = RunMode::PerformLayout;
                let _ = SizingMode::InherentSize;
                // pretend the text is 699.4 wide, 28 tall, one line
                let w = width.unwrap_or(699.4);
                LayoutOutput {
                    size: Size {
                        width: w.min(699.4),
                        height: 28.0,
                    },
                    scrollable_overflow_rect: taffy::geometry::Rect::ZERO,
                    baselines: Baselines::NONE,
                    top_margin: CollapsibleMarginSet::ZERO,
                    bottom_margin: CollapsibleMarginSet::ZERO,
                    margins_can_collapse_through: false,
                }
            },
        )
        .unwrap();
        // Root margin behavior probe: container WITH margins as layout root.
        {
            let mut tree2: TaffyTree<()> = TaffyTree::new();
            let leaf2 = tree2
                .new_leaf(Style {
                    display: Display::Block,
                    overflow: ov(),
                    align_self: Some(AlignSelf::START),
                    ..Style::default()
                })
                .unwrap();
            let container2 = tree2
                .new_with_children(
                    Style {
                        display: Display::Block,
                        margin: Rect {
                            top: LengthPercentage::length(24.0).into(),
                            right: LengthPercentage::length(24.0).into(),
                            bottom: LengthPercentage::length(24.0).into(),
                            left: LengthPercentage::length(24.0).into(),
                        },
                        ..Style::default()
                    },
                    &[leaf2],
                )
                .unwrap();
            tree2
                .compute_layout_with_measure(
                    container2,
                    Size {
                        width: AvailableSpace::Definite(1360.0),
                        height: AvailableSpace::Definite(860.0),
                    },
                    |input: LayoutInput,
                     _n: taffy::NodeId,
                     _c: Option<&mut ()>,
                     _s: &Style|
                     -> LayoutOutput {
                        let w = match input.known_dimensions.width {
                            Some(w) => Some(w.max(0.0)),
                            None => match input.available_space.width {
                                AvailableSpace::Definite(w) => Some(w.max(0.0)),
                                _ => None,
                            },
                        };
                        LayoutOutput {
                            size: Size {
                                width: w.unwrap_or(699.4).min(699.4),
                                height: 28.0,
                            },
                            scrollable_overflow_rect: taffy::geometry::Rect::ZERO,
                            baselines: Baselines::NONE,
                            top_margin: CollapsibleMarginSet::ZERO,
                            bottom_margin: CollapsibleMarginSet::ZERO,
                            margins_can_collapse_through: false,
                        }
                    },
                )
                .unwrap();
            let c2 = tree2.layout(container2).unwrap();
            let l2 = tree2.layout(leaf2).unwrap();
            println!(
                "root-margins: root loc=({:.1},{:.1}) size={:.1}x{:.1}; leaf loc=({:.1},{:.1}) size={:.1}x{:.1}",
                c2.location.x, c2.location.y, c2.size.width, c2.size.height,
                l2.location.x, l2.location.y, l2.size.width, l2.size.height
            );
        }
        let l = tree.layout(leaf).unwrap();
        let c = tree.layout(container).unwrap();
        println!(
            "{label}: leaf final = {:.1}x{:.1} @ ({:.1},{:.1}); container = {:.1}x{:.1}",
            l.size.width, l.size.height, l.location.x, l.location.y, c.size.width, c.size.height
        );
    }
}
