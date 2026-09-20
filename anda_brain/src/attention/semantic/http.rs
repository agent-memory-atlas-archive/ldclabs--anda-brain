use super::*;
use anda_engine::model::reqwest::{Client, Url};
use futures::StreamExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiWatchConfig {
    pub contract: SemanticConfig,
    pub api_key_env: String,
}
pub(super) fn endpoint(value: &str) -> Result<Url, BoxError> {
    let url = Url::parse(value)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if value.len() > 2048
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err("semantic endpoint requires HTTPS (HTTP only on loopback), without URL credentials/query/fragment".into());
    }
    Ok(url)
}
impl OpenAiWatchConfig {
    pub fn resolve(
        &self,
        secret: &mut impl FnMut(&str) -> Option<String>,
    ) -> Result<SemanticBindings, BoxError> {
        self.contract.validate()?;
        if self.api_key_env.is_empty()
            || self.api_key_env.len() > 128
            || !self
                .api_key_env
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("invalid semantic provider secret environment reference".into());
        }
        let token = secret(&self.api_key_env)
            .filter(|s| !s.is_empty())
            .ok_or("semantic provider secret unavailable")?;
        let evaluator = HttpEvaluator {
            pin: self.contract.pin()?,
            client: Client::builder()
                .redirect(anda_engine::model::reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_millis(
                    self.contract.limits.callback_ms,
                ))
                .build()?,
            endpoint: endpoint(&self.contract.endpoint)?,
            token,
            model: self.contract.model.clone(),
        };
        Ok(SemanticBindings {
            config: self.contract.clone(),
            evaluator: Arc::new(evaluator),
        })
    }
}
struct HttpEvaluator {
    pin: RuntimePin,
    client: Client,
    endpoint: Url,
    token: String,
    model: String,
}
#[async_trait]
impl SemanticEvaluator for HttpEvaluator {
    async fn evaluate(&self, request: Json) -> Result<String, BoxError> {
        // Bind the actual compiled client, including its endpoint, to the host
        // pin. Editing a cloned binding's config cannot relabel this client.
        let material: Json = serde_json::from_str(
            request["messages"][1]["content"]
                .as_str()
                .ok_or("semantic_request_missing")?,
        )
        .map_err(|_| "semantic_request_invalid")?;
        if request["model"] != self.model || material["evaluator"] != json!(self.pin) {
            return Err("semantic_client_configuration_mismatch".into());
        }
        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await
            .map_err(|_| "semantic_provider_unavailable")?;
        if !response.status().is_success() || response.content_length().is_some_and(|n| n > 524_288)
        {
            return Err("semantic_provider_response_rejected".into());
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "semantic_provider_response_unavailable")?;
            if body.len() + chunk.len() > 524_288 {
                return Err("semantic_provider_response_budget_exceeded".into());
            }
            body.extend_from_slice(&chunk);
        }
        let value: Json =
            serde_json::from_slice(&body).map_err(|_| "semantic_provider_invalid_json")?;
        if value["model"] != self.model
            || value["choices"].as_array().is_none_or(|v| v.len() != 1)
            || value["choices"][0]["finish_reason"] != "stop"
            || value["choices"][0]["message"]["role"] != "assistant"
            || !value["choices"][0]["message"]["tool_calls"].is_null()
            || !value["choices"][0]["message"]["refusal"].is_null()
        {
            return Err("semantic_provider_model_or_completion_mismatch".into());
        }
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "semantic_provider_content_missing".into())
    }
}
