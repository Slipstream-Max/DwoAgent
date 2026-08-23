use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentModelConfig, BaseClient, ModelCatalog, ModelClient, ModelClientConfig, ModelClientError,
    ModelLimits, ModelReply, ModelSelection, ModelStreamEvent, SummaryReply,
};

pub struct ConfiguredModelClient {
    config: ModelClientConfig,
    providers: BTreeMap<String, Arc<BaseClient>>,
}

impl ConfiguredModelClient {
    pub fn new(
        catalog: &ModelCatalog,
        agent: &AgentModelConfig,
    ) -> Result<Arc<Self>, ModelClientError> {
        let config = ModelClientConfig::resolve(catalog, agent)?;
        Self::from_resolved(config)
    }

    pub fn from_resolved(config: ModelClientConfig) -> Result<Arc<Self>, ModelClientError> {
        let providers = config
            .providers
            .iter()
            .map(|(id, config)| Ok((id.clone(), Arc::new(BaseClient::new(config.clone())?))))
            .collect::<Result<BTreeMap<_, _>, ModelClientError>>()?;
        Ok(Arc::new(Self { config, providers }))
    }

    pub fn from_builtin(agent: &AgentModelConfig) -> Result<Arc<Self>, ModelClientError> {
        Self::new(&ModelCatalog::builtin()?, agent)
    }

    pub fn from_yaml(catalog: &str, agent: &str) -> Result<Arc<Self>, ModelClientError> {
        Self::new(
            &ModelCatalog::from_yaml(catalog)?,
            &AgentModelConfig::from_yaml(agent)?,
        )
    }

    pub fn default_model(&self) -> &str {
        &self.config.default_model
    }

    pub fn default_reasoning(&self) -> Option<&str> {
        self.config.default_reasoning.as_deref()
    }

    fn resolve(&self, alias: &str) -> Result<(&BaseClient, &crate::ModelConfig), ModelClientError> {
        let model = self
            .config
            .models
            .get(alias)
            .ok_or_else(|| ModelClientError::UnknownModel(alias.to_string()))?;
        let provider = self.providers.get(&model.provider).ok_or_else(|| {
            ModelClientError::config(format!("provider disappeared: {}", model.provider))
        })?;
        Ok((provider.as_ref(), model))
    }
}

#[async_trait]
impl ModelClient for ConfiguredModelClient {
    fn model_limits(&self, model: &str) -> Result<ModelLimits, ModelClientError> {
        let (_, model) = self.resolve(model)?;
        let max_input_tokens = model.max_input_tokens()?;
        Ok(ModelLimits {
            context_window_tokens: model.context_window_tokens,
            max_output_tokens: model.max_output_tokens,
            max_input_tokens,
            compact_trigger_tokens: ((max_input_tokens as f64) * model.compaction_trigger_ratio)
                .floor()
                .max(1.0) as u64,
        })
    }

    fn supports_image_input(&self, model: &str) -> Result<bool, ModelClientError> {
        Ok(self.resolve(model)?.1.capabilities.image_input)
    }

    fn provider_id(&self, model: &str) -> Result<String, ModelClientError> {
        Ok(self.resolve(model)?.1.provider.clone())
    }

    fn context_owner_id(&self, model: &str) -> Result<String, ModelClientError> {
        Ok(self.resolve(model)?.1.context_owner_id())
    }

    fn reasoning_modes(&self, model: &str) -> Result<Vec<String>, ModelClientError> {
        Ok(self.resolve(model)?.1.reasoning.keys().cloned().collect())
    }

    fn validate_selection(&self, selection: &ModelSelection) -> Result<(), ModelClientError> {
        let (_, model) = self.resolve(&selection.model)?;
        let mode = selection
            .reasoning
            .as_deref()
            .unwrap_or(&model.default_reasoning_mode);
        if mode != "auto" && !model.reasoning.contains_key(mode) {
            return Err(ModelClientError::config(format!(
                "model {} does not configure reasoning mode {mode}",
                model.model_id
            )));
        }
        Ok(())
    }

    async fn stream_turn(
        &self,
        selection: ModelSelection,
        messages: &[dwo_context::ContextMessage],
        tools: &[serde_json::Value],
        events: mpsc::UnboundedSender<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        self.validate_selection(&selection)?;
        if messages
            .first()
            .is_none_or(|message| message.role != dwo_context::MessageRole::System)
        {
            return Err(ModelClientError::protocol(
                "turn messages must start with the system message",
            ));
        }
        let (provider, model) = self.resolve(&selection.model)?;
        provider
            .stream(
                model,
                messages,
                tools,
                selection.reasoning.as_deref(),
                &events,
                cancellation,
            )
            .await
    }

    async fn complete(
        &self,
        selection: ModelSelection,
        messages: Vec<dwo_context::ContextMessage>,
        cancellation: CancellationToken,
    ) -> Result<ModelReply, ModelClientError> {
        self.validate_selection(&selection)?;
        if messages
            .first()
            .is_none_or(|message| message.role != dwo_context::MessageRole::System)
        {
            return Err(ModelClientError::protocol(
                "completion messages must start with the system message",
            ));
        }
        let (provider, model) = self.resolve(&selection.model)?;
        provider
            .complete(
                model,
                &messages,
                selection.reasoning.as_deref(),
                &cancellation,
            )
            .await
    }

    async fn summarize(
        &self,
        selection: ModelSelection,
        view: dwo_context::CompactionView,
        cancellation: CancellationToken,
    ) -> Result<SummaryReply, ModelClientError> {
        let mut messages = Vec::with_capacity(view.messages.len() + 1);
        messages.push(dwo_context::ContextMessage::system(view.instruction));
        messages.extend(view.messages);
        let response = self.complete(selection, messages, cancellation).await?;
        if response.content.trim().is_empty() {
            return Err(ModelClientError::invalid_response(
                "summary response content must not be empty",
            ));
        }
        Ok(SummaryReply {
            summary: response.content,
            usage: response.usage,
        })
    }
}
