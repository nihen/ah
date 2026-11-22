// SPDX-License-Identifier: MIT
//! Compile as an example of codex-app-server-client in the upstream v0.171.0 workspace.
use codex_app_server_client::{EmbeddedNetworkPolicy, legacy_core::config::Config};
use codex_cloud_client::{
    Client, Credentials, HistoryOrder, HistoryQuery, ItemSummary, ListItemsRequest,
    ThreadListQuery, ThreadSortDirection, ThreadSortKey, UserMessageInput,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;

#[derive(Deserialize)]
struct Request {
    owner: String,
    account: String,
    id: Option<String>,
    cursor: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> color_eyre::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(16_385).read_to_end(&mut input)?;
    if input.len() > 16_384 {
        return Err(color_eyre::eyre::eyre!("request too large"));
    }
    let request: Request = serde_json::from_slice(&input)?;
    let mut config = Config::load_with_cli_overrides(Vec::new()).await?;
    if config
        .config_layer_stack
        .requirements_toml()
        .config_server
        .is_some()
    {
        return Err(color_eyre::eyre::eyre!(
            "use the stock TUI with a managed config server"
        ));
    }
    EmbeddedNetworkPolicy::default().activate(&mut config);
    let auth = config
        .auth_config()
        .load_auth(false)
        .await?
        .ok_or_else(|| color_eyre::eyre::eyre!("Codex sign-in required"))?;
    if !auth.is_chatgpt_auth() || auth.is_fedramp_account() {
        return Err(color_eyre::eyre::eyre!(
            "Cloud does not support this auth route"
        ));
    }
    let tokens = auth.get_token_data()?;
    // The exact validated snapshot also supplies the bearer token used below.
    if tokens.login_owner_id.as_deref() != Some(&request.owner)
        || tokens.account_id.as_deref() != Some(&request.account)
    {
        return Err(color_eyre::eyre::eyre!(
            "Codex login changed or selected account differs"
        ));
    }
    let factory = config.http_client_factory();
    let policy = factory.network_policy().clone().for_current_account();
    let factory = factory.with_network_policy(policy);
    let client = Client::new(
        &factory,
        "https://codex-cloud-backend.chatgpt.com",
        Credentials::new(&tokens.access_token, &request.account)?,
    )?;
    let result = if let Some(id) = request.id {
        if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(color_eyre::eyre::eyre!("invalid Cloud thread ID"));
        }
        let page = client
            .list_items(&ListItemsRequest {
                thread_id: id.clone(),
                turn_id: None,
                page: HistoryQuery {
                    cursor: request.cursor,
                    limit: Some(100),
                    order: HistoryOrder::Ascending,
                    ..Default::default()
                },
            })
            .await?;
        if page.thread_id != id {
            return Err(color_eyre::eyre::eyre!("Cloud thread ID mismatch"));
        }
        let created = page
            .data
            .iter()
            .filter_map(|row| row.started_at_ms)
            .min()
            .map(|ms| ms / 1000);
        let updated = page
            .data
            .iter()
            .filter_map(|row| row.completed_at_ms.or(row.started_at_ms))
            .max()
            .map(|ms| ms / 1000);
        let messages: Vec<Value> = page
            .data
            .into_iter()
            .filter_map(|row| match row.summary? {
                ItemSummary::UserMessage { content, .. } => {
                    let text = content
                        .into_iter()
                        .filter_map(|part| match part {
                            UserMessageInput::Text { text, .. } => Some(text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!text.is_empty()).then(|| json!({"role":"user","text":text}))
                }
                ItemSummary::AgentMessage { text, .. } => {
                    (!text.is_empty()).then(|| json!({"role":"assistant","text":text}))
                }
                _ => None,
            })
            .collect();
        json!({"id":id,"messages":messages,"nextCursor":page.next_cursor,"created":created,"updated":updated})
    } else {
        let page = client
            .list_threads(&ThreadListQuery {
                cursor: request.cursor,
                limit: Some(20),
                sort_key: ThreadSortKey::UpdatedAt,
                sort_direction: ThreadSortDirection::Desc,
                ..Default::default()
            })
            .await?;
        let data: Vec<Value> = page
            .data
            .into_iter()
            .map(|r| {
                let title = r.name.filter(|s| !s.is_empty()).unwrap_or(r.preview);
                json!({"id":r.id,"name":title,"created":r.created_at,"updated":r.updated_at})
            })
            .collect();
        json!({"data":data,"nextCursor":page.next_cursor})
    };
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}
