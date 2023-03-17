mod models;
mod schema;

use std::{collections::VecDeque, env};

use anyhow::anyhow;
use diesel::{
    r2d2::{ConnectionManager, Pool},
    result::Error::NotFound,
    ExpressionMethods, QueryDsl, RunQueryDsl, SqliteConnection,
};
use dotenvy::dotenv;
use models::Guild;
use serenity::{
    async_trait,
    model::prelude::{Activity, Message, Ready},
    prelude::*,
};

use crate::models::NewGuild;

struct DB;

impl TypeMapKey for DB {
    type Value = Pool<ConnectionManager<SqliteConnection>>;
}

struct Handler;

struct CommandExecution<'a> {
    ctx: &'a Context,
    msg: &'a Message,
    guild: Guild,
    command: &'a str,
    args: VecDeque<&'a str>,
}

/// Function called whenever a **message-based command** is triggered.
async fn handle_command(data: CommandExecution<'_>) -> anyhow::Result<()> {
    let CommandExecution {
        ctx,
        msg,
        guild,
        command,
        mut args,
    } = data;

    let mut conn = ctx
        .data
        .read()
        .await
        .get::<DB>()
        .ok_or(anyhow!("DB was None"))?
        .get()?;

    let prefix = guild.prefix.unwrap_or("+".to_string());

    if command == "config" {
        if !msg.member(ctx).await?.permissions(ctx)?.manage_guild() {
            msg.reply(
                ctx,
                "You need the Manage Server permission to run this command!",
            )
            .await?;
            return Ok(());
        }

        let subcommand = match args.pop_front() {
            Some(subcommand) => subcommand,
            None => {
                msg.reply(
                    ctx,
                    format!(
                        "You need to specify a subcommand. Try `{}config help`",
                        prefix
                    ),
                )
                .await?;
                return Ok(());
            }
        }
        .to_lowercase();

        if subcommand == "help" {
            msg.reply(ctx, "Available subcommands: `prefix`, `help`")
                .await?;
        } else if subcommand == "prefix" {
            let action = match args.pop_front() {
                Some(action) => action,
                None => {
                    msg.reply(ctx, "Specify an action verb, `get` or `set`.")
                        .await?;
                    return Ok(());
                }
            }
            .to_lowercase();

            if action == "set" {
                if args.len() < 1 {
                    msg.reply(
                        ctx,
                        format!(
                            "You need to specify a prefix. Try `{}config prefix set <prefix>`",
                            prefix
                        ),
                    )
                    .await?;
                    return Ok(());
                }

                let new_prefix = args.make_contiguous().join(" ");

                diesel::update(schema::guilds::table)
                    .filter(
                        schema::guilds::id.eq(msg
                            .guild_id
                            .ok_or(anyhow!("msg.guild_id was None"))?
                            .to_string()),
                    )
                    .set(schema::guilds::prefix.eq(new_prefix.as_str()))
                    .execute(&mut conn)?;

                msg.reply(
                    ctx,
                    format!("This server's prefix has been set to `{}`.", new_prefix),
                )
                .await?;
            } else if action == "get" {
                msg.reply(ctx, format!("This server's prefix is set to `{}`.", prefix))
                    .await?;
            } else {
                msg.reply(
                    ctx,
                    format!(
                        "Unknown action verb. Try `{}config prefix get` or `{}config prefix set`.",
                        prefix, prefix
                    ),
                )
                .await?;
            }
        } else {
            msg.reply(
                ctx,
                format!("Unknown subcommand. Try `{}config help`", prefix),
            )
            .await?;
        }
    }

    Ok(())
}

/// Obtain a [Guild] instance
async fn obtain_guild(ctx: &Context, msg: &Message) -> anyhow::Result<Guild> {
    use schema::guilds::dsl::*;

    let mut conn = ctx
        .data
        .read()
        .await
        .get::<DB>()
        .ok_or(anyhow!("DB was None"))?
        .get()?;

    Ok(
        match guilds
            .filter(
                id.eq(msg
                    .guild_id
                    .ok_or(anyhow!("msg.guild_id was None"))?
                    .to_string()),
            )
            .first::<Guild>(&mut conn)
        {
            Ok(guild) => guild,
            Err(NotFound) => {
                let id_string = msg
                    .guild_id
                    .ok_or(anyhow!("msg.guild_id was None"))?
                    .to_string();
                let new_guild = NewGuild {
                    id: id_string.as_str(),
                    prefix: None,
                };

                diesel::insert_into(guilds)
                    .values(&new_guild)
                    .execute(&mut conn)?;

                new_guild.into()
            }
            Err(e) => return Err(e.into()),
        },
    )
}

/// Function called on every message.
async fn handle_message(ctx: &Context, msg: &Message) -> anyhow::Result<()> {
    if msg.author.bot {
        return Ok(());
    }

    if msg.channel(ctx).await?.guild().is_none() {
        msg.reply(ctx, "This bot only works in servers.").await?;
        return Ok(());
    }

    // Get this Guild from the database
    let guild = obtain_guild(ctx, msg).await?;

    let prefix = guild.prefix.clone().unwrap_or("+".to_string());

    // TODO: Guide the user if they mention the bot instead of a prefix

    if !msg.content.starts_with(&prefix) {
        return Ok(());
    }

    let mut args = msg.content[prefix.len()..]
        .split_whitespace()
        .collect::<VecDeque<_>>();

    let command = match args.pop_front() {
        Some(command) => command,
        None => return Ok(()),
    };

    println!(
        "Command {} run by {} ({}) with args \"{}\"",
        command,
        msg.author.tag(),
        msg.author.id,
        args.make_contiguous().join(" ")
    );

    handle_command(CommandExecution {
        ctx,
        msg,
        guild,
        command,
        args,
    })
    .await
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let result = handle_message(&ctx, &msg).await;

        if let Err(e) = result {
            if let Err(e2) = msg
                .reply(
                    ctx,
                    format!(
                        "An internal error occurred while processing your command: {}",
                        e
                    ),
                )
                .await
            {
                println!("An error occurred while handling an error. {:?}", e2);
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("Logged in as {}!", ready.user.tag());
        ctx.set_activity(Activity::watching("for custom mentions"))
            .await;
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenv()?;

    let database_url = env::var("DATABASE_URL").expect("Expected DATABASE_URL in the environment");
    let pool = Pool::builder()
        .test_on_check_out(true)
        .build(ConnectionManager::<SqliteConnection>::new(database_url))?;

    let intents = GatewayIntents::all();

    let mut client = Client::builder(
        env::var("TOKEN").expect("Expected a token in the environment"),
        intents,
    )
    .event_handler(Handler)
    .await?;

    {
        let mut data = client.data.write().await;
        data.insert::<DB>(pool);
    }

    client.start().await?;

    Ok(())
}
