#![recursion_limit = "512"]
use actix_web::{error, post, web, App, HttpRequest, HttpResponse, HttpServer};
use anyhow::anyhow;
use github_webhook::payload_types;
use octokit_rs::webhook;
use once_cell::sync::Lazy;
use polodb_core::bson::{doc, to_document};
use polodb_core::results::DeleteResult;
use polodb_core::Database;
use r#final::Final;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{from_value, json, Value};
use sysinfo::{get_current_pid, System};
use tokio::sync::{Mutex, RwLock};

struct FeishuCredential {
    app_id: Final<String>,
    app_secret: Final<String>,
    tenant_access_token: Mutex<Option<String>>,
}

const SUBSCRIPTIONS_COLLECTION: &str = "subscriptions";

#[derive(Serialize, Deserialize, Debug)]
struct Subscription {
    chat_id: String,
    repo: String,
    event: SubscriptionEvent,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
enum SubscriptionEvent {
    PullRequest,
    Issue,
}

struct BotData {
    feishu_credential: FeishuCredential,
    db: RwLock<Database>,
}

enum FeishuNewMessage {
    Text(String),
    Interactive(String),
}

impl FeishuCredential {
    pub async fn refresh_token(&self) -> anyhow::Result<String> {
        // Send the POST request
        let client = Client::new();
        let response = client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&json!({
                "app_id": *self.app_id,
                "app_secret": *self.app_secret
            }))
            .send()
            .await?;

        // Parse the response
        let result: Value = response.json().await?;

        // If the request is successful, update the access token
        if result["code"].as_i64() == Some(0) {
            if let Some(token) = result["tenant_access_token"].as_str() {
                return Ok(token.to_string());
            }
        }

        Err(anyhow!("Bad response"))
    }

    pub async fn request_json<T: Serialize + ?Sized>(
        &self,
        url: &str,
        method: reqwest::Method,
        body: &T,
    ) -> anyhow::Result<Value> {
        let client = Client::new();
        let mut result = None;

        for _ in 0..3 {
            let mut access_token_ref = self.tenant_access_token.lock().await;
            let access_token = {
                match access_token_ref.as_ref() {
                    Some(token) => token,
                    None => {
                        let new_token = self.refresh_token().await?;
                        *access_token_ref = Some(new_token.clone());
                        access_token_ref.as_ref().unwrap()
                    }
                }
            };

            let response = client
                .request(method.clone(), url)
                .bearer_auth(access_token)
                .json(body)
                .send()
                .await?;

            drop(access_token_ref);

            let result_status = response.status();
            let result_value: Value = response.json().await?;
            
            
            if result_status.is_success() {
                result = Some(result_value);
                break;
            }
        }

        result.ok_or(anyhow!("Failed to get a valid response"))
    }

    pub async fn api_send_message(
        &self,
        chat_id: &str,
        content: FeishuNewMessage,
    ) -> anyhow::Result<()> {
        // Construct the request body
        let body = match content {
            FeishuNewMessage::Text(text) => json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": text
            }),
            FeishuNewMessage::Interactive(interactive) => json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": interactive
            }),
        };

        // Send the POST request
        self.request_json(
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id",
            reqwest::Method::POST,
            &body,
        )
        .await?;

        Ok(())
    }
}

async fn feishu_message_handler(
    chat_id: impl AsRef<str>,
    content: impl AsRef<str>,
    db: &RwLock<Database>,
) -> FeishuNewMessage {
    let message = serde_json::from_str::<Value>(content.as_ref());
    if let Err(e) = message {
        return FeishuNewMessage::Text(
            json!({
                "text": format!("Failed to parse message: {}", e)
            })
            .to_string(),
        );
    }
    let message = message.unwrap();
    let text_content = message["text"].as_str().unwrap_or("").trim();
    println!("Received message: {}", text_content);
    static HELP: Lazy<Regex> = Lazy::new(|| Regex::new(r"^@\S+\s+help$").unwrap());
    static PING: Lazy<Regex> = Lazy::new(|| Regex::new(r"^@\S+\s+ping$").unwrap());
    static SUBSCRIBE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^@\S+\s+subscribe\s+(\S+)\s+(pr|issue)$").unwrap());
    static UNSUBSCRIBE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^@\S+\s+unsubscribe\s+(\S+)\s+(pr|issue)$").unwrap());
    static LIST: Lazy<Regex> = Lazy::new(|| Regex::new(r"^@\S+\s+list$").unwrap());

    if HELP.is_match(text_content) {
        FeishuNewMessage::Text(
            json!({
                "text": r#"Available commands:
- `@bot help`: Show this help message
- `@bot ping`: Check if the bot is alive
- `@bot subscribe <repo> <pr|issue>`: Subscribe to the events of a repository
- `@bot unsubscribe <repo> <pr|issue>`: Unsubscribe from the events of a repository
- `@bot list`: List all the subscriptions
"#
            })
            .to_string(),
        )
    } else if PING.is_match(text_content) {
        let s = System::new_all();
        let rss = get_current_pid()
            .and_then(|pid| s.process(pid).ok_or("Failed to get process"))
            .map(|process| process.memory());
        let free_memory = s.available_memory();
        let total_memory = s.total_memory();
        let rss_mb: f64 = rss.unwrap_or(0) as f64 / 1024.0 / 1024.0;
        let free_memory_mb: f64 = free_memory as f64 / 1024.0 / 1024.0;
        let total_memory_mb: f64 = total_memory as f64 / 1024.0 / 1024.0;
        FeishuNewMessage::Text(json!({
            "text": format!("Pong!\nMemory Usage: {:?} MiB\nFree Memory: {} MiB / {} MiB", rss_mb, free_memory_mb, total_memory_mb)
        }).to_string())
    } else if let Some(captures) = SUBSCRIBE.captures(text_content) {
        let repo = &captures[1];
        let event = match &captures[2] {
            "pr" => SubscriptionEvent::PullRequest,
            "issue" => SubscriptionEvent::Issue,
            _ => {
                return FeishuNewMessage::Text(
                    json!({
                        "text": "Invalid event type"
                    })
                    .to_string(),
                )
            }
        };
        let subscription = Subscription {
            chat_id: chat_id.as_ref().to_string(),
            repo: repo.to_string(),
            event,
        };
        let db = db.write().await;
        let col = db.collection(SUBSCRIPTIONS_COLLECTION);
        let result = to_document(&subscription)
            .map_err(|e| anyhow!("Failed to convert subscription to document: {}", e))
            .and_then(|doc| {
                col.find_one(doc)
                    .map_err(|e| anyhow!("Failed to find subscription: {}", e))
            });
        match result {
            Ok(Some(_)) => {
                return FeishuNewMessage::Text(
                    json!({
                        "text": format!("Already subscribed to {} {:?}", repo, event)
                    })
                    .to_string(),
                )
            }
            Err(e) => {
                return FeishuNewMessage::Text(
                    json!({
                        "text": format!("Failed to check subscription existence: {}", e)
                    })
                    .to_string(),
                )
            }
            _ => {}
        }
        match col.insert_one(subscription) {
            Ok(_) => FeishuNewMessage::Text(
                json!({
                    "text": format!("Subscribed to {} {:?}", repo, event)
                })
                .to_string(),
            ),
            Err(e) => FeishuNewMessage::Text(
                json!({
                    "text": format!("Failed to subscribe: {}", e)
                })
                .to_string(),
            ),
        }
    } else if let Some(captures) = UNSUBSCRIBE.captures(text_content) {
        let repo = &captures[1];
        let event = match &captures[2] {
            "pr" => SubscriptionEvent::PullRequest,
            "issue" => SubscriptionEvent::Issue,
            _ => {
                return FeishuNewMessage::Text(
                    json!({
                        "text": "Invalid event type"
                    })
                    .to_string(),
                )
            }
        };
        let subscription = Subscription {
            chat_id: chat_id.as_ref().to_string(),
            repo: repo.to_string(),
            event,
        };
        let db = db.write().await;
        let col = db.collection::<Subscription>(SUBSCRIPTIONS_COLLECTION);
        let doc = to_document(&subscription);
        if let Err(e) = doc {
            return FeishuNewMessage::Text(
                json!({
                    "text": format!("Failed to convert subscription to document: {}", e)
                })
                .to_string(),
            );
        }
        let doc = doc.unwrap();
        match col.delete_one(doc) {
            Ok(DeleteResult { deleted_count: 0 }) => FeishuNewMessage::Text(
                json!({
                    "text": format!("Have not subscribed to {} {:?}", repo, event)
                })
                .to_string(),
            ),
            Ok(DeleteResult { deleted_count: _ }) => FeishuNewMessage::Text(
                json!({
                    "text": format!("Unsubscribed from {} {:?}", repo, event)
                })
                .to_string(),
            ),
            Err(e) => FeishuNewMessage::Text(
                json!({
                    "text": format!("Failed to unsubscribe: {}", e)
                })
                .to_string(),
            ),
        }
    } else if LIST.is_match(text_content) {
        let db = db.read().await;
        let col = db.collection::<Subscription>(SUBSCRIPTIONS_COLLECTION);
        let cursor = col.find(doc! {"chat_id": chat_id.as_ref()});
        if let Err(e) = cursor {
            return FeishuNewMessage::Text(
                json!({
                    "text": format!("Failed to list subscriptions: {}", e)
                })
                .to_string(),
            );
        }
        let cursor = cursor.unwrap();
        let mut subscriptions = Vec::new();
        for result in cursor {
            match result {
                Ok(subscription) => subscriptions.push(subscription),
                Err(e) => {
                    return FeishuNewMessage::Text(
                        json!({
                            "text": format!("Failed to list subscriptions: {}", e)
                        })
                        .to_string(),
                    )
                }
            }
        }
        FeishuNewMessage::Text(
            json!({
                "text": format!("Subscriptions: \n{}", subscriptions.into_iter().map(|s| format!("{} {:?}", s.repo, s.event)).collect::<Vec<String>>().join("\n"))
            })
            .to_string(),
        )
    } else {
        FeishuNewMessage::Text(
            json!({
                "text": "Unsupported command"
            })
            .to_string(),
        )
    }
}

#[post("/feishu")]
async fn feishu_handler(
    info: web::Json<Value>,
    bot_data: web::Data<BotData>,
) -> actix_web::Result<HttpResponse> {
    if info["type"].as_str() == Some("url_verification") {
        let challenge = info["challenge"]
            .as_str()
            .ok_or(error::ErrorBadRequest("No challenge in request"))?;
        return Ok(HttpResponse::Ok().json(json!({
            "challenge": challenge
        })));
    }

    // require schema 2.0
    if info["schema"]
        .as_str()
        .ok_or(error::ErrorBadRequest("No schema in request"))?
        != "2.0"
    {
        return Err(error::ErrorBadRequest("Unsupported schema"));
    }

    // get event type
    let event_type = info["header"]["event_type"]
        .as_str()
        .ok_or(error::ErrorBadRequest("No event type in request"))?;
    match event_type {
        "im.message.receive_v1" => {
            let message_json = info["event"]["message"]["content"]
                .as_str()
                .ok_or(error::ErrorBadRequest("No message content in request"))?;
            let chat_id = info["event"]["message"]["chat_id"]
                .as_str()
                .ok_or(error::ErrorBadRequest("No chat id in request"))?;
            bot_data
                .feishu_credential
                .api_send_message(
                    chat_id,
                    feishu_message_handler(chat_id, message_json, &bot_data.db).await,
                )
                .await
                .map_err(|e| error::ErrorBadRequest(format!("Failed to send message: {}", e)))?;
            Ok(HttpResponse::Ok().body(format!("Received message: {}", message_json)))
        }
        _ => Ok(HttpResponse::Ok().body("Unsupported event type")),
    }
}

#[post("/github")]
async fn github_handler(
    req_body: web::Json<Value>,
    req: HttpRequest,
    bot_data: web::Data<BotData>,
) -> actix_web::Result<HttpResponse> {
    let event_type = req
        .headers()
        .get("X-GitHub-Event")
        .ok_or(error::ErrorBadRequest(
            "No X-GitHub-Event header in request",
        ))?
        .to_str()
        .map_err(|e| {
            error::ErrorBadRequest(format!("Failed to parse X-GitHub-Event header: {}", e))
        })?;

    let body = req_body.into_inner();
    println!(
        "Received GitHub event: {}, event type: {}",
        body, event_type
    );
    match event_type {
        "issues" | "issue_comment" => {
            let repo;
            let repo_url;
            let action;
            let issue_number;
            let issue_title;
            let issue_url;
            let sender_login;
            let sender_url;
            let content_body;
            if let Ok(issue_open) = from_value::<webhook::IssuesOpened>(body.clone()) {
                repo = issue_open.repository.full_name.clone();
                repo_url = issue_open.repository.html_url.clone();
                action = "Opened".to_string();
                issue_number = issue_open.issue.number;
                issue_title = issue_open.issue.title.clone();
                issue_url = issue_open.issue.html_url.clone();
                sender_login = issue_open.sender.login.clone();
                sender_url = issue_open.sender.html_url.clone();
                content_body = issue_open.issue.body.unwrap_or_default();
            } else if let Ok(issue_comment) =
                from_value::<webhook::IssueCommentCreated>(body.clone())
            {
                repo = issue_comment.repository.full_name.clone();
                repo_url = issue_comment.repository.html_url.clone();
                action = "Commented".to_string();
                issue_number = issue_comment.issue.number;
                issue_title = issue_comment.issue.title.clone();
                issue_url = issue_comment.issue.html_url.clone();
                sender_login = issue_comment.sender.login.clone();
                sender_url = issue_comment.sender.html_url.clone();
                content_body = issue_comment.comment.body.clone();
            } else if let Ok(issue_closed) = from_value::<webhook::IssuesClosed>(body.clone()) {
                repo = issue_closed.repository.full_name.clone();
                repo_url = issue_closed.repository.html_url.clone();
                action = "Closed".to_string();
                issue_number = issue_closed.issue.number;
                issue_title = issue_closed.issue.title.clone();
                issue_url = issue_closed.issue.html_url.clone();
                sender_login = issue_closed.sender.login.clone();
                sender_url = issue_closed.sender.html_url.clone();
                content_body = issue_closed.issue.body.unwrap_or_default();
            } else if let Ok(issue_reopened) = from_value::<webhook::IssuesReopened>(body.clone()) {
                repo = issue_reopened.repository.full_name.clone();
                repo_url = issue_reopened.repository.html_url.clone();
                action = "Reopened".to_string();
                issue_number = issue_reopened.issue.number;
                issue_title = issue_reopened.issue.title.clone();
                issue_url = issue_reopened.issue.html_url.clone();
                sender_login = issue_reopened.sender.login.clone();
                sender_url = issue_reopened.sender.html_url.clone();
                content_body = issue_reopened.issue.body.unwrap_or_default();
            } else {
                return Ok(HttpResponse::NoContent().finish());
            }
            let db = bot_data.db.read().await;
            let col = db.collection::<Subscription>(SUBSCRIPTIONS_COLLECTION);
            let all_chats = col
                .find(doc! { "repo": repo.clone(), "event": "Issue" })
                .map_err(|e| {
                    error::ErrorNotFound(format!("Failed to find subscriptions: {}", e))
                })?;

            // build message card
            let header = json!({
                    "template": "blue",
                    "title":{
                        "content": format!("[{}] #{} {}", action, issue_number, issue_title),
                        "tag": "plain_text"
                    }
                }
            );
            let content = json!({
                "tag": "markdown",
                "content": format!("{} by [{}]({}).\n[#{} {}]({})\n{}", action, sender_login, sender_url, issue_number, issue_title, issue_url, content_body),
            });
            let divider = json!({"tag": "hr"});
            let repo_link = json!({
                "tag": "note",
                "elements": [
                    {
                        "tag": "lark_md",
                        "content": format!("[{}]({})", repo, repo_url)
                    }
                ]
            });
            let card = json!({
                "config": {
                    "wide_screen_mode": true
                },
                "elements": [content, divider, repo_link],
                "header": header
            })
            .to_string();

            for chat in all_chats {
                if let Ok(chat) = chat {
                    bot_data
                        .feishu_credential
                        .api_send_message(
                            &chat.chat_id,
                            FeishuNewMessage::Interactive(card.clone()),
                        )
                        .await
                        .map_err(|e| {
                            error::ErrorBadRequest(format!("Failed to send message: {}", e))
                        })?;
                }
            }
        }
        "pull_request" => {
            let repo;
            let repo_url;
            let action;
            let pr_number;
            let pr_title;
            let pr_url;
            let sender_login;
            let sender_url;
            let content_body;
            
            if let Ok(pr_open) = payload_types::PullRequestOpenedEvent::deserialize(&body) {
                repo = pr_open.repository.full_name;
                repo_url = pr_open.repository.html_url;
                action = "Opened".to_string();
                pr_number = pr_open.pull_request.pull_request.number;
                pr_title = pr_open.pull_request.pull_request.title;
                pr_url = pr_open.pull_request.pull_request.html_url;
                sender_login = pr_open.sender.login;
                sender_url = pr_open.sender.html_url;
                content_body = pr_open.pull_request.pull_request.body.unwrap_or_default();
            }  else if let Ok(pr_closed) = payload_types::PullRequestClosedEvent::deserialize(&body) {
                repo = pr_closed.repository.full_name;
                repo_url = pr_closed.repository.html_url;
                action = "Closed".to_string();
                pr_number = pr_closed.pull_request.pull_request.number;
                pr_title = pr_closed.pull_request.pull_request.title;
                pr_url = pr_closed.pull_request.pull_request.html_url;
                sender_login = pr_closed.sender.login;
                sender_url = pr_closed.sender.html_url;
                content_body = pr_closed.pull_request.pull_request.body.unwrap_or_default();
            } else if let Ok(pr_reopened) = payload_types::PullRequestReopenedEvent::deserialize(&body)
            {
                repo = pr_reopened.repository.full_name;
                repo_url = pr_reopened.repository.html_url;
                action = "Reopened".to_string();
                pr_number = pr_reopened.pull_request.pull_request.number;
                pr_title = pr_reopened.pull_request.pull_request.title;
                pr_url = pr_reopened.pull_request.pull_request.html_url;
                sender_login = pr_reopened.sender.login;
                sender_url = pr_reopened.sender.html_url;
                content_body = pr_reopened.pull_request.pull_request.body.unwrap_or_default();
            } else {
                return Ok(HttpResponse::BadRequest().finish());
            }
            
            println!("{} {} {} {} {} {} {}", repo, repo_url, action, pr_number, pr_title, pr_url, sender_login);
            let db = bot_data.db.read().await;
            let col = db.collection::<Subscription>(SUBSCRIPTIONS_COLLECTION);
            let all_chats = col
                .find(doc! { "repo": repo, "event": "PullRequest" })
                .map_err(|e| {
                    error::ErrorNotFound(format!("Failed to find subscriptions: {}", e))
                })?;

            // build message card
            let header = json!({
                    "template": "blue",
                    "title":{
                        "content": format!("[{}] #{} {}", action, pr_number, pr_title),
                        "tag": "plain_text"
                    }
                }
            );
            let content = json!({
                "tag": "markdown",
                "content": format!("{} by [{}]({}).\n[#{} {}]({})\n{}", action, sender_login, sender_url, pr_number, pr_title, pr_url, content_body),
            });
            let divider = json!({"tag": "hr"});
            let repo_link = json!({
                "tag": "note",
                "elements": [
                    {
                        "tag": "lark_md",
                        "content": format!("[{}]({})", repo, repo_url)
                    }
                ]
            });
            let card = json!({
                "config": {
                    "wide_screen_mode": true
                },
                "elements": [content, divider, repo_link],
                "header": header
            })
            .to_string();

            println!("Sending message: {}", card);
            for chat in all_chats {
                if let Ok(chat) = chat {
                    bot_data
                        .feishu_credential
                        .api_send_message(
                            &chat.chat_id,
                            FeishuNewMessage::Interactive(card.clone()),
                        )
                        .await
                        .map_err(|e| {
                            error::ErrorBadRequest(format!("Failed to send message: {}", e))
                        })?;
                }
            }

            return Ok(HttpResponse::NoContent().finish());
        }
        _ => {}
    }

    Ok(HttpResponse::NoContent().finish())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let app_id = std::env::var("FEISHU_APP_ID").expect("FEISHU_APP_ID must be set");
    let app_secret = std::env::var("FEISHU_APP_SECRET").expect("FEISHU_APP_SECRET must be set");
    let db = Database::open_file("app.polo.db")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let bot_data = web::Data::new(BotData {
        feishu_credential: FeishuCredential {
            app_id: Final::new(app_id),
            app_secret: Final::new(app_secret),
            tenant_access_token: Mutex::new(None),
        },
        db: RwLock::new(db),
    });
    HttpServer::new(move || {
        App::new()
            .service(feishu_handler)
            .service(github_handler)
            .app_data(bot_data.clone())
    })
    .bind(("0.0.0.0", 18235))?
    .run()
    .await
}
