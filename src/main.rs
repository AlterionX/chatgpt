use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::RequestBuilder;
use reqwest::header::HeaderMap;
use serenity::async_trait;
use serenity::model::application::interaction::{Interaction, autocomplete::AutocompleteInteraction, application_command::ApplicationCommandInteraction, message_component::MessageComponentInteraction};
use serenity::model::prelude::command::{Command, CommandOptionType};
use serenity::model::prelude::{UserId, Ready};
use serenity::prelude::*;
use serenity::model::channel::Message;

use tracing_subscriber::{
    prelude::*,
    fmt,
    EnvFilter,
    registry,
};

const OPENAI_API_KEY: &'static str = "";
const OPENAI_ORG_ID: &'static str = "";

const DISCORD_SECRET: &'static str = "";
const DISCORD_TOKEN: &'static str = "";

pub struct LoggingCfg {
    level: String,
    filter: Option<String>,
}

pub fn setup_logging(cfg: LoggingCfg) {
    // This should really go in the environment, but should suffice. If it gets any more complicated,
    // we'll use the environment.
    // const LOGGING_FILTER: &'static str = "tracing::span=warn,rustls=warn,h2=warn,tungstenite=warn,hyper=warn,reqwest=warn,serenity=warn";
    const LOGGING_FILTER: &'static str = "rustls=warn,h2=warn,tungstenite=warn,hyper=warn,reqwest=warn,serenity=warn";

    let level = cfg.level.as_str();
    let filter: Cow<_> = if let Some(filter) = cfg.filter {
        filter.into()
    } else {
        LOGGING_FILTER.into()
    };
    let filter: Cow<_> = if filter.is_empty() {
        level.into()
    } else {
        format!("{level},{filter}").into()
    };
    println!("Logging is being initialized with {filter}.");
    let filter = EnvFilter::builder()
        .parse(filter.as_ref())
        .expect("logging.level to be a valid log level/logging.filter to be a valid filter");

    let logger = fmt::layer();

    registry()
        .with(filter)
        .with(logger)
        .init();

    log::info!("Logging initialized successfully.");
}

async fn build_client() -> serenity::Result<Client> {
    // Login with a bot token from the environment
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    Client::builder(DISCORD_TOKEN, intents)
        .event_handler(Handler {
            chat_histories: Mutex::new(HashMap::new()),
        })
        .await
}

struct Handler {
    chat_histories: Mutex<HashMap<UserId, Arc<Mutex<String>>>>,
}

fn build_openai_client() -> Result<reqwest::Client, ()> {
    let mut default_client_headers = HeaderMap::new();
    // Bearer Auth
    default_client_headers.insert("Authorization", format!("Bearer {OPENAI_API_KEY}").try_into().expect("API key header is valid"));

    let res = reqwest::Client::builder()
        .default_headers(default_client_headers)
        .build();

    res.ok().ok_or(())
}

fn build_completion(prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "text-davinci-003",
        "prompt": prompt,
        "max_tokens": 500,
        "suffix": null,
        "n": 1,
    })
}

impl Handler {
    fn show_time<TZ: chrono::TimeZone>(ui: &str, source: &str, data: impl std::fmt::Display, start: chrono::DateTime<TZ>, end: chrono::DateTime<TZ>) {
        let diff = end - start;
        let diff_ns = diff.num_nanoseconds().unwrap_or(-1);
        if diff <= chrono::Duration::seconds(1) {
            let diff_human = diff.num_milliseconds();
            log::info!("TIMING ui={ui} {source}={data} duration={diff_ns}ns human={diff_human}ms");
        } else {
            let diff_human = "parsing_todo"; // TODO
            log::info!("TIMING ui={ui} {source}={data} duration={diff_ns}ns human={diff_human}");
        }
    }

    async fn chat(&self, user_id: UserId, user_name: &str, model: &str, prompt: &str) -> Result<String, Option<Cow<'static, str>>> {
        log::info!("COMMAND-PARSED model={model:?}, prompt={prompt:?}");

        let history = {
            let mut history = self.chat_histories.lock();
            Arc::clone(history.entry(user_id).or_default())
        };

        let client = build_openai_client().map_err(|e| {
            log::warn!("OpenAI client build failed. Error: {e:?}");
            None
        })?;

        const MAX_PROMPT_LEN: usize = 2000;

        let locked_history = history.lock().clone();
        let relevant_history_with_prompt = {
            let saved_history = locked_history.as_str();
            let history_and_prompt = format!("{saved_history}\n\nPrompt from {user_name}: {prompt}");
            let slice_prompt_start = if history_and_prompt.len() > MAX_PROMPT_LEN {
                history_and_prompt.len() - MAX_PROMPT_LEN
            } else {
                0
            };
            history_and_prompt[slice_prompt_start..].to_owned()
        };

        let request_body = build_completion(relevant_history_with_prompt.as_str());
        let response = match client.post("https://api.openai.com/v1/completions").json(&request_body).send().await {
            Ok(response) => response,
            Err(e) => {
                log::error!("Completion post failed due to {e:?}");
                return Err(None);
            },
        };

        let outcome: serde_json::Value = match response.json().await {
            Ok(value) => value,
            Err(e) => {
                log::error!("Completion post failed getting body due to {e:?}");
                return Err(None);
            },
        };

        log::info!("post replied with {outcome:?}");
        let choice_0_text = outcome
            .as_object().expect("an object")
            .get("choices").expect("choices to be present")
            .as_array().expect("an array")
            .get(0).expect("choice to be present")
            .as_object().expect("an object")
            .get("text").expect("text to be present")
            .as_str().expect("a string");

        history.lock().push_str(format!("\n\n{user_name}: {prompt}\n{model}: {choice_0_text}").as_str());

        Ok(choice_0_text.to_owned())
    }

    async fn clear(&self, user_id: UserId) -> Result<(), Option<Cow<'static, str>>> {
        self.chat_histories.lock().remove(&user_id);

        Ok(())
    }

    async fn handle_autocomp_and_errors(&self, ctx: Context, autocomplete: AutocompleteInteraction) {
        log::info!("BEGIN ui=discord_autocomp interaction={autocomplete:?}");
        let id = autocomplete.id;
        let res = autocomplete.create_autocomplete_response(&ctx, |response| {
            response.set_choices(serde_json::json!({}))
        }).await;
        match res {
            Ok(_) => {
                log::info!("COMPLETE ui=discord_autocomp interaction={id:?}")
            },
            Err(e) => {
                log::error!("COMPLETE ui=discord_autocomp interaction={id:?} error={e:?} user_error=false")
            },
        }
    }

    async fn handle_msgcomp_and_errors(&self, ctx: Context, msgcomponent: MessageComponentInteraction) {
        msgcomponent.defer(&ctx).await;
    }

    async fn handle_appcomm_and_errors(&self, ctx: Context, appcommand: ApplicationCommandInteraction) {
        log::info!("BEGIN ui=discord_appcomm interaction={appcommand:?}");
        let interaction_id = appcommand.id;
        match self.handle_appcomm(&ctx, &appcommand).await {
            Ok(_) => {
                log::info!("COMPLETE ui=discord_appcomm message={interaction_id:?} outcome=success");
            },
            Err(e0) => {
                let message = e0.as_ref().map(|e| e.as_ref()).unwrap_or("An error occurred");
                match appcommand.create_followup_message(ctx, |m| m.content(message)).await {
                    Ok(_) => {
                        log::error!("COMPLETE ui=discord_appcomm interaction={interaction_id:?} outcome=error error={e0:?} user_error=false");
                    },
                    Err(e1) => {
                        log::error!("COMPLETE ui=discord_appcomm interaction={interaction_id:?} outcome=error primary_error={e0:?} secondary_error={e1:?} user_error=false");
                    },
                }
            },
        }
    }

    async fn handle_appcomm(&self, ctx: &Context, appcommand: &ApplicationCommandInteraction) -> Result<(), Option<Cow<'static, str>>> {
        if let Err(e) = appcommand.defer(&ctx).await {
            log::error!("Application command failed to be deferred. Error: {e:?}");
            return Err(None);
        }

        if appcommand.data.name == "clear" {
            self.clear(appcommand.user.id).await?;
            appcommand.create_followup_message(ctx, |m| m.content("Chat history cleared.")).await.ok().ok_or(None)?;
            return Ok(());
        }

        if appcommand.data.name != "chat" {
            return Ok(());
        }

        let model = appcommand.data.options.iter().find(|o| o.name == "model").ok_or(None)?
            .value.as_ref().expect("model to be present")
            .as_str().expect("a str");
        let prompt = appcommand.data.options.iter().find(|o| o.name == "prompt").ok_or(None)?
            .value.as_ref().expect("prompt to be present")
            .as_str().expect("a str");

        let gpt_response = self.chat(appcommand.user.id, appcommand.user.name.as_str(), model, prompt).await?;

        let response_result = appcommand.create_followup_message(ctx, |m| {
            m
                .content(format!("{prompt}{gpt_response}"))
                .allowed_mentions(|allowed_mentions| allowed_mentions.empty_parse().replied_user(true))
        }).await;

        match response_result {
            Ok(_) => {
                Ok(())
            },
            Err(_) => {
                log::error!("Something went wrong sending the message...");
                Err(None)
            },
        }
    }

    async fn handle_message_and_errors(&self, ctx: Context, msg: Message) {
        log::info!("BEGIN ui=discord_classic message={msg:?}");
        let msg_id = msg.id;
        match self.handle_message(&ctx, &msg).await {
            Ok(_) => {
                log::info!("COMPLETE ui=discord_classic message={msg_id:?} outcome=success");
            },
            Err(e0) => {
                let message = e0.as_ref().map(|e| e.as_ref()).unwrap_or("An error occurred");
                match msg.reply(ctx, message).await {
                    Ok(_) => {
                        log::error!("COMPLETE ui=discord_classic message={msg_id:?} outcome=error error={e0:?} user_error=false");
                    },
                    Err(e1) => {
                        log::error!("COMPLETE ui=discord_classic message={msg_id:?} outcome=error primary_error={e0:?} secondary_error={e1:?} user_error=false");
                    },
                }
            },
        }
    }

    async fn handle_message(&self, ctx: &Context, msg: &Message) -> Result<(), Option<Cow<'static, str>>> {
        if msg.content.as_str() == "-clear" {
            self.clear(msg.author.id).await?;
            msg.reply(ctx, "Chat history cleared.").await.ok().ok_or(None)?;
            return Ok(());
        }

        if !msg.content.as_str().starts_with("-chat ") {
            return Ok(());
        }

        let mut pieces = msg.content.as_str().splitn(3, |c: char| c.is_whitespace());

        pieces.next();

        let Some(model) = pieces.next() else {
            log::warn!("Model should be present and be one of: `davinci`, `curie`, `babbage`, and `ada`. Found nothing.");
            return Err(Some("Model should be present and be one of: `davinci`, `curie`, `babbage`, and `ada`.".into()));
        };
        if ["davinci", "curie", "babbage", "ada"].iter().all(|s| &model != s) {
            log::warn!("Model should be one of: `davinci`, `curie`, `babbage`, and `ada`. Found `{model}`.");
            return Err(Some(format!("Model should be one of: `davinci`, `curie`, `babbage`, and `ada`. Found `{model}`.").into()));
        }
        if model != "davinci" {
            log::warn!("Only `davinci` works. Found `{model}`.");
            return Err(Some(format!("Only `davinci` works. Found `{model}`.").into()));
        }

        let Some(prompt) = pieces.next() else {
            log::warn!("A prompt is needed to give to the AI.");
            return Err(Some("A prompt is needed to give to the AI.".into()));
        };

        let in_progress_message = msg.reply(ctx, "Thinking...").await.ok();
        if in_progress_message.is_none() {
            log::error!("Failed to send in progress message. Continuing.");
        }

        let response = self.chat(msg.author.id, msg.author.name.as_str(), model, prompt).await?;

        if let Some(in_progress_message) = in_progress_message {
            if in_progress_message.delete(ctx).await.ok().is_none() {
                log::error!("Failed to delete in progress message. Continuing.");
            }
        }

        msg.channel_id.send_message(ctx, |msg_builder| {
            msg_builder
                .content(format!("{prompt}{response}"))
                .allowed_mentions(|allowed_mentions| allowed_mentions.empty_parse().replied_user(true))
                .reference_message(msg)
        }).await.ok().ok_or(None)?;

        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, data_about_bot: Ready) {
        // TODO
        log::info!("Setting up slash commands.");

        // TODO What should happen about errors?
        log::info!("Setting up global commands.");
        Command::set_global_application_commands(&ctx, |commands| commands
            .create_application_command(|command| {
                command
                    .name("chat")
                    .description("Chat with an AI model.")
                    .create_option(|option| {
                        option
                            .name("model")
                            .description("name of the model to user")
                            .kind(CommandOptionType::String)
                            .add_string_choice("Davinci", "davinci")
                            .set_autocomplete(false)
                            .required(true)
                    })
                    .create_option(|option| {
                        option
                            .name("prompt")
                            .description("Prompt to pass onto the model")
                            .kind(CommandOptionType::String)
                            .set_autocomplete(false)
                            .required(true)
                    })
            })
            .create_application_command(|command| {
                command.name("clear").description("Clear chat history")
            })
        ).await.unwrap();
    }

    async fn message(
        &self,
        ctx: Context,
        new_message: Message,
    ) {
        let ui = "discord_classic";
        let message_id = new_message.id;
        let start = chrono::Utc::now();

        self.handle_message_and_errors(ctx, new_message).await;

        let end = chrono::Utc::now();
        Self::show_time(ui, "message", message_id, start, end);
    }

    async fn interaction_create(
        &self,
        ctx: Context,
        interaction: Interaction,
    ) {
        let start = chrono::Utc::now();

        let (ui, interaction_id) = match interaction {
            Interaction::Ping(_) => {
                ("discord_ping", 0.into())
            }
            Interaction::ModalSubmit(submission) => {
                ("discord_modalsub", submission.id)
            }
            Interaction::Autocomplete(autocomplete) => {
                let id = autocomplete.id;
                self.handle_autocomp_and_errors(ctx, autocomplete).await;
                ("discord_autocomp", id)
            }
            Interaction::MessageComponent(component) => {
                let id = component.id;
                self.handle_msgcomp_and_errors(ctx, component).await;
                ("discord_msgcomp", id)
            }
            Interaction::ApplicationCommand(command) => {
                let id = command.id;
                self.handle_appcomm_and_errors(ctx, command).await;
                ("discord_appcomm", id)
            }
        };
        let end = chrono::Utc::now();
        Self::show_time(ui, "interaction", interaction_id, start, end);
    }
}

#[tokio::main]
async fn main() {
    setup_logging(LoggingCfg {
        level: "info".to_owned(),
        filter: None,
    });

    let mut client = build_client().await.expect("no error");
    client.start().await.expect("no error");
}
