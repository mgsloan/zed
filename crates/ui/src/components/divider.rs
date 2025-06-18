use gpui::{Hsla, IntoElement};

use crate::prelude::*;

pub fn divider() -> Divider {
    Divider {
        style: DividerStyle::Solid,
        direction: DividerDirection::Horizontal,
        color: DividerColor::default(),
        inset: false,
    }
}

pub fn vertical_divider() -> Divider {
    Divider {
        style: DividerStyle::Solid,
        direction: DividerDirection::Vertical,
        color: DividerColor::default(),
        inset: false,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DividerStyle {
    Solid,
    Dashed,
}

#[derive(Clone, Copy, PartialEq)]
enum DividerDirection {
    Horizontal,
    Vertical,
}

/// The color of a [`Divider`].
#[derive(Default)]
pub enum DividerColor {
    Border,
    #[default]
    BorderVariant,
}

impl DividerColor {
    pub fn hsla(self, cx: &mut App) -> Hsla {
        match self {
            DividerColor::Border => cx.theme().colors().border,
            DividerColor::BorderVariant => cx.theme().colors().border_variant,
        }
    }
}

#[derive(IntoElement, RegisterComponent)]
pub struct Divider {
    style: DividerStyle,
    direction: DividerDirection,
    color: DividerColor,
    inset: bool,
}

impl RenderOnce for Divider {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .map(|this| match self.direction {
                DividerDirection::Horizontal => this
                    .border_t_1()
                    .w_full()
                    .h_0()
                    .when(self.inset, |this| this.mx_1p5()),
                DividerDirection::Vertical => this
                    .border_l_1()
                    .h_full()
                    .w_0()
                    .when(self.inset, |this| this.my_1p5()),
            })
            .map(|this| match self.style {
                DividerStyle::Solid => this,
                DividerStyle::Dashed => this.border_dashed(),
            })
            .border_color(self.color.hsla(cx))
    }
}

impl Divider {
    pub fn horizontal() -> Self {
        Self {
            style: DividerStyle::Solid,
            direction: DividerDirection::Horizontal,
            color: DividerColor::default(),
            inset: false,
        }
    }

    pub fn vertical() -> Self {
        Self {
            style: DividerStyle::Solid,
            direction: DividerDirection::Vertical,
            color: DividerColor::default(),
            inset: false,
        }
    }

    pub fn horizontal_dashed() -> Self {
        Self {
            style: DividerStyle::Dashed,
            direction: DividerDirection::Horizontal,
            color: DividerColor::default(),
            inset: false,
        }
    }

    pub fn vertical_dashed() -> Self {
        Self {
            style: DividerStyle::Dashed,
            direction: DividerDirection::Vertical,
            color: DividerColor::default(),
            inset: false,
        }
    }

    pub fn inset(mut self) -> Self {
        self.inset = true;
        self
    }

    pub fn color(mut self, color: DividerColor) -> Self {
        self.color = color;
        self
    }
}

impl Component for Divider {
    fn scope() -> ComponentScope {
        ComponentScope::Layout
    }

    fn description() -> Option<&'static str> {
        Some(
            "Visual separator used to create divisions between groups of content or sections in a layout.",
        )
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        Some(
            v_flex()
                .gap_6()
                .children(vec![
                    example_group_with_title(
                        "Horizontal Dividers",
                        vec![
                            single_example("Default", Divider::horizontal().into_any_element()),
                            single_example(
                                "Border Color",
                                Divider::horizontal()
                                    .color(DividerColor::Border)
                                    .into_any_element(),
                            ),
                            single_example(
                                "Inset",
                                Divider::horizontal().inset().into_any_element(),
                            ),
                            single_example(
                                "Dashed",
                                Divider::horizontal_dashed().into_any_element(),
                            ),
                        ],
                    ),
                    example_group_with_title(
                        "Vertical Dividers",
                        vec![
                            single_example(
                                "Default",
                                div().h_16().child(Divider::vertical()).into_any_element(),
                            ),
                            single_example(
                                "Border Color",
                                div()
                                    .h_16()
                                    .child(Divider::vertical().color(DividerColor::Border))
                                    .into_any_element(),
                            ),
                            single_example(
                                "Inset",
                                div()
                                    .h_16()
                                    .child(Divider::vertical().inset())
                                    .into_any_element(),
                            ),
                            single_example(
                                "Dashed",
                                div()
                                    .h_16()
                                    .child(Divider::vertical_dashed())
                                    .into_any_element(),
                            ),
                        ],
                    ),
                    example_group_with_title(
                        "Example Usage",
                        vec![single_example(
                            "Between Content",
                            v_flex()
                                .gap_4()
                                .px_4()
                                .child(Label::new("Section One"))
                                .child(Divider::horizontal())
                                .child(Label::new("Section Two"))
                                .child(Divider::horizontal_dashed())
                                .child(Label::new("Section Three"))
                                .into_any_element(),
                        )],
                    ),
                ])
                .into_any_element(),
        )
    }
}
