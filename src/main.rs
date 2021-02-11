#[macro_use]
extern crate diesel;

#[macro_use]
extern crate diesel_migrations;

#[macro_use]
extern crate log;

mod api;
mod ban;
mod command_history;
mod commands;
mod crates;
mod db;
mod godbolt;
mod jobs;
mod playground;
mod schema;
mod text;
mod welcome;

use crate::db::DB;
use commands::{Args, Commands, GuardFn};
use diesel::prelude::*;
use indexmap::IndexMap;
use serde::Deserialize;
use serenity::{model::prelude::*, prelude::*};

pub enum Error {
    MissingCodeblock,
    NoCratesFound,
    MissingPermissions,
    EvalWithFnMain,
    Unknown(Box<dyn std::error::Error + Send + Sync>),
}

impl<T> From<T> for Error
where
    Box<dyn std::error::Error + Send + Sync>: From<T>,
{
    fn from(error: T) -> Self {
        Self::Unknown(error.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCodeblock => write!(
                f,
                "Missing code block. Please use the following markdown:
\\`code here\\`
or
\\`\\`\\`rust
code here
\\`\\`\\`",
            ),
            Self::NoCratesFound => write!(f, "No crates found"),
            Self::MissingPermissions => write!(f, "You are not allowed to use this command"),
            Self::EvalWithFnMain => write!(f, "Code passed to ?eval should not contain `fn main`"),
            Self::Unknown(error) => write!(f, "Unexpected error ({})", error),
        }
    }
}

#[derive(Deserialize)]
struct Config {
    crates: bool,
    eval: bool,
    discord_token: String,
    mod_id: String,
    talk_id: String,
}

fn init_data(config: &Config) -> Result<(), Error> {
    use crate::schema::roles;
    info!("Loading data into database");

    let conn = DB.get()?;

    let upsert_role = |name: &str, role_id: &str| -> Result<(), Error> {
        diesel::insert_into(roles::table)
            .values((roles::role.eq(role_id), roles::name.eq(name)))
            .on_conflict(roles::name)
            .do_update()
            .set(roles::role.eq(role_id))
            .execute(&conn)?;

        Ok(())
    };

    let _ = conn
        .build_transaction()
        .read_write()
        .run::<_, Error, _>(|| {
            upsert_role("mod", &config.mod_id)?;
            upsert_role("talk", &config.talk_id)?;

            Ok(())
        })?;

    Ok(())
}

fn app() -> Result<(), Error> {
    let config = envy::from_env::<Config>()?;

    info!("starting...");

    let _ = db::run_migrations()?;

    let _ = init_data(&config)?;

    let mut cmds = Commands::new();

    if config.crates {
        // crates.io
        cmds.add("crate", crates::search);
        cmds.help("crate", "Lookup crates on crates.io", crates::help);

        // docs.rs
        cmds.add("docs", crates::doc_search);
        cmds.help("docs", "Lookup documentation", crates::doc_help);
    }

    if config.eval {
        // rust playground
        cmds.add("play", playground::play);
        cmds.help(
            "play",
            "Compile and run rust code in a playground",
            |args| playground::play_and_eval_help(args, "play"),
        );

        cmds.add("eval", playground::eval);
        cmds.help("eval", "Evaluate a single rust expression", |args| {
            playground::play_and_eval_help(args, "eval")
        });

        cmds.add("miri", playground::miri);
        cmds.help(
            "miri",
            "Run code and detect undefined behavior using Miri",
            playground::miri_help,
        );

        cmds.add("expand", playground::expand_macros);
        cmds.help(
            "expand",
            "Expand macros to their raw desugared form",
            playground::expand_macros_help,
        );

        cmds.add("clippy", playground::clippy);
        cmds.help(
            "clippy",
            "Catch common mistakes using the Clippy linter",
            playground::clippy_help,
        );

        cmds.add("fmt", playground::fmt);
        cmds.help("fmt", "Format code using rustfmt", playground::fmt_help);
    }

    cmds.add("go", |args| api::send_reply(&args, "No"));
    cmds.help("go", "Evaluates Go code", |args| {
        api::send_reply(&args, "Evaluates Go code")
    });

    cmds.add("godbolt", godbolt::godbolt);
    cmds.help("godbolt", "View assembly using Godbolt", godbolt::help);

    cmds.add("cleanup", command_history::cleanup);
    cmds.help(
        "cleanup",
        "Deletes the bot's messages for cleanup",
        command_history::cleanup_help,
    );

    cmds.add("source", |args| {
        api::send_reply(args, "https://github.com/kangalioo/discord-mods-bot")
    });
    cmds.help("source", "Links to the bot GitHub repo", |args| {
        api::send_reply(args, "?source\n\nLinks to the bot GitHub repo")
    });

    // Slow mode.
    // 0 seconds disables slowmode
    cmds.add_protected("slowmode", api::slow_mode, api::is_mod);
    cmds.help_protected(
        "slowmode",
        "Set slowmode on a channel",
        api::slow_mode_help,
        api::is_mod,
    );

    // Kick
    cmds.add_protected("kick", api::kick, api::is_mod);
    cmds.help_protected(
        "kick",
        "Kick a user from the guild",
        api::kick_help,
        api::is_mod,
    );

    // Ban
    cmds.add_protected("ban", ban::temp_ban, api::is_mod);
    cmds.help_protected(
        "ban",
        "Temporarily ban a user from the guild",
        ban::help,
        api::is_mod,
    );

    // Post the welcome message to the welcome channel.
    cmds.add_protected("CoC", welcome::post_message, api::is_mod);
    cmds.help_protected(
        "CoC",
        "Post the code of conduct message to a channel",
        welcome::help,
        api::is_mod,
    );

    let menu = cmds.take_menu().unwrap();
    cmds.add("help", move |args| {
        if args.body.is_empty() {
            api::send_reply(&args, &format!("```{}```", &main_menu(&args, &menu)))?;
        }
        Ok(())
    });

    let mut client = Client::new_with_extras(&config.discord_token, |e| {
        e.event_handler(Events { cmds });
        e
    })?;

    client.start()?;

    Ok(())
}

/// Send a Discord reply message and truncate the message with a given truncation message if the
/// text is too long. "Too long" means, it either goes beyond Discord's 2000 char message limit,
/// or if the text_body has too many lines.
///
/// Only `text_body` is truncated. `text_end` will always be appended at the end. This is useful
/// for example for large code blocks. You will want to truncate the code block contents, but the
/// finalizing \`\`\` should always stay - that's what `text_end` is for.
///
/// ```rust,no_run
/// # let args = todo!();
/// // This will send "```\nvery long stringvery long stringver...long stringve\n```"
/// //                Character limit reached, text_end starts ~~~~~~~~~~~~~~~~^
/// reply_potentially_long_text(
///     args,
///     format!("```\n{}", "very long string".repeat(500)),
///     "\n```"
/// )
/// ```
fn reply_potentially_long_text(
    args: &Args,
    text_body: &str,
    text_end: &str,
    truncation_msg: &str,
) -> Result<(), Error> {
    const MAX_OUTPUT_LINES: usize = 45;

    // check the 2000 char limit first, because otherwise we could produce a too large message
    let msg = if text_body.len() + text_end.len() > 2000 {
        // This is how long the text body may be at max to conform to Discord's limit
        let available_space = 2000 - text_end.len() - truncation_msg.len();

        let mut cut_off_point = available_space;
        while !text_body.is_char_boundary(cut_off_point) {
            cut_off_point -= 1;
        }

        format!(
            "{}{}{}",
            &text_body[..cut_off_point],
            text_end,
            truncation_msg
        )
    } else if text_body.lines().count() > MAX_OUTPUT_LINES {
        format!(
            "{}{}{}",
            text_body
                .lines()
                .take(MAX_OUTPUT_LINES)
                .collect::<Vec<_>>()
                .join("\n"),
            text_end,
            truncation_msg,
        )
    } else {
        format!("{}{}", text_body, text_end)
    };

    api::send_reply(args, &msg)
}

/// Extract code from a Discord code block on a best-effort basis
///
/// ```rust
/// assert_eq!(extract_code("`hello`"), Some("hello"));
/// assert_eq!(extract_code("`    hello `"), Some("hello"));
/// assert_eq!(extract_code("``` hello ```"), Some("hello"));
/// assert_eq!(extract_code("```rust hello ```"), Some("hello"));
/// assert_eq!(extract_code("```rust\nhello\n```"), Some("hello"));
/// assert_eq!(extract_code("``` rust\nhello\n```"), Some("rust\nhello"));
/// ```
pub fn extract_code(input: &str) -> Result<&str, Error> {
    fn inner(input: &str) -> Option<&str> {
        let input = input.trim();

        println!("{:?}", input);
        let extracted_code = if input.starts_with("```") && input.ends_with("```") {
            let code_starting_point = input.find(char::is_whitespace)?; // skip over lang specifier
            let code_end_point = input.len() - 3;

            // can't fail but you can never be too sure
            input.get(code_starting_point..code_end_point)?
        } else if input.starts_with('`') && input.ends_with('`') {
            // can't fail but you can never be too sure
            input.get(1..(input.len() - 1))?
        } else {
            return None;
        };

        Some(extracted_code.trim())
    }
    inner(input).ok_or(Error::MissingCodeblock)
}

fn main_menu(args: &Args, commands: &IndexMap<&str, (&str, GuardFn)>) -> String {
    let mut menu = "Commands:\n".to_owned();
    for (base_cmd, (description, guard)) in commands {
        if let Ok(true) = (guard)(&args) {
            menu += &format!("\t?{cmd:<12}{desc}\n", cmd = base_cmd, desc = description);
        }
    }

    menu += &format!("\t?{cmd:<12}This menu\n", cmd = "help");
    menu += "\nType ?help command for more info on a command.";
    menu += "\n\nAdditional Info:\n";
    menu += "\tYou can edit your message to the bot and the bot will edit its response.";
    menu
}

fn main() {
    env_logger::init();

    if let Err(e) = app() {
        error!("{}", e);
        std::process::exit(1);
    }
}

struct BotUserId;

impl TypeMapKey for BotUserId {
    type Value = UserId;
}

struct Events {
    cmds: Commands,
}

impl EventHandler for Events {
    fn ready(&self, cx: Context, ready: Ready) {
        info!("{} connected to discord", ready.user.name);
        {
            let mut data = cx.data.write();
            data.insert::<command_history::CommandHistory>(IndexMap::new());
            data.insert::<BotUserId>(ready.user.id);
        }

        jobs::start_jobs(cx);
    }

    fn message(&self, cx: Context, message: Message) {
        self.cmds.execute(&cx, &message);
    }

    fn message_update(
        &self,
        cx: Context,
        _: Option<Message>,
        _: Option<Message>,
        ev: MessageUpdateEvent,
    ) {
        if let Err(e) = command_history::replay_message(cx, ev, &self.cmds) {
            error!("{}", e);
        }
    }

    fn message_delete(&self, cx: Context, channel_id: ChannelId, message_id: MessageId) {
        let mut data = cx.data.write();
        let history = data.get_mut::<command_history::CommandHistory>().unwrap();
        if let Some(response_id) = history.remove(&message_id) {
            info!("deleting message: {:?}", response_id);
            let _ = channel_id.delete_message(&cx, response_id);
        }
    }

    fn reaction_add(&self, cx: Context, reaction: Reaction) {
        if let Err(e) = welcome::assign_talk_role(&cx, &reaction) {
            error!("{}", e);
        }
    }

    fn guild_ban_removal(&self, _cx: Context, guild_id: GuildId, user: User) {
        if let Err(e) = ban::save_unban(format!("{}", user.id), format!("{}", guild_id)) {
            error!("{}", e);
        }
    }
}
