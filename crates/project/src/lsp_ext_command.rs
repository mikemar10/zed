use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use gpui::{AppContext, AsyncAppContext, Model};
use language::{point_to_lsp, Buffer};
use lsp::{LanguageServer, LanguageServerId};
use serde::{Deserialize, Serialize};
use text::PointUtf16;

use crate::{lsp_command::LspCommand, Project};

pub enum LspExpandMacro {}

impl lsp::request::Request for LspExpandMacro {
    type Params = ExpandMacroParams;
    type Result = Option<ExpandedMacro>;
    const METHOD: &'static str = "rust-analyzer/expandMacro";
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExpandMacroParams {
    pub text_document: lsp::TextDocumentIdentifier,
    pub position: lsp::Position,
}

#[derive(Default, Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExpandedMacro {
    pub name: String,
    pub expansion: String,
}

impl ExpandedMacro {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.expansion.is_empty()
    }
}

pub struct ExpandMacro {
    pub position: PointUtf16,
}

#[async_trait(?Send)]
impl LspCommand for ExpandMacro {
    type Response = ExpandedMacro;
    type LspRequest = LspExpandMacro;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> ExpandMacroParams {
        ExpandMacroParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: lsp::Url::from_file_path(path).unwrap(),
            },
            position: point_to_lsp(self.position),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<ExpandedMacro>,
        _: Model<Project>,
        _: Model<Buffer>,
        _: LanguageServerId,
        _: AsyncAppContext,
    ) -> anyhow::Result<ExpandedMacro> {
        Ok(message
            .map(|message| ExpandedMacro {
                name: message.name,
                expansion: message.expansion,
            })
            .unwrap_or_default())
    }
}
