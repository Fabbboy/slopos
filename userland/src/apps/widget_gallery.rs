use slopos_appkit::{
    Action, App, ButtonStyle, CrossAxisAlignment, EdgeInsets, Length, Node, ScrollDirection,
    ScrollbarVisibility, TextAlignment,
};

#[derive(Clone, Debug)]
pub enum GalleryMsg {
    ButtonClicked,
    ToggleCheck,
    TextChanged(String),
    TabChanged(usize),
    ListSelect(usize),
}

pub struct WidgetGalleryApp {
    counter: u32,
    checked: bool,
    text: String,
    active_tab: usize,
    selected_item: Option<usize>,
}

impl WidgetGalleryApp {
    pub fn new() -> Self {
        Self {
            counter: 0,
            checked: false,
            text: String::new(),
            active_tab: 0,
            selected_item: None,
        }
    }

    fn basics_tab(&self) -> Node<GalleryMsg> {
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Widget Gallery"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::Divider,
                    Node::HStack {
                        spacing: 8,
                        align: CrossAxisAlignment::Center,
                        children: vec![
                            Node::Button {
                                label: format!("Click me ({})", self.counter),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Primary,
                                enabled: true,
                            },
                            Node::Button {
                                label: String::from("Disabled"),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Secondary,
                                enabled: false,
                            },
                            Node::Button {
                                label: String::from("Delete"),
                                on_press: Some(GalleryMsg::ButtonClicked),
                                style: ButtonStyle::Destructive,
                                enabled: true,
                            },
                        ],
                    },
                    Node::Checkbox {
                        checked: self.checked,
                        label: String::from("Enable feature"),
                        on_toggle: Some(GalleryMsg::ToggleCheck),
                        enabled: true,
                    },
                    Node::Spacer {
                        size: Length::Px(16),
                    },
                    Node::Label {
                        text: format!("Button clicked {} times", self.counter),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                ],
            }),
        }
    }

    fn input_tab(&self) -> Node<GalleryMsg> {
        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Text Input"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::TextField {
                        text: self.text.clone(),
                        placeholder: String::from("Type here..."),
                        on_change: Some(GalleryMsg::TextChanged),
                        max_length: None,
                        read_only: false,
                    },
                    Node::Label {
                        text: format!("Current value: {}", self.text),
                        alignment: TextAlignment::Start,
                        wrap: true,
                        max_lines: None,
                    },
                ],
            }),
        }
    }

    fn lists_tab(&self) -> Node<GalleryMsg> {
        let items: Vec<Node<GalleryMsg>> = (0..50)
            .map(|i| Node::Label {
                text: format!("Item {}", i),
                alignment: TextAlignment::Start,
                wrap: false,
                max_lines: None,
            })
            .collect();

        Node::Padding {
            padding: EdgeInsets::all(16),
            child: Box::new(Node::VStack {
                spacing: 8,
                align: CrossAxisAlignment::Start,
                children: vec![
                    Node::Label {
                        text: String::from("Scrollable List"),
                        alignment: TextAlignment::Start,
                        wrap: false,
                        max_lines: None,
                    },
                    Node::ScrollView {
                        direction: ScrollDirection::Vertical,
                        show_scrollbar: ScrollbarVisibility::WhenNeeded,
                        scroll_y: 0,
                        on_scroll: None,
                        child: Box::new(Node::VStack {
                            spacing: 4,
                            align: CrossAxisAlignment::Start,
                            children: items,
                        }),
                    },
                ],
            }),
        }
    }
}

impl App for WidgetGalleryApp {
    type Message = GalleryMsg;

    fn view(&self) -> Node<GalleryMsg> {
        Node::TabBar {
            tabs: vec![
                String::from("Basics"),
                String::from("Input"),
                String::from("Lists"),
            ],
            active: self.active_tab,
            on_change: Some(GalleryMsg::TabChanged),
            content: vec![self.basics_tab(), self.input_tab(), self.lists_tab()],
        }
    }

    fn update(&mut self, msg: GalleryMsg) -> Action {
        match msg {
            GalleryMsg::ButtonClicked => {
                self.counter += 1;
                Action::Rebuild
            }
            GalleryMsg::ToggleCheck => {
                self.checked = !self.checked;
                Action::Rebuild
            }
            GalleryMsg::TextChanged(_text) => Action::None,
            GalleryMsg::TabChanged(tab) => {
                self.active_tab = tab;
                Action::Rebuild
            }
            GalleryMsg::ListSelect(idx) => {
                self.selected_item = Some(idx);
                Action::Rebuild
            }
        }
    }
}

pub fn main() {
    let app = WidgetGalleryApp::new();
    slopos_appkit::run_app(app, 640, 480);
}
