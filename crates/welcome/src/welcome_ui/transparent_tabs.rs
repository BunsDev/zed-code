use std::sync::OnceLock;

use gpui::Entity;
use ui::{IntoElement, RenderOnce, component_prelude::Documented, prelude::*};

/// The tabs in the Zed walkthrough
#[derive(IntoElement, RegisterComponent, Documented)]
pub struct TransparentTabs {
    selected: Entity<usize>,
    tabs: Vec<Tab>,
}

struct Tab {
    tab_title: String,
    content: Option<Box<dyn Fn(&mut ui::Window, &mut ui::App) -> AnyElement>>,
}

impl TransparentTabs {
    pub fn new(selected: Entity<usize>) -> Self {
        Self {
            selected,
            tabs: Vec::new(),
        }
    }

    pub fn tab<R: IntoElement>(
        mut self,
        tab_title: &str,
        content: impl Fn(&mut ui::Window, &mut ui::App) -> R + 'static,
    ) -> Self {
        self.tabs.push(Tab {
            tab_title: tab_title.to_owned(),
            content: Some(Box::new(move |window, cx| {
                content(window, cx).into_any_element()
            })),
        });
        self
    }
}

impl RenderOnce for TransparentTabs {
    fn render(mut self, window: &mut ui::Window, cx: &mut ui::App) -> impl IntoElement {
        let content = self.tabs[*self.selected.read(cx)].content.take().unwrap();
        let selected = *self.selected.read(cx);
        v_flex()
            .child(
                h_flex()
                    .children(self.tabs.into_iter().enumerate().map(|(i, t)| {
                        // using index was causing id collisions with the content from that tab...
                        // should probably do something more robust for that
                        Button::new(i + 100, t.tab_title)
                            .toggle_state(i == selected)
                            // .when(i==selected, this.bg(cx.theme().colors().element_selected))
                            .selected_style(ButtonStyle::Filled)
                            .on_click({
                                let selected = self.selected.clone();
                                move |_, _window, cx| {
                                    selected.update(cx, |selected, cx| {
                                        *selected = i;
                                        cx.notify();
                                    })
                                }
                            })
                    }))
                    .flex_grow()
                    .justify_center(),
            )
            .child(div().child((content)(window, cx)).size_full())
    }
}

impl Component for TransparentTabs {
    fn description() -> Option<&'static str> {
        Some(Self::DOCS)
    }

    fn preview(_window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        static SELECTED: OnceLock<Entity<usize>> = OnceLock::new();
        let selected = SELECTED.get_or_init(|| cx.new(|_| 0)).clone();

        let tabs = TransparentTabs::new(selected)
            .tab("Tab 1", |_window, _cx| div().size_10().bg(gpui::red()))
            .tab("Tab 2", |_window, _cx| div().size_10().bg(gpui::blue()))
            .tab("Tab 3", |_window, _cx| div().size_10().bg(gpui::green()));

        Some(
            v_flex()
                .gap_6()
                .p_4()
                .children({
                    vec![example_group(vec![single_example(
                        "Default",
                        div().child(tabs).into_any_element(),
                    )])]
                })
                .into_any_element(),
        )
    }
}
