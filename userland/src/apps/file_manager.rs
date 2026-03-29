//! File Manager Application
//!
//! Nautilus-inspired file browser using the declarative widget toolkit.

use std::fs;
use std::path::PathBuf;
use std::string::String;

use crate::theme;
use crate::ui::{
    Action, App, ButtonStyle, CrossAxisAlignment, EdgeInsets, Key, Length, MessageId, Modifiers,
    NamedKey, Node, TableColumn, TableColumnWidth, TextAlignment,
};

// ---------------------------------------------------------------------------
// Message IDs
// ---------------------------------------------------------------------------

const MSG_NAV_BACK: MessageId = MessageId::new(1);
const MSG_NAV_FWD: MessageId = MessageId::new(2);
const MSG_NAV_UP: MessageId = MessageId::new(3);
const MSG_FILE_SELECT: MessageId = MessageId::new(10);
const MSG_BOOKMARK_BASE: u32 = 20;

#[derive(Clone, Debug)]
pub enum FileMsg {
    NavBack,
    NavForward,
    NavUp,
    FileSelect(usize),
    Bookmark(usize),
    Unknown(#[allow(dead_code)] MessageId),
}

impl From<MessageId> for FileMsg {
    fn from(m: MessageId) -> Self {
        match m.id {
            1 => FileMsg::NavBack,
            2 => FileMsg::NavForward,
            3 => FileMsg::NavUp,
            10 => FileMsg::FileSelect(m.payload as usize),
            20 => FileMsg::Bookmark(m.payload as usize),
            _ => FileMsg::Unknown(m),
        }
    }
}

// ---------------------------------------------------------------------------
// Bookmarks (richer format matching the old Nautilus-style sidebar)
// ---------------------------------------------------------------------------

struct Bookmark {
    icon: &'static str,
    label: &'static str,
    path: &'static str,
}

const BOOKMARKS: &[Bookmark] = &[
    Bookmark {
        icon: "/",
        label: "Root",
        path: "/",
    },
    Bookmark {
        icon: "~",
        label: "Home",
        path: "/home",
    },
    Bookmark {
        icon: "T",
        label: "Temp",
        path: "/tmp",
    },
    Bookmark {
        icon: "D",
        label: "Devices",
        path: "/dev",
    },
    Bookmark {
        icon: "B",
        label: "Programs",
        path: "/bin",
    },
    Bookmark {
        icon: "S",
        label: "System",
        path: "/sys",
    },
];

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

struct FileEntry {
    name: String,
    is_directory: bool,
    size: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Ok,
    Empty,
    Error,
}

// ---------------------------------------------------------------------------
// FileManagerApp
// ---------------------------------------------------------------------------

pub struct FileManagerApp {
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    load_state: LoadState,
    error_msg: String,
    history_back: Vec<PathBuf>,
    history_forward: Vec<PathBuf>,
    selected: Option<usize>,
}

impl FileManagerApp {
    fn new() -> Self {
        let mut app = Self {
            current_path: PathBuf::from("/"),
            entries: Vec::new(),
            load_state: LoadState::Ok,
            error_msg: String::new(),
            history_back: Vec::new(),
            history_forward: Vec::new(),
            selected: None,
        };
        app.refresh();
        app
    }

    // -----------------------------------------------------------------------
    // Directory loading
    // -----------------------------------------------------------------------

    fn refresh(&mut self) {
        self.entries.clear();
        self.error_msg.clear();

        let read_dir = match fs::read_dir(&self.current_path) {
            Ok(rd) => rd,
            Err(e) => {
                self.load_state = LoadState::Error;
                self.error_msg = format!("{}", e);
                return;
            }
        };

        for dir_entry in read_dir {
            let dir_entry = match dir_entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            let name = match dir_entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };
            if name == "." || name == ".." {
                continue;
            }

            let (is_directory, size) = match dir_entry.metadata() {
                Ok(meta) => (meta.is_dir(), meta.len() as u32),
                Err(_) => (false, 0),
            };

            self.entries.push(FileEntry {
                name,
                is_directory,
                size,
            });
        }

        // Sort: directories first, then alphabetical (case-insensitive)
        self.entries.sort_by(|a, b| {
            b.is_directory.cmp(&a.is_directory).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });

        self.load_state = if self.entries.is_empty() {
            LoadState::Empty
        } else {
            LoadState::Ok
        };
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    fn navigate_to(&mut self, path: PathBuf) {
        self.history_back.push(self.current_path.clone());
        self.history_forward.clear();
        self.current_path = path;
        self.selected = None;
        self.refresh();
    }

    fn navigate_up(&mut self) {
        let mut parent = self.current_path.clone();
        if parent.pop() && !parent.as_os_str().is_empty() {
            self.navigate_to(parent);
        } else if self.current_path.as_os_str() != "/" {
            self.navigate_to(PathBuf::from("/"));
        }
    }

    fn navigate_back(&mut self) {
        if let Some(prev) = self.history_back.pop() {
            self.history_forward.push(self.current_path.clone());
            self.current_path = prev;
            self.selected = None;
            self.refresh();
        }
    }

    fn navigate_forward(&mut self) {
        if let Some(next) = self.history_forward.pop() {
            self.history_back.push(self.current_path.clone());
            self.current_path = next;
            self.selected = None;
            self.refresh();
        }
    }

    fn open_selected(&mut self) {
        if let Some(idx) = self.selected {
            if idx < self.entries.len() && self.entries[idx].is_directory {
                let name = self.entries[idx].name.clone();
                let mut next = self.current_path.clone();
                next.push(&name);
                self.navigate_to(next);
            }
        }
    }

    // -----------------------------------------------------------------------
    // View helpers -- sidebar
    // -----------------------------------------------------------------------

    fn active_bookmark_index(&self) -> Option<usize> {
        let path_str = self.current_path.to_string_lossy();
        BOOKMARKS.iter().position(|bm| path_str == bm.path)
    }

    fn build_sidebar(&self) -> Node {
        let active_idx = self.active_bookmark_index();

        // "Places" heading
        let heading = Node::Padding {
            padding: EdgeInsets::new(6, 8, 2, 8),
            child: Box::new(Node::StyledLabel {
                text: String::from("Places"),
                color: theme::FM_SIDEBAR_HEADING,
                alignment: TextAlignment::Start,
            }),
        };

        // Bookmark entries as a ListView with styled content
        let items: Vec<Node> = BOOKMARKS
            .iter()
            .enumerate()
            .map(|(i, bm)| {
                let is_active = active_idx == Some(i);
                let text_color = if is_active {
                    theme::TEXT_PRIMARY
                } else {
                    theme::FM_SIDEBAR_TEXT
                };

                // "icon  label" as HStack with colored icon + label
                Node::Padding {
                    padding: EdgeInsets::new(0, 10, 0, 4),
                    child: Box::new(Node::HStack {
                        spacing: 6,
                        align: CrossAxisAlignment::Center,
                        children: vec![
                            Node::StyledLabel {
                                text: String::from(bm.icon),
                                color: theme::FM_DIR_COLOR,
                                alignment: TextAlignment::Start,
                            },
                            Node::StyledLabel {
                                text: String::from(bm.label),
                                color: text_color,
                                alignment: TextAlignment::Start,
                            },
                        ],
                    }),
                }
            })
            .collect();

        let bookmark_list = Node::ListView {
            item_height: theme::FM_SIDEBAR_ITEM_HEIGHT,
            selected: active_idx,
            on_select: MessageId::new(MSG_BOOKMARK_BASE),
            items,
        };

        // Wrap in a SizedBox to fix width, with the dark sidebar background
        Node::SizedBox {
            width: Some(Length::Px(theme::FM_SIDEBAR_WIDTH)),
            height: None,
            child: Box::new(Node::Background {
                color: theme::FM_SIDEBAR_BG,
                child: Box::new(Node::VStack {
                    spacing: 0,
                    align: CrossAxisAlignment::Stretch,
                    children: vec![heading, bookmark_list],
                }),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // View helpers -- navigation bar
    // -----------------------------------------------------------------------

    fn build_navbar(&self) -> Node {
        // Path breadcrumb with truncation from left
        let path_str = self.current_path.display().to_string();
        let display_path = if path_str.len() > 50 {
            let start = path_str.len() - 47;
            format!("...{}", &path_str[start..])
        } else {
            path_str
        };

        Node::Background {
            color: theme::FM_NAV_BG,
            child: Box::new(Node::Padding {
                padding: EdgeInsets::new(4, 4, 4, 4),
                child: Box::new(Node::HStack {
                    spacing: 2,
                    align: CrossAxisAlignment::Center,
                    children: vec![
                        Node::Button {
                            label: String::from(" < "),
                            on_press: Some(MSG_NAV_BACK),
                            style: ButtonStyle::Secondary,
                            enabled: !self.history_back.is_empty(),
                        },
                        Node::Button {
                            label: String::from(" > "),
                            on_press: Some(MSG_NAV_FWD),
                            style: ButtonStyle::Secondary,
                            enabled: !self.history_forward.is_empty(),
                        },
                        Node::Button {
                            label: String::from(" ^ "),
                            on_press: Some(MSG_NAV_UP),
                            style: ButtonStyle::Secondary,
                            enabled: true,
                        },
                        Node::Spacer {
                            size: Length::Px(8),
                        },
                        Node::StyledLabel {
                            text: display_path,
                            color: theme::TEXT_PRIMARY,
                            alignment: TextAlignment::Start,
                        },
                    ],
                }),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // View helpers -- file list rows
    // -----------------------------------------------------------------------

    fn build_file_row(&self, entry: &FileEntry) -> Vec<Node> {
        let (name_text, name_color) = if entry.is_directory {
            (format!("[D] {}", entry.name), theme::FM_DIR_COLOR)
        } else {
            (format!("    {}", entry.name), theme::FM_FILE_COLOR)
        };

        let size_text = if entry.is_directory {
            String::from("--")
        } else {
            format_size(entry.size)
        };

        vec![
            Node::StyledLabel {
                text: name_text,
                color: name_color,
                alignment: TextAlignment::Start,
            },
            Node::StyledLabel {
                text: size_text,
                color: theme::FM_SIZE_COLOR,
                alignment: TextAlignment::End,
            },
        ]
    }

    // -----------------------------------------------------------------------
    // View helpers -- main content area
    // -----------------------------------------------------------------------

    fn build_content(&self) -> Node {
        let content_node = match self.load_state {
            LoadState::Error => Node::Background {
                color: theme::FM_COLOR_BG,
                child: Box::new(Node::Padding {
                    padding: EdgeInsets::all(16),
                    child: Box::new(Node::StyledLabel {
                        text: self.error_msg.clone(),
                        color: theme::FM_ERROR_COLOR,
                        alignment: TextAlignment::Center,
                    }),
                }),
            },
            LoadState::Empty => Node::Background {
                color: theme::FM_COLOR_BG,
                child: Box::new(Node::Padding {
                    padding: EdgeInsets::all(16),
                    child: Box::new(Node::StyledLabel {
                        text: String::from("Empty directory"),
                        color: theme::FM_SIZE_COLOR,
                        alignment: TextAlignment::Center,
                    }),
                }),
            },
            LoadState::Ok => {
                let rows: Vec<Vec<Node>> = self
                    .entries
                    .iter()
                    .map(|e| self.build_file_row(e))
                    .collect();
                Node::Table {
                    columns: vec![
                        TableColumn {
                            label: String::from("Name"),
                            width: TableColumnWidth::Flex(3),
                            sort_indicator: None,
                        },
                        TableColumn {
                            label: String::from("Size"),
                            width: TableColumnWidth::Fixed(80),
                            sort_indicator: None,
                        },
                    ],
                    rows,
                    row_height: theme::FM_ITEM_HEIGHT,
                    selected: self.selected,
                    on_select: MSG_FILE_SELECT,
                    on_header_click: None,
                }
            }
        };

        Node::Expand {
            weight: 1,
            child: Box::new(content_node),
        }
    }

    // -----------------------------------------------------------------------
    // View helpers -- status bar
    // -----------------------------------------------------------------------

    fn build_status_bar(&self) -> Node {
        let text = match self.load_state {
            LoadState::Error => String::from("Error"),
            LoadState::Empty => String::from("0 items"),
            LoadState::Ok => {
                let dir_count = self.entries.iter().filter(|e| e.is_directory).count();
                let file_count = self.entries.len() - dir_count;
                let base = if dir_count > 0 && file_count > 0 {
                    format!("{} folders, {} files", dir_count, file_count)
                } else if dir_count > 0 {
                    format!("{} folders", dir_count)
                } else {
                    format!("{} files", file_count)
                };
                if let Some(sel) = self.selected {
                    if sel < self.entries.len() {
                        let e = &self.entries[sel];
                        if e.is_directory {
                            format!("{} | \"{}\"", base, e.name)
                        } else {
                            format!("{} | \"{}\" ({})", base, e.name, format_size(e.size))
                        }
                    } else {
                        base
                    }
                } else {
                    base
                }
            }
        };

        Node::Background {
            color: theme::FM_STATUS_BG,
            child: Box::new(Node::Padding {
                padding: EdgeInsets::new(3, 6, 3, 6),
                child: Box::new(Node::StyledLabel {
                    text,
                    color: theme::FM_STATUS_TEXT,
                    alignment: TextAlignment::Start,
                }),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Size formatting
// ---------------------------------------------------------------------------

fn format_size(size: u32) -> String {
    if size < 1024 {
        format!("{} B", size)
    } else if size < 1024 * 1024 {
        format!("{} K", size / 1024)
    } else {
        format!("{:.1} M", size as f64 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// App trait implementation
// ---------------------------------------------------------------------------

impl App for FileManagerApp {
    type Message = FileMsg;

    fn view(&self) -> Node {
        Node::HStack {
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
            children: vec![
                // Sidebar (fixed width via SizedBox, dark background)
                self.build_sidebar(),
                // Vertical separator between sidebar and content
                Node::Divider,
                // Main content area (expanded to fill remaining space)
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::VStack {
                        spacing: 0,
                        align: CrossAxisAlignment::Stretch,
                        children: vec![
                            self.build_navbar(),
                            Node::Divider,
                            self.build_content(),
                            self.build_status_bar(),
                        ],
                    }),
                },
            ],
        }
    }

    fn update(&mut self, msg: FileMsg) -> Action {
        match msg {
            FileMsg::NavBack => {
                self.navigate_back();
                Action::Rebuild
            }
            FileMsg::NavForward => {
                self.navigate_forward();
                Action::Rebuild
            }
            FileMsg::NavUp => {
                self.navigate_up();
                Action::Rebuild
            }
            FileMsg::FileSelect(idx) => {
                // Click on already-selected row = double-click → open directory
                if self.selected == Some(idx) {
                    self.open_selected();
                } else {
                    self.selected = Some(idx);
                }
                Action::Rebuild
            }
            FileMsg::Bookmark(idx) => {
                if idx < BOOKMARKS.len() {
                    self.navigate_to(PathBuf::from(BOOKMARKS[idx].path));
                }
                Action::Rebuild
            }
            FileMsg::Unknown(_) => Action::None,
        }
    }

    fn on_key(&mut self, key: Key, _modifiers: Modifiers) -> Action {
        match key {
            Key::Named(NamedKey::Enter) => {
                self.open_selected();
                Action::Rebuild
            }
            Key::Named(NamedKey::Backspace) => {
                self.navigate_up();
                Action::Rebuild
            }
            _ => Action::None,
        }
    }

    fn title(&self) -> &str {
        "File Manager"
    }

    fn app_id(&self) -> &str {
        "org.slopos.files"
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn file_manager_main() -> ! {
    let app = FileManagerApp::new();
    crate::ui::run_app(app, 640, 392)
}
