use crate::{
    DocumentHighlight, Hover, HoverBlock, HoverBlockKind, InlayHint, InlayHintLabel,
    InlayHintLabelPart, InlayHintLabelPartTooltip, InlayHintTooltip, Location, LocationLink,
    MarkupContent, Project, ProjectTransaction, ResolveState,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::future;
use gpui::{AppContext, AsyncAppContext, Model};
use language::{
    language_settings::InlayHintKind, point_from_lsp, point_to_lsp,
    prepare_completion_documentation, range_from_lsp, range_to_lsp, Anchor, Bias, Buffer,
    BufferSnapshot, CachedLspAdapter, CharKind, CodeAction, Completion, OffsetRangeExt, PointUtf16,
    ToOffset, ToPointUtf16, Transaction, Unclipped,
};
use lsp::{
    CompletionListItemDefaultsEditRange, LanguageServer, LanguageServerId, OneOf,
    ServerCapabilities,
};
use std::{cmp::Reverse, ops::Range, path::Path, sync::Arc};
use text::LineEnding;

pub fn lsp_formatting_options(tab_size: u32) -> lsp::FormattingOptions {
    lsp::FormattingOptions {
        tab_size,
        insert_spaces: true,
        insert_final_newline: Some(true),
        ..lsp::FormattingOptions::default()
    }
}

#[async_trait(?Send)]
pub trait LspCommand: 'static + Sized + Send {
    type Response: 'static + Default + Send;
    type LspRequest: 'static + Send + lsp::request::Request;

    fn check_capabilities(&self, _: &lsp::ServerCapabilities) -> bool {
        true
    }

    fn to_lsp(
        &self,
        path: &Path,
        buffer: &Buffer,
        language_server: &Arc<LanguageServer>,
        cx: &AppContext,
    ) -> <Self::LspRequest as lsp::request::Request>::Params;

    async fn response_from_lsp(
        self,
        message: <Self::LspRequest as lsp::request::Request>::Result,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        cx: AsyncAppContext,
    ) -> Result<Self::Response>;
}

pub(crate) struct PrepareRename {
    pub position: PointUtf16,
}

pub(crate) struct PerformRename {
    pub position: PointUtf16,
    pub new_name: String,
    pub push_to_history: bool,
}

pub struct GetDefinition {
    pub position: PointUtf16,
}

pub(crate) struct GetTypeDefinition {
    pub position: PointUtf16,
}

pub(crate) struct GetImplementation {
    pub position: PointUtf16,
}

pub(crate) struct GetReferences {
    pub position: PointUtf16,
}

pub(crate) struct GetDocumentHighlights {
    pub position: PointUtf16,
}

pub(crate) struct GetHover {
    pub position: PointUtf16,
}

pub(crate) struct GetCompletions {
    pub position: PointUtf16,
}

pub(crate) struct GetCodeActions {
    pub range: Range<Anchor>,
    pub kinds: Option<Vec<lsp::CodeActionKind>>,
}

pub(crate) struct OnTypeFormatting {
    pub position: PointUtf16,
    pub trigger: String,
    pub options: FormattingOptions,
    pub push_to_history: bool,
}

pub(crate) struct InlayHints {
    pub range: Range<Anchor>,
}

pub(crate) struct FormattingOptions {
    tab_size: u32,
}

impl From<lsp::FormattingOptions> for FormattingOptions {
    fn from(value: lsp::FormattingOptions) -> Self {
        Self {
            tab_size: value.tab_size,
        }
    }
}

#[async_trait(?Send)]
impl LspCommand for PrepareRename {
    type Response = Option<Range<Anchor>>;
    type LspRequest = lsp::request::PrepareRenameRequest;

    fn check_capabilities(&self, capabilities: &ServerCapabilities) -> bool {
        if let Some(lsp::OneOf::Right(rename)) = &capabilities.rename_provider {
            rename.prepare_provider == Some(true)
        } else {
            false
        }
    }

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::TextDocumentPositionParams {
        lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: lsp::Url::from_file_path(path).unwrap(),
            },
            position: point_to_lsp(self.position),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::PrepareRenameResponse>,
        _: Model<Project>,
        buffer: Model<Buffer>,
        _: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Option<Range<Anchor>>> {
        buffer.update(&mut cx, |buffer, _| {
            if let Some(
                lsp::PrepareRenameResponse::Range(range)
                | lsp::PrepareRenameResponse::RangeWithPlaceholder { range, .. },
            ) = message
            {
                let Range { start, end } = range_from_lsp(range);
                if buffer.clip_point_utf16(start, Bias::Left) == start.0
                    && buffer.clip_point_utf16(end, Bias::Left) == end.0
                {
                    return Ok(Some(buffer.anchor_after(start)..buffer.anchor_before(end)));
                }
            }
            Ok(None)
        })?
    }
}

#[async_trait(?Send)]
impl LspCommand for PerformRename {
    type Response = ProjectTransaction;
    type LspRequest = lsp::request::Rename;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::RenameParams {
        lsp::RenameParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            new_name: self.new_name.clone(),
            work_done_progress_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::WorkspaceEdit>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<ProjectTransaction> {
        if let Some(edit) = message {
            let (lsp_adapter, lsp_server) =
                language_server_for_buffer(&project, &buffer, server_id, &mut cx)?;
            Project::deserialize_workspace_edit(
                project,
                edit,
                self.push_to_history,
                lsp_adapter,
                lsp_server,
                &mut cx,
            )
            .await
        } else {
            Ok(ProjectTransaction::default())
        }
    }
}

#[async_trait(?Send)]
impl LspCommand for GetDefinition {
    type Response = Vec<LocationLink>;
    type LspRequest = lsp::request::GotoDefinition;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::GotoDefinitionParams {
        lsp::GotoDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::GotoDefinitionResponse>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        cx: AsyncAppContext,
    ) -> Result<Vec<LocationLink>> {
        location_links_from_lsp(message, project, buffer, server_id, cx).await
    }
}

#[async_trait(?Send)]
impl LspCommand for GetImplementation {
    type Response = Vec<LocationLink>;
    type LspRequest = lsp::request::GotoImplementation;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::GotoImplementationParams {
        lsp::GotoImplementationParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::GotoImplementationResponse>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        cx: AsyncAppContext,
    ) -> Result<Vec<LocationLink>> {
        location_links_from_lsp(message, project, buffer, server_id, cx).await
    }
}

#[async_trait(?Send)]
impl LspCommand for GetTypeDefinition {
    type Response = Vec<LocationLink>;
    type LspRequest = lsp::request::GotoTypeDefinition;

    fn check_capabilities(&self, capabilities: &ServerCapabilities) -> bool {
        match &capabilities.type_definition_provider {
            None => false,
            Some(lsp::TypeDefinitionProviderCapability::Simple(false)) => false,
            _ => true,
        }
    }

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::GotoTypeDefinitionParams {
        lsp::GotoTypeDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::GotoTypeDefinitionResponse>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        cx: AsyncAppContext,
    ) -> Result<Vec<LocationLink>> {
        location_links_from_lsp(message, project, buffer, server_id, cx).await
    }
}

fn language_server_for_buffer(
    project: &Model<Project>,
    buffer: &Model<Buffer>,
    server_id: LanguageServerId,
    cx: &mut AsyncAppContext,
) -> Result<(Arc<CachedLspAdapter>, Arc<LanguageServer>)> {
    project
        .update(cx, |project, cx| {
            project
                .language_server_for_buffer(buffer.read(cx), server_id, cx)
                .map(|(adapter, server)| (adapter.clone(), server.clone()))
        })?
        .ok_or_else(|| anyhow!("no language server found for buffer"))
}

async fn location_links_from_lsp(
    message: Option<lsp::GotoDefinitionResponse>,
    project: Model<Project>,
    buffer: Model<Buffer>,
    server_id: LanguageServerId,
    mut cx: AsyncAppContext,
) -> Result<Vec<LocationLink>> {
    let message = match message {
        Some(message) => message,
        None => return Ok(Vec::new()),
    };

    let mut unresolved_links = Vec::new();
    match message {
        lsp::GotoDefinitionResponse::Scalar(loc) => {
            unresolved_links.push((None, loc.uri, loc.range));
        }

        lsp::GotoDefinitionResponse::Array(locs) => {
            unresolved_links.extend(locs.into_iter().map(|l| (None, l.uri, l.range)));
        }

        lsp::GotoDefinitionResponse::Link(links) => {
            unresolved_links.extend(links.into_iter().map(|l| {
                (
                    l.origin_selection_range,
                    l.target_uri,
                    l.target_selection_range,
                )
            }));
        }
    }

    let (lsp_adapter, language_server) =
        language_server_for_buffer(&project, &buffer, server_id, &mut cx)?;
    let mut definitions = Vec::new();
    for (origin_range, target_uri, target_range) in unresolved_links {
        let target_buffer_handle = project
            .update(&mut cx, |this, cx| {
                this.open_local_buffer_via_lsp(
                    target_uri,
                    language_server.server_id(),
                    lsp_adapter.name.clone(),
                    cx,
                )
            })?
            .await?;

        cx.update(|cx| {
            let origin_location = origin_range.map(|origin_range| {
                let origin_buffer = buffer.read(cx);
                let origin_start =
                    origin_buffer.clip_point_utf16(point_from_lsp(origin_range.start), Bias::Left);
                let origin_end =
                    origin_buffer.clip_point_utf16(point_from_lsp(origin_range.end), Bias::Left);
                Location {
                    buffer: buffer.clone(),
                    range: origin_buffer.anchor_after(origin_start)
                        ..origin_buffer.anchor_before(origin_end),
                }
            });

            let target_buffer = target_buffer_handle.read(cx);
            let target_start =
                target_buffer.clip_point_utf16(point_from_lsp(target_range.start), Bias::Left);
            let target_end =
                target_buffer.clip_point_utf16(point_from_lsp(target_range.end), Bias::Left);
            let target_location = Location {
                buffer: target_buffer_handle,
                range: target_buffer.anchor_after(target_start)
                    ..target_buffer.anchor_before(target_end),
            };

            definitions.push(LocationLink {
                origin: origin_location,
                target: target_location,
            })
        })?;
    }
    Ok(definitions)
}

#[async_trait(?Send)]
impl LspCommand for GetReferences {
    type Response = Vec<Location>;
    type LspRequest = lsp::request::References;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::ReferenceParams {
        lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp::ReferenceContext {
                include_declaration: true,
            },
        }
    }

    async fn response_from_lsp(
        self,
        locations: Option<Vec<lsp::Location>>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Vec<Location>> {
        let mut references = Vec::new();
        let (lsp_adapter, language_server) =
            language_server_for_buffer(&project, &buffer, server_id, &mut cx)?;

        if let Some(locations) = locations {
            for lsp_location in locations {
                let target_buffer_handle = project
                    .update(&mut cx, |this, cx| {
                        this.open_local_buffer_via_lsp(
                            lsp_location.uri,
                            language_server.server_id(),
                            lsp_adapter.name.clone(),
                            cx,
                        )
                    })?
                    .await?;

                target_buffer_handle
                    .clone()
                    .update(&mut cx, |target_buffer, _| {
                        let target_start = target_buffer
                            .clip_point_utf16(point_from_lsp(lsp_location.range.start), Bias::Left);
                        let target_end = target_buffer
                            .clip_point_utf16(point_from_lsp(lsp_location.range.end), Bias::Left);
                        references.push(Location {
                            buffer: target_buffer_handle,
                            range: target_buffer.anchor_after(target_start)
                                ..target_buffer.anchor_before(target_end),
                        });
                    })?;
            }
        }

        Ok(references)
    }
}

#[async_trait(?Send)]
impl LspCommand for GetDocumentHighlights {
    type Response = Vec<DocumentHighlight>;
    type LspRequest = lsp::request::DocumentHighlightRequest;

    fn check_capabilities(&self, capabilities: &ServerCapabilities) -> bool {
        capabilities.document_highlight_provider.is_some()
    }

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::DocumentHighlightParams {
        lsp::DocumentHighlightParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        lsp_highlights: Option<Vec<lsp::DocumentHighlight>>,
        _: Model<Project>,
        buffer: Model<Buffer>,
        _: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Vec<DocumentHighlight>> {
        buffer.update(&mut cx, |buffer, _| {
            let mut lsp_highlights = lsp_highlights.unwrap_or_default();
            lsp_highlights.sort_unstable_by_key(|h| (h.range.start, Reverse(h.range.end)));
            lsp_highlights
                .into_iter()
                .map(|lsp_highlight| {
                    let start = buffer
                        .clip_point_utf16(point_from_lsp(lsp_highlight.range.start), Bias::Left);
                    let end = buffer
                        .clip_point_utf16(point_from_lsp(lsp_highlight.range.end), Bias::Left);
                    DocumentHighlight {
                        range: buffer.anchor_after(start)..buffer.anchor_before(end),
                        kind: lsp_highlight
                            .kind
                            .unwrap_or(lsp::DocumentHighlightKind::READ),
                    }
                })
                .collect()
        })
    }
}

#[async_trait(?Send)]
impl LspCommand for GetHover {
    type Response = Option<Hover>;
    type LspRequest = lsp::request::HoverRequest;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::HoverParams {
        lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier {
                    uri: lsp::Url::from_file_path(path).unwrap(),
                },
                position: point_to_lsp(self.position),
            },
            work_done_progress_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<lsp::Hover>,
        _: Model<Project>,
        buffer: Model<Buffer>,
        _: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Self::Response> {
        let Some(hover) = message else {
            return Ok(None);
        };

        let (language, range) = buffer.update(&mut cx, |buffer, _| {
            (
                buffer.language().cloned(),
                hover.range.map(|range| {
                    let token_start =
                        buffer.clip_point_utf16(point_from_lsp(range.start), Bias::Left);
                    let token_end = buffer.clip_point_utf16(point_from_lsp(range.end), Bias::Left);
                    buffer.anchor_after(token_start)..buffer.anchor_before(token_end)
                }),
            )
        })?;

        fn hover_blocks_from_marked_string(marked_string: lsp::MarkedString) -> Option<HoverBlock> {
            let block = match marked_string {
                lsp::MarkedString::String(content) => HoverBlock {
                    text: content,
                    kind: HoverBlockKind::Markdown,
                },
                lsp::MarkedString::LanguageString(lsp::LanguageString { language, value }) => {
                    HoverBlock {
                        text: value,
                        kind: HoverBlockKind::Code { language },
                    }
                }
            };
            if block.text.is_empty() {
                None
            } else {
                Some(block)
            }
        }

        let contents = match hover.contents {
            lsp::HoverContents::Scalar(marked_string) => {
                hover_blocks_from_marked_string(marked_string)
                    .into_iter()
                    .collect()
            }
            lsp::HoverContents::Array(marked_strings) => marked_strings
                .into_iter()
                .filter_map(hover_blocks_from_marked_string)
                .collect(),
            lsp::HoverContents::Markup(markup_content) => vec![HoverBlock {
                text: markup_content.value,
                kind: if markup_content.kind == lsp::MarkupKind::Markdown {
                    HoverBlockKind::Markdown
                } else {
                    HoverBlockKind::PlainText
                },
            }],
        };

        Ok(Some(Hover {
            contents,
            range,
            language,
        }))
    }
}

#[async_trait(?Send)]
impl LspCommand for GetCompletions {
    type Response = Vec<Completion>;
    type LspRequest = lsp::request::Completion;

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::CompletionParams {
        lsp::CompletionParams {
            text_document_position: lsp::TextDocumentPositionParams::new(
                lsp::TextDocumentIdentifier::new(lsp::Url::from_file_path(path).unwrap()),
                point_to_lsp(self.position),
            ),
            context: Default::default(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        completions: Option<lsp::CompletionResponse>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Vec<Completion>> {
        let mut response_list = None;
        let completions = if let Some(completions) = completions {
            match completions {
                lsp::CompletionResponse::Array(completions) => completions,

                lsp::CompletionResponse::List(mut list) => {
                    let items = std::mem::take(&mut list.items);
                    response_list = Some(list);
                    items
                }
            }
        } else {
            Default::default()
        };

        let language_server_adapter = project
            .update(&mut cx, |project, _cx| {
                project.language_server_adapter_for_id(server_id)
            })?
            .ok_or_else(|| anyhow!("no such language server"))?;

        let completions = buffer.update(&mut cx, |buffer, cx| {
            let language_registry = project.read(cx).languages().clone();
            let language = buffer.language().cloned();
            let snapshot = buffer.snapshot();
            let clipped_position = buffer.clip_point_utf16(Unclipped(self.position), Bias::Left);

            let mut range_for_token = None;
            completions
                .into_iter()
                .filter_map(move |mut lsp_completion| {
                    let (old_range, mut new_text) = match lsp_completion.text_edit.as_ref() {
                        // If the language server provides a range to overwrite, then
                        // check that the range is valid.
                        Some(lsp::CompletionTextEdit::Edit(edit)) => {
                            let range = range_from_lsp(edit.range);
                            let start = snapshot.clip_point_utf16(range.start, Bias::Left);
                            let end = snapshot.clip_point_utf16(range.end, Bias::Left);
                            if start != range.start.0 || end != range.end.0 {
                                log::info!("completion out of expected range");
                                return None;
                            }
                            (
                                snapshot.anchor_before(start)..snapshot.anchor_after(end),
                                edit.new_text.clone(),
                            )
                        }

                        // If the language server does not provide a range, then infer
                        // the range based on the syntax tree.
                        None => {
                            if self.position != clipped_position {
                                log::info!("completion out of expected range");
                                return None;
                            }

                            let default_edit_range = response_list
                                .as_ref()
                                .and_then(|list| list.item_defaults.as_ref())
                                .and_then(|defaults| defaults.edit_range.as_ref())
                                .and_then(|range| match range {
                                    CompletionListItemDefaultsEditRange::Range(r) => Some(r),
                                    _ => None,
                                });

                            let range = if let Some(range) = default_edit_range {
                                let range = range_from_lsp(*range);
                                let start = snapshot.clip_point_utf16(range.start, Bias::Left);
                                let end = snapshot.clip_point_utf16(range.end, Bias::Left);
                                if start != range.start.0 || end != range.end.0 {
                                    log::info!("completion out of expected range");
                                    return None;
                                }

                                snapshot.anchor_before(start)..snapshot.anchor_after(end)
                            } else {
                                range_for_token
                                    .get_or_insert_with(|| {
                                        let offset = self.position.to_offset(&snapshot);
                                        let (range, kind) = snapshot.surrounding_word(offset);
                                        let range = if kind == Some(CharKind::Word) {
                                            range
                                        } else {
                                            offset..offset
                                        };

                                        snapshot.anchor_before(range.start)
                                            ..snapshot.anchor_after(range.end)
                                    })
                                    .clone()
                            };

                            let text = lsp_completion
                                .insert_text
                                .as_ref()
                                .unwrap_or(&lsp_completion.label)
                                .clone();
                            (range, text)
                        }

                        Some(lsp::CompletionTextEdit::InsertAndReplace(edit)) => {
                            let range = range_from_lsp(edit.insert);

                            let start = snapshot.clip_point_utf16(range.start, Bias::Left);
                            let end = snapshot.clip_point_utf16(range.end, Bias::Left);
                            if start != range.start.0 || end != range.end.0 {
                                log::info!("completion out of expected range");
                                return None;
                            }
                            (
                                snapshot.anchor_before(start)..snapshot.anchor_after(end),
                                edit.new_text.clone(),
                            )
                        }
                    };

                    let language_registry = language_registry.clone();
                    let language = language.clone();
                    let language_server_adapter = language_server_adapter.clone();
                    LineEnding::normalize(&mut new_text);
                    Some(async move {
                        let mut label = None;
                        if let Some(language) = &language {
                            language_server_adapter
                                .process_completion(&mut lsp_completion)
                                .await;
                            label = language_server_adapter
                                .label_for_completion(&lsp_completion, language)
                                .await;
                        }

                        let documentation = if let Some(lsp_docs) = &lsp_completion.documentation {
                            Some(
                                prepare_completion_documentation(
                                    lsp_docs,
                                    &language_registry,
                                    language.clone(),
                                )
                                .await,
                            )
                        } else {
                            None
                        };

                        Completion {
                            old_range,
                            new_text,
                            label: label.unwrap_or_else(|| {
                                language::CodeLabel::plain(
                                    lsp_completion.label.clone(),
                                    lsp_completion.filter_text.as_deref(),
                                )
                            }),
                            documentation,
                            server_id,
                            lsp_completion,
                        }
                    })
                })
        })?;

        Ok(future::join_all(completions).await)
    }
}

#[async_trait(?Send)]
impl LspCommand for GetCodeActions {
    type Response = Vec<CodeAction>;
    type LspRequest = lsp::request::CodeActionRequest;

    fn check_capabilities(&self, capabilities: &ServerCapabilities) -> bool {
        match &capabilities.code_action_provider {
            None => false,
            Some(lsp::CodeActionProviderCapability::Simple(false)) => false,
            _ => true,
        }
    }

    fn to_lsp(
        &self,
        path: &Path,
        buffer: &Buffer,
        language_server: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::CodeActionParams {
        let relevant_diagnostics = buffer
            .snapshot()
            .diagnostics_in_range::<_, usize>(self.range.clone(), false)
            .map(|entry| entry.to_lsp_diagnostic_stub())
            .collect();
        lsp::CodeActionParams {
            text_document: lsp::TextDocumentIdentifier::new(
                lsp::Url::from_file_path(path).unwrap(),
            ),
            range: range_to_lsp(self.range.to_point_utf16(buffer)),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp::CodeActionContext {
                diagnostics: relevant_diagnostics,
                only: self
                    .kinds
                    .clone()
                    .or_else(|| language_server.code_action_kinds()),
                ..lsp::CodeActionContext::default()
            },
        }
    }

    async fn response_from_lsp(
        self,
        actions: Option<lsp::CodeActionResponse>,
        _: Model<Project>,
        _: Model<Buffer>,
        server_id: LanguageServerId,
        _: AsyncAppContext,
    ) -> Result<Vec<CodeAction>> {
        Ok(actions
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                if let lsp::CodeActionOrCommand::CodeAction(lsp_action) = entry {
                    Some(CodeAction {
                        server_id,
                        range: self.range.clone(),
                        lsp_action,
                    })
                } else {
                    None
                }
            })
            .collect())
    }
}

impl GetCodeActions {
    pub fn can_resolve_actions(capabilities: &ServerCapabilities) -> bool {
        capabilities
            .code_action_provider
            .as_ref()
            .and_then(|options| match options {
                lsp::CodeActionProviderCapability::Simple(_is_supported) => None,
                lsp::CodeActionProviderCapability::Options(options) => options.resolve_provider,
            })
            .unwrap_or(false)
    }
}

#[async_trait(?Send)]
impl LspCommand for OnTypeFormatting {
    type Response = Option<Transaction>;
    type LspRequest = lsp::request::OnTypeFormatting;

    fn check_capabilities(&self, server_capabilities: &lsp::ServerCapabilities) -> bool {
        let Some(on_type_formatting_options) =
            &server_capabilities.document_on_type_formatting_provider
        else {
            return false;
        };
        on_type_formatting_options
            .first_trigger_character
            .contains(&self.trigger)
            || on_type_formatting_options
                .more_trigger_character
                .iter()
                .flatten()
                .any(|chars| chars.contains(&self.trigger))
    }

    fn to_lsp(
        &self,
        path: &Path,
        _: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::DocumentOnTypeFormattingParams {
        lsp::DocumentOnTypeFormattingParams {
            text_document_position: lsp::TextDocumentPositionParams::new(
                lsp::TextDocumentIdentifier::new(lsp::Url::from_file_path(path).unwrap()),
                point_to_lsp(self.position),
            ),
            ch: self.trigger.clone(),
            options: lsp_formatting_options(self.options.tab_size),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<Vec<lsp::TextEdit>>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> Result<Option<Transaction>> {
        if let Some(edits) = message {
            let (lsp_adapter, lsp_server) =
                language_server_for_buffer(&project, &buffer, server_id, &mut cx)?;
            Project::deserialize_edits(
                project,
                buffer,
                edits,
                self.push_to_history,
                lsp_adapter,
                lsp_server,
                &mut cx,
            )
            .await
        } else {
            Ok(None)
        }
    }
}

impl InlayHints {
    pub async fn lsp_to_project_hint(
        lsp_hint: lsp::InlayHint,
        buffer_handle: &Model<Buffer>,
        server_id: LanguageServerId,
        resolve_state: ResolveState,
        force_no_type_left_padding: bool,
        cx: &mut AsyncAppContext,
    ) -> anyhow::Result<InlayHint> {
        let kind = lsp_hint.kind.and_then(|kind| match kind {
            lsp::InlayHintKind::TYPE => Some(InlayHintKind::Type),
            lsp::InlayHintKind::PARAMETER => Some(InlayHintKind::Parameter),
            _ => None,
        });

        let position = buffer_handle.update(cx, |buffer, _| {
            let position = buffer.clip_point_utf16(point_from_lsp(lsp_hint.position), Bias::Left);
            if kind == Some(InlayHintKind::Parameter) {
                buffer.anchor_before(position)
            } else {
                buffer.anchor_after(position)
            }
        })?;
        let label = Self::lsp_inlay_label_to_project(lsp_hint.label, server_id)
            .await
            .context("lsp to project inlay hint conversion")?;
        let padding_left = if force_no_type_left_padding && kind == Some(InlayHintKind::Type) {
            false
        } else {
            lsp_hint.padding_left.unwrap_or(false)
        };

        Ok(InlayHint {
            position,
            padding_left,
            padding_right: lsp_hint.padding_right.unwrap_or(false),
            label,
            kind,
            tooltip: lsp_hint.tooltip.map(|tooltip| match tooltip {
                lsp::InlayHintTooltip::String(s) => InlayHintTooltip::String(s),
                lsp::InlayHintTooltip::MarkupContent(markup_content) => {
                    InlayHintTooltip::MarkupContent(MarkupContent {
                        kind: match markup_content.kind {
                            lsp::MarkupKind::PlainText => HoverBlockKind::PlainText,
                            lsp::MarkupKind::Markdown => HoverBlockKind::Markdown,
                        },
                        value: markup_content.value,
                    })
                }
            }),
            resolve_state,
        })
    }

    async fn lsp_inlay_label_to_project(
        lsp_label: lsp::InlayHintLabel,
        server_id: LanguageServerId,
    ) -> anyhow::Result<InlayHintLabel> {
        let label = match lsp_label {
            lsp::InlayHintLabel::String(s) => InlayHintLabel::String(s),
            lsp::InlayHintLabel::LabelParts(lsp_parts) => {
                let mut parts = Vec::with_capacity(lsp_parts.len());
                for lsp_part in lsp_parts {
                    parts.push(InlayHintLabelPart {
                        value: lsp_part.value,
                        tooltip: lsp_part.tooltip.map(|tooltip| match tooltip {
                            lsp::InlayHintLabelPartTooltip::String(s) => {
                                InlayHintLabelPartTooltip::String(s)
                            }
                            lsp::InlayHintLabelPartTooltip::MarkupContent(markup_content) => {
                                InlayHintLabelPartTooltip::MarkupContent(MarkupContent {
                                    kind: match markup_content.kind {
                                        lsp::MarkupKind::PlainText => HoverBlockKind::PlainText,
                                        lsp::MarkupKind::Markdown => HoverBlockKind::Markdown,
                                    },
                                    value: markup_content.value,
                                })
                            }
                        }),
                        location: Some(server_id).zip(lsp_part.location),
                    });
                }
                InlayHintLabel::LabelParts(parts)
            }
        };

        Ok(label)
    }

    pub fn project_to_lsp_hint(hint: InlayHint, snapshot: &BufferSnapshot) -> lsp::InlayHint {
        lsp::InlayHint {
            position: point_to_lsp(hint.position.to_point_utf16(snapshot)),
            kind: hint.kind.map(|kind| match kind {
                InlayHintKind::Type => lsp::InlayHintKind::TYPE,
                InlayHintKind::Parameter => lsp::InlayHintKind::PARAMETER,
            }),
            text_edits: None,
            tooltip: hint.tooltip.and_then(|tooltip| {
                Some(match tooltip {
                    InlayHintTooltip::String(s) => lsp::InlayHintTooltip::String(s),
                    InlayHintTooltip::MarkupContent(markup_content) => {
                        lsp::InlayHintTooltip::MarkupContent(lsp::MarkupContent {
                            kind: match markup_content.kind {
                                HoverBlockKind::PlainText => lsp::MarkupKind::PlainText,
                                HoverBlockKind::Markdown => lsp::MarkupKind::Markdown,
                                HoverBlockKind::Code { .. } => return None,
                            },
                            value: markup_content.value,
                        })
                    }
                })
            }),
            label: match hint.label {
                InlayHintLabel::String(s) => lsp::InlayHintLabel::String(s),
                InlayHintLabel::LabelParts(label_parts) => lsp::InlayHintLabel::LabelParts(
                    label_parts
                        .into_iter()
                        .map(|part| lsp::InlayHintLabelPart {
                            value: part.value,
                            tooltip: part.tooltip.and_then(|tooltip| {
                                Some(match tooltip {
                                    InlayHintLabelPartTooltip::String(s) => {
                                        lsp::InlayHintLabelPartTooltip::String(s)
                                    }
                                    InlayHintLabelPartTooltip::MarkupContent(markup_content) => {
                                        lsp::InlayHintLabelPartTooltip::MarkupContent(
                                            lsp::MarkupContent {
                                                kind: match markup_content.kind {
                                                    HoverBlockKind::PlainText => {
                                                        lsp::MarkupKind::PlainText
                                                    }
                                                    HoverBlockKind::Markdown => {
                                                        lsp::MarkupKind::Markdown
                                                    }
                                                    HoverBlockKind::Code { .. } => return None,
                                                },
                                                value: markup_content.value,
                                            },
                                        )
                                    }
                                })
                            }),
                            location: part.location.map(|(_, location)| location),
                            command: None,
                        })
                        .collect(),
                ),
            },
            padding_left: Some(hint.padding_left),
            padding_right: Some(hint.padding_right),
            data: match hint.resolve_state {
                ResolveState::CanResolve(_, data) => data,
                ResolveState::Resolving | ResolveState::Resolved => None,
            },
        }
    }

    pub fn can_resolve_inlays(capabilities: &ServerCapabilities) -> bool {
        capabilities
            .inlay_hint_provider
            .as_ref()
            .and_then(|options| match options {
                OneOf::Left(_is_supported) => None,
                OneOf::Right(capabilities) => match capabilities {
                    lsp::InlayHintServerCapabilities::Options(o) => o.resolve_provider,
                    lsp::InlayHintServerCapabilities::RegistrationOptions(o) => {
                        o.inlay_hint_options.resolve_provider
                    }
                },
            })
            .unwrap_or(false)
    }
}

#[async_trait(?Send)]
impl LspCommand for InlayHints {
    type Response = Vec<InlayHint>;
    type LspRequest = lsp::InlayHintRequest;

    fn check_capabilities(&self, server_capabilities: &lsp::ServerCapabilities) -> bool {
        let Some(inlay_hint_provider) = &server_capabilities.inlay_hint_provider else {
            return false;
        };
        match inlay_hint_provider {
            lsp::OneOf::Left(enabled) => *enabled,
            lsp::OneOf::Right(inlay_hint_capabilities) => match inlay_hint_capabilities {
                lsp::InlayHintServerCapabilities::Options(_) => true,
                lsp::InlayHintServerCapabilities::RegistrationOptions(_) => false,
            },
        }
    }

    fn to_lsp(
        &self,
        path: &Path,
        buffer: &Buffer,
        _: &Arc<LanguageServer>,
        _: &AppContext,
    ) -> lsp::InlayHintParams {
        lsp::InlayHintParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: lsp::Url::from_file_path(path).unwrap(),
            },
            range: range_to_lsp(self.range.to_point_utf16(buffer)),
            work_done_progress_params: Default::default(),
        }
    }

    async fn response_from_lsp(
        self,
        message: Option<Vec<lsp::InlayHint>>,
        project: Model<Project>,
        buffer: Model<Buffer>,
        server_id: LanguageServerId,
        mut cx: AsyncAppContext,
    ) -> anyhow::Result<Vec<InlayHint>> {
        let (lsp_adapter, lsp_server) =
            language_server_for_buffer(&project, &buffer, server_id, &mut cx)?;
        // `typescript-language-server` adds padding to the left for type hints, turning
        // `const foo: boolean` into `const foo : boolean` which looks odd.
        // `rust-analyzer` does not have the padding for this case, and we have to accommodate both.
        //
        // We could trim the whole string, but being pessimistic on par with the situation above,
        // there might be a hint with multiple whitespaces at the end(s) which we need to display properly.
        // Hence let's use a heuristic first to handle the most awkward case and look for more.
        let force_no_type_left_padding =
            lsp_adapter.name.0.as_ref() == "typescript-language-server";

        let hints = message.unwrap_or_default().into_iter().map(|lsp_hint| {
            let resolve_state = if InlayHints::can_resolve_inlays(lsp_server.capabilities()) {
                ResolveState::CanResolve(lsp_server.server_id(), lsp_hint.data.clone())
            } else {
                ResolveState::Resolved
            };

            let buffer = buffer.clone();
            cx.spawn(move |mut cx| async move {
                InlayHints::lsp_to_project_hint(
                    lsp_hint,
                    &buffer,
                    server_id,
                    resolve_state,
                    force_no_type_left_padding,
                    &mut cx,
                )
                .await
            })
        });
        future::join_all(hints)
            .await
            .into_iter()
            .collect::<anyhow::Result<_>>()
            .context("lsp to project inlay hints conversion")
    }
}
