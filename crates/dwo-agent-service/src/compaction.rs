use std::sync::Arc;

use anyhow::Result;
use dwo_context::{CompactionPlanner, ContextManager, SystemPromptBuilder};
use dwo_model_client::{ModelClient, ModelSelection, request_with_retry};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::CompactionTrigger;

pub(crate) struct CompactionRequest {
    pub selection: ModelSelection,
    pub trigger: CompactionTrigger,
    pub supplied_summary: Option<String>,
}

pub(crate) struct CompactionResult {
    pub context: ContextManager,
    pub summary: Option<String>,
    pub compacted: bool,
}

pub(crate) async fn execute(
    mut context: ContextManager,
    prompt_builder: &SystemPromptBuilder,
    model: &Arc<dyn ModelClient>,
    tools: &[Value],
    cancellation: &CancellationToken,
    request: CompactionRequest,
) -> Result<CompactionResult> {
    let planner = CompactionPlanner::default();
    let plan = match request.trigger {
        CompactionTrigger::Recovery => planner.build_recovery(context.context()),
        _ => planner.build(context.context()),
    };
    let should_apply = request.supplied_summary.is_some() || plan.needs_replacement();
    if !should_apply {
        return Ok(CompactionResult {
            context,
            summary: None,
            compacted: false,
        });
    }

    let summary = match request.supplied_summary {
        Some(summary) => summary,
        None if plan.has_compactable_history() => {
            let selection = ModelSelection {
                model: context
                    .context()
                    .usage
                    .last_model
                    .clone()
                    .unwrap_or(request.selection.model),
                reasoning: request.selection.reasoning,
            };
            request_with_retry(cancellation, || {
                model.summarize(selection.clone(), plan.view.clone(), cancellation.clone())
            })
            .await?
            .summary
        }
        None => String::new(),
    };
    let display_summary = (!summary.is_empty()).then_some(summary.clone());
    context.apply_compaction(plan, summary, prompt_builder, tools)?;
    Ok(CompactionResult {
        context,
        summary: display_summary,
        compacted: true,
    })
}
