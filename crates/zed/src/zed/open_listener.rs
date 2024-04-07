use anyhow::{anyhow, Result};

use collections::HashMap;
use editor::scroll::Autoscroll;
use editor::Editor;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use gpui::{AppContext, AsyncAppContext, Global, WindowHandle};
use language::{Bias, Point};
use std::path::PathBuf;
use std::sync::Arc;

use util::paths::PathLikeWithPosition;
use util::ResultExt;
use workspace::item::ItemHandle;
use workspace::{AppState, Workspace};

#[derive(Default, Debug)]
pub struct OpenRequest {
    pub open_paths: Vec<PathLikeWithPosition<PathBuf>>,
    pub open_channel_notes: Vec<(u64, Option<String>)>,
    pub join_channel: Option<u64>,
}

impl OpenRequest {
    pub fn parse(urls: Vec<String>) -> Result<Self> {
        let mut this = Self::default();
        for url in urls {
            if let Some(file) = url.strip_prefix("file://") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://file") {
                this.parse_file_path(file)
            } else {
                log::error!("unhandled url: {}", url);
            }
        }

        Ok(this)
    }

    fn parse_file_path(&mut self, file: &str) {
        if let Some(decoded) = urlencoding::decode(file).log_err() {
            if let Some(path_buf) =
                PathLikeWithPosition::parse_str(&decoded, |s| PathBuf::try_from(s)).log_err()
            {
                self.open_paths.push(path_buf)
            }
        }
    }
}

pub struct OpenListener {
    tx: UnboundedSender<Vec<String>>,
}

struct GlobalOpenListener(Arc<OpenListener>);

impl Global for GlobalOpenListener {}

impl OpenListener {
    pub fn global(cx: &AppContext) -> Arc<Self> {
        cx.global::<GlobalOpenListener>().0.clone()
    }

    pub fn set_global(listener: Arc<OpenListener>, cx: &mut AppContext) {
        cx.set_global(GlobalOpenListener(listener))
    }

    pub fn new() -> (Self, UnboundedReceiver<Vec<String>>) {
        let (tx, rx) = mpsc::unbounded();
        (OpenListener { tx }, rx)
    }

    pub fn open_urls(&self, urls: Vec<String>) {
        self.tx
            .unbounded_send(urls)
            .map_err(|_| anyhow!("no listener for open requests"))
            .log_err();
    }
}

pub async fn open_paths_with_positions(
    path_likes: &Vec<PathLikeWithPosition<PathBuf>>,
    app_state: Arc<AppState>,
    open_options: workspace::OpenOptions,
    cx: &mut AsyncAppContext,
) -> Result<(
    WindowHandle<Workspace>,
    Vec<Option<Result<Box<dyn ItemHandle>>>>,
)> {
    let mut caret_positions = HashMap::default();

    let paths = path_likes
        .iter()
        .map(|path_with_position| {
            let path = path_with_position.path_like.clone();
            if let Some(row) = path_with_position.row {
                if path.is_file() {
                    let row = row.saturating_sub(1);
                    let col = path_with_position.column.unwrap_or(0).saturating_sub(1);
                    caret_positions.insert(path.clone(), Point::new(row, col));
                }
            }
            path
        })
        .collect::<Vec<_>>();

    let (workspace, items) = cx
        .update(|cx| workspace::open_paths(&paths, app_state, open_options, cx))?
        .await?;

    for (item, path) in items.iter().zip(&paths) {
        let Some(Ok(item)) = item else {
            continue;
        };
        let Some(point) = caret_positions.remove(path) else {
            continue;
        };
        if let Some(active_editor) = item.downcast::<Editor>() {
            workspace
                .update(cx, |_, cx| {
                    active_editor.update(cx, |editor, cx| {
                        let snapshot = editor.snapshot(cx).display_snapshot;
                        let point = snapshot.buffer_snapshot.clip_point(point, Bias::Left);
                        editor.change_selections(Some(Autoscroll::center()), cx, |s| {
                            s.select_ranges([point..point])
                        });
                    });
                })
                .log_err();
        }
    }

    Ok((workspace, items))
}
