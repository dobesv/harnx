use super::super::params::*;
use super::super::store::*;
use super::{require_plan, OperationContext};
use anyhow::anyhow;
use harnx_blob_store::plans::{
    create_document, delete_document, get_document, list_documents, parse_note, serialize_note,
    update_document, NoteDocument, NoteFrontMatter,
};
use harnx_core::cid_url::{CidUrl, PlanItem};
use rmcp::model::{CallToolResult, ErrorData};
use serde_json::{json, Value};

pub(crate) async fn list(
    context: &OperationContext<'_>,
    params: ListNotesParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    require_plan(context, &plan).await?;
    let prefix = format!(
        "{}notes/",
        harnx_blob_store::plans::plan_prefix(&plan).map_err(map_internal)?
    );
    let documents = list_documents(context.store, &prefix)
        .await
        .map_err(map_internal)?;
    let notes = documents
        .into_iter()
        .map(|(_, stored)| parse_note(&stored.content).map(|note| note_json(&note)))
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(map_internal)?;
    context.touch(&plan).await;
    result_json(Value::Array(notes))
}

pub(crate) async fn add(
    context: &OperationContext<'_>,
    params: AddNoteParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    require_plan(context, &plan).await?;
    let requested_id = params.id.as_deref().map(validate_item_id).transpose()?;
    for _ in 0..100 {
        let id = requested_id.clone().unwrap_or_else(gen_id);
        let url = item_url(&plan, PlanItem::Note(id));
        let document = NoteDocument {
            front: NoteFrontMatter {
                id: url.to_string(),
                summary: params.summary.clone(),
                author: params.author.clone(),
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body.clone(),
        };
        let serialized = serialize_note(&document).map_err(map_internal)?;
        match create_document(context.store, &url, &serialized).await {
            Ok(_) => {
                let path = format!("{url}.md");
                let diff = diff_text(DiffText {
                    before: "",
                    after: &serialized,
                    path: &path,
                });
                context.touch(&plan).await;
                return result_with_diff(DiffResult {
                    message: format!("added note {url} to plan {plan}"),
                    diff,
                });
            }
            Err(error) => {
                let exists = get_document(context.store, &url)
                    .await
                    .map_err(map_internal)?
                    .is_some();
                if exists && requested_id.is_none() {
                    continue;
                }
                if exists {
                    return Err(ErrorData::invalid_params(
                        format!("note already exists: {url}"),
                        None,
                    ));
                }
                return Err(map_internal(error));
            }
        }
    }
    Err(ErrorData::internal_error(
        "could not allocate a unique note ID",
        None,
    ))
}

pub(crate) async fn get(
    context: &OperationContext<'_>,
    params: GetNoteParams,
) -> Result<CallToolResult, ErrorData> {
    let (plan, url) = note_location(&params.plan, &params.note_id)?;
    let stored = get_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::invalid_params(format!("note not found: {url}"), None))?;
    let note = parse_note(&stored.content).map_err(map_internal)?;
    context.touch(&plan).await;
    result_json(note_json(&note))
}

fn note_location(plan: &str, id: &str) -> Result<(CidUrl, CidUrl), ErrorData> {
    let plan = parse_plan_url(plan)?;
    let note = parse_note_url(&plan, id)?;
    Ok((plan, note))
}

pub(crate) async fn update(
    context: &OperationContext<'_>,
    params: UpdateNoteParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    let url = parse_note_url(&plan, &params.note_id)?;
    let edit_params = params.clone();
    let mut before_body = String::new();
    let stored = update_document(context.store, &url, |content| {
        let note = parse_note(content)?;
        before_body.clone_from(&note.body);
        update_note_document(note, &edit_params)
    })
    .await
    .map_err(map_internal)?;
    let note = parse_note(&stored.content).map_err(map_internal)?;
    let path = format!("{url}.md");
    let diff = diff_text(DiffText {
        before: &before_body,
        after: &note.body,
        path: &path,
    });
    context.touch(&plan).await;
    result_with_diff(DiffResult {
        message: format!("updated note {url}"),
        diff,
    })
}

fn update_note_document(
    mut note: NoteDocument,
    params: &UpdateNoteParams,
) -> anyhow::Result<String> {
    note.front.summary = params.summary.clone().or(note.front.summary);
    note.front.author = params.author.clone().or(note.front.author);
    note.front.updated_at = Some(now_iso());
    note.body = apply_body_edit(
        &note.body,
        BodyEdit {
            replace: params.replace_body.clone(),
            append: params.append_body.clone(),
            replace_in: params.replace_in_body.clone(),
        },
    )
    .map_err(|error| anyhow!(error.message.to_string()))?;
    serialize_note(&note)
}

pub(crate) async fn delete(
    context: &OperationContext<'_>,
    params: DeleteNoteParams,
) -> Result<CallToolResult, ErrorData> {
    let (plan, url) = note_location(&params.plan, &params.note_id)?;
    let content = delete_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::invalid_params(format!("note not found: {url}"), None))?;
    let path = format!("{url}.md");
    let diff = diff_text(DiffText {
        before: &content,
        after: "",
        path: &path,
    });
    context.touch(&plan).await;
    result_with_diff(DiffResult {
        message: format!("deleted note {url}"),
        diff,
    })
}

fn note_json(note: &NoteDocument) -> Value {
    json!({
        "id": note.front.id,
        "summary": note.front.summary,
        "author": note.front.author,
        "created_at": note.front.created_at,
        "updated_at": note.front.updated_at,
        "body": note.body,
    })
}
