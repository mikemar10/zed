use gpui::{svg, IntoElement, Rems, Transformation};
use strum::EnumIter;

use crate::prelude::*;

#[derive(Default, PartialEq, Copy, Clone)]
pub enum IconSize {
    Indicator,
    XSmall,
    Small,
    #[default]
    Medium,
}

impl IconSize {
    pub fn rems(self) -> Rems {
        match self {
            IconSize::Indicator => rems_from_px(10.),
            IconSize::XSmall => rems_from_px(12.),
            IconSize::Small => rems_from_px(14.),
            IconSize::Medium => rems_from_px(16.),
        }
    }
}

#[derive(Debug, PartialEq, Copy, Clone, EnumIter)]
pub enum IconName {
    Ai,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowUpRight,
    ArrowCircle,
    AtSign,
    AudioOff,
    AudioOn,
    Backspace,
    Bell,
    BellOff,
    BellRing,
    BellDot,
    Bolt,
    CaseSensitive,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    Close,
    Command,
    Control,
    Copy,
    Dash,
    Delete,
    Disconnected,
    Ellipsis,
    Envelope,
    Escape,
    ExclamationTriangle,
    Exit,
    ExternalLink,
    File,
    FileDoc,
    FileGeneric,
    FileGit,
    FileLock,
    FileRust,
    FileToml,
    FileTree,
    Filter,
    Folder,
    FolderOpen,
    FolderX,
    Github,
    Hash,
    InlayHint,
    Link,
    MagicWand,
    MagnifyingGlass,
    MailOpen,
    Maximize,
    Menu,
    MessageBubbles,
    Mic,
    MicMute,
    Minimize,
    Option,
    PageDown,
    PageUp,
    Pencil,
    Play,
    Plus,
    Public,
    Quote,
    Replace,
    ReplaceAll,
    ReplaceNext,
    Return,
    ReplyArrowRight,
    ReplyArrowLeft,
    Screen,
    SelectAll,
    Shift,
    Snip,
    Space,
    Split,
    Tab,
    Terminal,
    Update,
    WholeWord,
    XCircle,
}

impl IconName {
    pub fn path(self) -> &'static str {
        match self {
            IconName::Ai => "icons/ai.svg",
            IconName::ArrowDown => "icons/arrow_down.svg",
            IconName::ArrowLeft => "icons/arrow_left.svg",
            IconName::ArrowRight => "icons/arrow_right.svg",
            IconName::ArrowUp => "icons/arrow_up.svg",
            IconName::ArrowUpRight => "icons/arrow_up_right.svg",
            IconName::ArrowCircle => "icons/arrow_circle.svg",
            IconName::AtSign => "icons/at_sign.svg",
            IconName::AudioOff => "icons/speaker_off.svg",
            IconName::AudioOn => "icons/speaker_loud.svg",
            IconName::Backspace => "icons/backspace.svg",
            IconName::Bell => "icons/bell.svg",
            IconName::BellOff => "icons/bell_off.svg",
            IconName::BellRing => "icons/bell_ring.svg",
            IconName::BellDot => "icons/bell_dot.svg",
            IconName::Bolt => "icons/bolt.svg",
            IconName::CaseSensitive => "icons/case_insensitive.svg",
            IconName::Check => "icons/check.svg",
            IconName::ChevronDown => "icons/chevron_down.svg",
            IconName::ChevronLeft => "icons/chevron_left.svg",
            IconName::ChevronRight => "icons/chevron_right.svg",
            IconName::ChevronUp => "icons/chevron_up.svg",
            IconName::Close => "icons/x.svg",
            IconName::Command => "icons/command.svg",
            IconName::Control => "icons/control.svg",
            IconName::Copy => "icons/copy.svg",
            IconName::Dash => "icons/dash.svg",
            IconName::Delete => "icons/delete.svg",
            IconName::Disconnected => "icons/disconnected.svg",
            IconName::Ellipsis => "icons/ellipsis.svg",
            IconName::Envelope => "icons/feedback.svg",
            IconName::Escape => "icons/escape.svg",
            IconName::ExclamationTriangle => "icons/warning.svg",
            IconName::Exit => "icons/exit.svg",
            IconName::ExternalLink => "icons/external_link.svg",
            IconName::File => "icons/file.svg",
            IconName::FileDoc => "icons/file_icons/book.svg",
            IconName::FileGeneric => "icons/file_icons/file.svg",
            IconName::FileGit => "icons/file_icons/git.svg",
            IconName::FileLock => "icons/file_icons/lock.svg",
            IconName::FileRust => "icons/file_icons/rust.svg",
            IconName::FileToml => "icons/file_icons/toml.svg",
            IconName::FileTree => "icons/project.svg",
            IconName::Filter => "icons/filter.svg",
            IconName::Folder => "icons/file_icons/folder.svg",
            IconName::FolderOpen => "icons/file_icons/folder_open.svg",
            IconName::FolderX => "icons/stop_sharing.svg",
            IconName::Github => "icons/github.svg",
            IconName::Hash => "icons/hash.svg",
            IconName::InlayHint => "icons/inlay_hint.svg",
            IconName::Link => "icons/link.svg",
            IconName::MagicWand => "icons/magic_wand.svg",
            IconName::MagnifyingGlass => "icons/magnifying_glass.svg",
            IconName::MailOpen => "icons/mail_open.svg",
            IconName::Maximize => "icons/maximize.svg",
            IconName::Menu => "icons/menu.svg",
            IconName::MessageBubbles => "icons/conversations.svg",
            IconName::Mic => "icons/mic.svg",
            IconName::MicMute => "icons/mic_mute.svg",
            IconName::Minimize => "icons/minimize.svg",
            IconName::Option => "icons/option.svg",
            IconName::PageDown => "icons/page_down.svg",
            IconName::PageUp => "icons/page_up.svg",
            IconName::Pencil => "icons/pencil.svg",
            IconName::Play => "icons/play.svg",
            IconName::Plus => "icons/plus.svg",
            IconName::Public => "icons/public.svg",
            IconName::Quote => "icons/quote.svg",
            IconName::Replace => "icons/replace.svg",
            IconName::ReplaceAll => "icons/replace_all.svg",
            IconName::ReplaceNext => "icons/replace_next.svg",
            IconName::Return => "icons/return.svg",
            IconName::ReplyArrowRight => "icons/reply_arrow_right.svg",
            IconName::ReplyArrowLeft => "icons/reply_arrow_left.svg",
            IconName::Screen => "icons/desktop.svg",
            IconName::SelectAll => "icons/select_all.svg",
            IconName::Shift => "icons/shift.svg",
            IconName::Snip => "icons/snip.svg",
            IconName::Space => "icons/space.svg",
            IconName::Split => "icons/split.svg",
            IconName::Tab => "icons/tab.svg",
            IconName::Terminal => "icons/terminal.svg",
            IconName::Update => "icons/update.svg",
            IconName::WholeWord => "icons/word_search.svg",
            IconName::XCircle => "icons/error.svg",
        }
    }
}

#[derive(IntoElement)]
pub struct Icon {
    path: SharedString,
    color: Color,
    size: IconSize,
    transformation: Transformation,
}

impl Icon {
    pub fn new(icon: IconName) -> Self {
        Self {
            path: icon.path().into(),
            color: Color::default(),
            size: IconSize::default(),
            transformation: Transformation::default(),
        }
    }

    pub fn from_path(path: impl Into<SharedString>) -> Self {
        Self {
            path: path.into(),
            color: Color::default(),
            size: IconSize::default(),
            transformation: Transformation::default(),
        }
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    pub fn transform(mut self, transformation: Transformation) -> Self {
        self.transformation = transformation;
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, cx: &mut WindowContext) -> impl IntoElement {
        svg()
            .with_transformation(self.transformation)
            .size(self.size.rems())
            .flex_none()
            .path(self.path)
            .text_color(self.color.color(cx))
    }
}
