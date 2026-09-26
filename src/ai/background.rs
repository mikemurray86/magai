//! Helper models magai runs in the background: the memory fact extractor
//! (`[memory.extractor]`) and the quality judge (`[quality.judge]`) after each
//! turn, and the `smart` permission reviewer (`[permissions.reviewer]`) before
//! each dangerous tool call.
//! Both resolve through the same provider path as the chat model, so either
//! can run on any `[[named_models]]` alias rather than only a local Ollama tag.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use rig::message::Message;

use crate::approval::review::DEFAULT_REVIEWER_PROMPT;
use crate::approval::ReviewFn;
use crate::config::{BackgroundModelConfig, Config};
use crate::memory::extract::DEFAULT_EXTRACTOR_PROMPT;
use crate::memory::quality::DEFAULT_JUDGE_PROMPT;

use super::providers::{resolve_agent_with_preamble, DynAgent};
use super::stream::OurItem;

pub(crate) struct BackgroundModel {
    agent: DynAgent,
    timeout: Duration,
}

impl BackgroundModel {
    /// Builds the helper `settings` describes, running on `model` with
    /// `default_prompt` unless a custom prompt is configured. `Ok(None)` when
    /// no model is set; `Err` when one is set but unusable (unknown alias,
    /// missing API key, unreadable `prompt_file`), so startup can say so once
    /// instead of every turn failing silently. `section` names the config
    /// table in that message.
    pub(crate) fn from_config(
        section: &str,
        model: Option<&str>,
        settings: &BackgroundModelConfig,
        default_prompt: &str,
        config: &Config,
    ) -> Result<Option<Self>, String> {
        let Some(model) = model else {
            return Ok(None);
        };
        let prompt = settings
            .resolve_prompt()
            .map_err(|e| format!("[{section}] {e}"))?
            .unwrap_or_else(|| default_prompt.to_string());
        let agent = resolve_agent_with_preamble(model, config, &prompt, None)
            .map_err(|e| format!("[{section}] model {model:?}: {e}"))?;
        Ok(Some(Self {
            agent,
            timeout: settings.timeout(),
        }))
    }

    /// One tool-less, history-less exchange, collected into a string. `None`
    /// on timeout or provider error: callers are fire-and-forget, so a failed
    /// call is simply dropped rather than interrupting the agent.
    pub(crate) async fn complete(&self, text: String) -> Option<String> {
        let call = async {
            let mut stream = self
                .agent
                .stream_chat(Message::user(text), Vec::new(), 1)
                .await;
            let mut reply = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    OurItem::Text(t) => reply.push_str(&t),
                    OurItem::Error(_) => return None,
                    _ => {}
                }
            }
            Some(reply)
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .ok()
            .flatten()
    }
}

/// Every background helper this session runs, built once in `run_agent`.
#[derive(Default)]
pub(crate) struct Helpers {
    pub(crate) extractor: Option<Arc<BackgroundModel>>,
    pub(crate) judge: Option<Arc<BackgroundModel>>,
}

impl Helpers {
    /// Builds the extractor and judge that `config` enables, given whether the
    /// memory db (which both write to) opened. A helper that is configured but
    /// unusable is reported through `on_error` and left off.
    pub(crate) fn from_config(
        config: &Config,
        have_db: bool,
        mut on_error: impl FnMut(String),
    ) -> Self {
        if !have_db {
            return Self::default();
        }
        let mut build = |section: &str, model, settings, prompt: &str| {
            BackgroundModel::from_config(section, model, settings, prompt, config)
                .unwrap_or_else(|e| {
                    on_error(format!("{e} — disabled"));
                    None
                })
                .map(Arc::new)
        };
        let extractor = build(
            "memory.extractor",
            config.memory.extractor_model(),
            &config.memory.extractor,
            DEFAULT_EXTRACTOR_PROMPT,
        );
        let judge = if config.quality.enabled {
            build(
                "quality.judge",
                config.quality.judge_model(),
                &config.quality.judge,
                DEFAULT_JUDGE_PROMPT,
            )
        } else {
            None
        };
        Self { extractor, judge }
    }
}

/// The `smart` permission reviewer, if `[permissions.reviewer]` names a model.
/// Unlike [`Helpers`] it does not depend on the memory db.
pub(crate) fn build_reviewer(config: &Config) -> Result<Option<ReviewFn>, String> {
    let settings = &config.permissions.reviewer;
    let model = BackgroundModel::from_config(
        "permissions.reviewer",
        settings.model_or(None),
        settings,
        DEFAULT_REVIEWER_PROMPT,
        config,
    )?;
    Ok(model.map(|m| {
        let m = Arc::new(m);
        let review: ReviewFn = Arc::new(move |text| {
            let m = Arc::clone(&m);
            Box::pin(async move { m.complete(text).await })
        });
        review
    }))
}
