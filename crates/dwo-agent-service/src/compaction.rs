use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use dwo_context::{CompactionPlanner, ContextManager, SessionContext, SystemPromptBuilder};
use dwo_model_client::{ModelClient, ModelSelection, request_with_retry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::events::{CompactionTrigger, NotificationLevel};
use crate::turn::{ModelStep, TurnError, TurnExecution, TurnUpdate};

pub(crate) struct CompactionRequest {
    pub selection: ModelSelection,
    pub trigger: CompactionTrigger,
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
    let should_apply = plan.needs_replacement();
    if !should_apply {
        return Ok(CompactionResult {
            context,
            summary: None,
            compacted: false,
        });
    }

    let summary = if plan.has_compactable_history() {
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
    } else {
        String::new()
    };
    let display_summary = (!summary.is_empty()).then_some(summary.clone());
    context.apply_compaction(plan, summary, prompt_builder, tools)?;
    Ok(CompactionResult {
        context,
        summary: display_summary,
        compacted: true,
    })
}

pub(crate) async fn compact(
    turn: &mut TurnExecution,
    selection: ModelSelection,
    trigger: CompactionTrigger,
) -> anyhow::Result<bool> {
    let compaction_id = format!("cmp_{}", uuid::Uuid::new_v4().simple());
    turn.emit(TurnUpdate::Notification {
        turn_id: turn.turn_id.clone(),
        category: "compaction_started".to_string(),
        level: NotificationLevel::Info,
        text: "Compacting context...".to_string(),
        data: json!({
            "compactionId": compaction_id,
            "trigger": trigger,
        }),
    });
    let started = Instant::now();
    tracing::info!(
        event = "context.compaction_started",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        "context compaction started"
    );
    let result = async {
        let context = std::mem::replace(
            &mut turn.context,
            ContextManager::new(SessionContext::default()),
        );
        let result = execute(
            context,
            &turn.prompt_builder,
            &turn.model,
            turn.tools.schemas(),
            &turn.cancellation,
            CompactionRequest {
                selection: selection.clone(),
                trigger,
            },
        )
        .await?;
        let compacted = result.compacted;
        turn.context = result.context;
        turn.checkpoint().await?;
        anyhow::Ok((compacted, result.summary))
    }
    .await;
    let (compacted, summary) = match result {
        Ok(result) => result,
        Err(error) => {
            if turn.cancellation.is_cancelled() {
                turn.emit(TurnUpdate::Notification {
                    turn_id: turn.turn_id.clone(),
                    category: "compaction_cancelled".to_string(),
                    level: NotificationLevel::Warning,
                    text: "Context compaction cancelled.".to_string(),
                    data: json!({"compactionId": compaction_id}),
                });
            } else {
                turn.emit(TurnUpdate::Notification {
                    turn_id: turn.turn_id.clone(),
                    category: "compaction_failed".to_string(),
                    level: NotificationLevel::Error,
                    text: "Context compaction failed.".to_string(),
                    data: json!({
                        "compactionId": compaction_id,
                        "error": format!("{error:#}"),
                    }),
                });
            }
            return Err(error);
        }
    };
    turn.emit(TurnUpdate::Notification {
        turn_id: turn.turn_id.clone(),
        category: "compaction_completed".to_string(),
        level: NotificationLevel::Success,
        text: "Context compacted.".to_string(),
        data: json!({
            "compactionId": compaction_id,
            "summary": summary,
            "compacted": compacted,
        }),
    });
    tracing::info!(
        event = "context.compaction_completed",
        session_id = %turn.session_id,
        turn_id = %turn.turn_id,
        model = %selection.model,
        duration_ms = started.elapsed().as_millis() as u64,
        "context compaction completed"
    );
    Ok(compacted)
}

impl TurnExecution {
    pub(crate) async fn maybe_compact(&mut self, model_step: &ModelStep) -> Result<(), TurnError> {
        let compact_trigger_tokens = self
            .model
            .model_limits(&model_step.selection.model)
            .map_err(|error| self.fail("compact context", format!("{error:#}")))?
            .compact_trigger_tokens;
        if self
            .context
            .compaction_due(compact_trigger_tokens, self.tools.schemas())
        {
            let compacted = compact(
                self,
                model_step.selection.clone(),
                CompactionTrigger::Automatic,
            )
            .await;
            if let Err(error) = compacted {
                return Err(self.fail("compact context", format!("{error:#}")));
            }
        }
        Ok(())
    }
}
